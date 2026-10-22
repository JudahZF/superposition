//! A Windows plug-in worker under Wine, attached to a Linux host's shared bank.
//!
//! The worker never creates a bank. It opens the host's `/dev/shm` object through Wine's `Z:`
//! drive; Wine backs the view with a `MAP_SHARED` mapping of the same pages. Windows has no
//! cross-process address wait and Wine traps raw Linux syscalls from PE code, so the futex and
//! the host's `CLOCK_MONOTONIC` come from the `sp_wine_bridge` unixlib (`wine/` in this crate).

use std::{
    ffi::{CStr, c_void},
    io,
    os::raw::c_int,
    ptr::{self, NonNull},
    sync::{OnceLock, atomic::AtomicU32},
};

use windows_sys::Win32::{
    Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE},
    Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileSizeEx,
        OPEN_EXISTING,
    },
    System::{
        LibraryLoader::{GetProcAddress, LoadLibraryW},
        Memory::{
            CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
            PAGE_READWRITE, UnmapViewOfFile,
        },
        SystemInformation::{GetSystemInfo, SYSTEM_INFO},
    },
};

type Export = unsafe extern "system" fn() -> isize;
type ClockFn = unsafe extern "C" fn() -> u64;
type WakeFn = unsafe extern "C" fn(*mut c_void) -> c_int;
type WaitFn = unsafe extern "C" fn(*mut c_void, u32, u64) -> c_int;
type SchedulerFn = unsafe extern "C" fn(c_int, c_int) -> c_int;

/// Entry points exported by `sp_wine_bridge.dll`.
pub(crate) struct Bridge {
    clock_ns: ClockFn,
    futex_wake: WakeFn,
    futex_wait: WaitFn,
    pub(crate) set_scheduler: SchedulerFn,
}

/// The loaded bridge, or `None` outside Wine or when the bridge is not installed.
pub(crate) fn bridge() -> Option<&'static Bridge> {
    static BRIDGE: OnceLock<Option<Bridge>> = OnceLock::new();
    BRIDGE.get_or_init(load_bridge).as_ref()
}

fn load_bridge() -> Option<Bridge> {
    let name = wide("sp_wine_bridge.dll");
    // SAFETY: `name` is a NUL-terminated UTF-16 string.
    let module = unsafe { LoadLibraryW(name.as_ptr()) };
    if module.is_null() {
        return None;
    }
    let symbol = |name: &CStr| {
        // SAFETY: `module` is loaded and never freed; `name` is NUL-terminated.
        unsafe { GetProcAddress(module, name.as_ptr().cast()) }
    };
    // SAFETY: each export has the C signature declared in `wine/sp_wine_bridge.h`.
    unsafe {
        Some(Bridge {
            clock_ns: std::mem::transmute::<Export, ClockFn>(symbol(c"sp_wine_clock_ns")?),
            futex_wake: std::mem::transmute::<Export, WakeFn>(symbol(c"sp_wine_futex_wake")?),
            futex_wait: std::mem::transmute::<Export, WaitFn>(symbol(c"sp_wine_futex_wait")?),
            set_scheduler: std::mem::transmute::<Export, SchedulerFn>(symbol(
                c"sp_wine_set_scheduler",
            )?),
        })
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

/// An open file and its file-mapping object, closed on drop.
pub(crate) struct RawObject {
    file: HANDLE,
    mapping: HANDLE,
}

impl Drop for RawObject {
    fn drop(&mut self) {
        // SAFETY: this value is the only owner of both handles.
        unsafe {
            CloseHandle(self.mapping);
            CloseHandle(self.file);
        }
    }
}

pub(crate) fn create_exclusive(_name: &CStr) -> io::Result<RawObject> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "a Wine worker opens the Linux host's bank; it never creates one",
    ))
}

/// Maps a POSIX name such as `/sp-1-2-3` to the host's tmpfs file through Wine's `Z:` drive.
pub(crate) fn host_path(name: &CStr) -> io::Result<String> {
    let name = name
        .to_str()
        .ok()
        .and_then(|name| name.strip_prefix('/'))
        .filter(|name| !name.is_empty() && !name.contains(['/', '\\']))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid POSIX bank name"))?;
    Ok(format!("Z:\\dev\\shm\\{name}"))
}

pub(crate) fn open_existing(name: &CStr) -> io::Result<RawObject> {
    let path = wide(&host_path(name)?);
    // SAFETY: `path` is NUL-terminated; the other arguments are documented constants.
    let file = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };
    if file == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `file` is an open read/write handle; a zero size maps the whole file.
    let mapping =
        unsafe { CreateFileMappingW(file, ptr::null(), PAGE_READWRITE, 0, 0, ptr::null()) };
    if mapping.is_null() {
        let error = io::Error::last_os_error();
        // SAFETY: `file` is owned here and closed once.
        unsafe { CloseHandle(file) };
        return Err(error);
    }
    Ok(RawObject { file, mapping })
}

pub(crate) fn set_len(_object: &RawObject, _length: i64) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "a Wine worker cannot resize the host's bank",
    ))
}

pub(crate) fn len(object: &RawObject) -> io::Result<i64> {
    let mut length = 0_i64;
    // SAFETY: `file` is open and `length` is valid writable storage.
    if unsafe { GetFileSizeEx(object.file, &raw mut length) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(length)
}

pub(crate) fn map(object: &RawObject, length: usize) -> io::Result<NonNull<c_void>> {
    // SAFETY: `mapping` is a live read/write file mapping at least `length` bytes long.
    let view = unsafe { MapViewOfFile(object.mapping, FILE_MAP_ALL_ACCESS, 0, 0, length) };
    NonNull::new(view.Value).ok_or_else(io::Error::last_os_error)
}

/// # Safety
///
/// `address` must be a live view returned by [`map`].
pub(crate) unsafe fn unmap(address: NonNull<c_void>, _length: usize) {
    // SAFETY: upheld by the caller.
    unsafe {
        UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
            Value: address.as_ptr(),
        })
    };
}

pub(crate) fn unlink(_name: &CStr) {}

pub(crate) fn page_size() -> i64 {
    let mut info = std::mem::MaybeUninit::<SYSTEM_INFO>::uninit();
    // SAFETY: `GetSystemInfo` always initializes the record.
    unsafe { GetSystemInfo(info.as_mut_ptr()) };
    // SAFETY: initialized above.
    i64::from(unsafe { info.assume_init() }.dwPageSize)
}

pub(crate) fn clock_now() -> u64 {
    // Without the bridge no bank can be opened, so this clock is never compared with a host.
    // SAFETY: the bridge export takes no arguments.
    bridge().map_or(0, |bridge| unsafe { (bridge.clock_ns)() })
}

pub(crate) fn clock_timebase() -> io::Result<(u32, u32)> {
    if bridge().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "sp_wine_bridge.dll is not available; Windows workers run only under Wine",
        ));
    }
    Ok((1, 1))
}

pub(crate) fn wake_supported() -> bool {
    bridge().is_some()
}

/// Wakes one waiter on a shared four-byte word. Returns a negative errno on failure.
///
/// # Safety
///
/// `word` must point into a live view of the host's bank.
pub(crate) unsafe fn notify(word: &AtomicU32) -> c_int {
    match bridge() {
        // SAFETY: the bridge issues one FUTEX_WAKE on the shared address.
        Some(bridge) => unsafe { (bridge.futex_wake)(ptr::from_ref(word).cast_mut().cast()) },
        None => -38, // ENOSYS
    }
}

/// Waits while `word` equals `observed`. Returns zero on wake, timeout, or interruption.
///
/// # Safety
///
/// `word` must point into a live view of the host's bank.
pub(crate) unsafe fn wait(word: &AtomicU32, observed: u32, timeout_ns: u64) -> c_int {
    match bridge() {
        // SAFETY: the bridge issues one bounded FUTEX_WAIT on the shared address.
        Some(bridge) => unsafe {
            (bridge.futex_wait)(ptr::from_ref(word).cast_mut().cast(), observed, timeout_ns)
        },
        None => 38, // ENOSYS
    }
}
