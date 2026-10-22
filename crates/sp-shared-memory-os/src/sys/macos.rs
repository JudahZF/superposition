//! Darwin POSIX shared memory, the continuous clock, and `os_sync` address waits.

use std::{
    ffi::CStr,
    io,
    mem::MaybeUninit,
    os::raw::{c_char, c_int, c_uint, c_void},
    ptr::{self, NonNull},
    sync::atomic::AtomicU32,
};

const O_RDWR: c_int = 0x0002;
const O_CREAT: c_int = 0x0200;
const O_EXCL: c_int = 0x0800;
const SHARED_MEMORY_MODE: c_uint = 0o666;
const PROT_READ: c_int = 0x01;
const PROT_WRITE: c_int = 0x02;
const MAP_SHARED: c_int = 0x0001;
const MAP_FAILED: *mut c_void = (-1_isize) as *mut c_void;

#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    fn close(file_descriptor: c_int) -> c_int;
    fn ftruncate(file_descriptor: c_int, length: i64) -> c_int;
    fn getpagesize() -> c_int;
    fn mmap(
        address: *mut c_void,
        length: usize,
        protection: c_int,
        flags: c_int,
        file_descriptor: c_int,
        offset: i64,
    ) -> *mut c_void;
    fn munmap(address: *mut c_void, length: usize) -> c_int;
    fn mach_continuous_time() -> u64;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> c_int;
    fn superposition_shm_open_create(name: *const c_char, flags: c_int, mode: c_uint) -> c_int;
    fn superposition_shm_length(file_descriptor: c_int, length: *mut i64) -> c_int;
    fn superposition_request_wake_supported() -> c_int;
    fn superposition_notify_request(address: *mut c_void) -> c_int;
    fn superposition_wait_for_request(
        address: *mut c_void,
        observed: u32,
        timeout_ns: u64,
    ) -> c_int;
    fn shm_open(name: *const c_char, flags: c_int) -> c_int;
    fn shm_unlink(name: *const c_char) -> c_int;
}

/// An open POSIX shared-memory descriptor, closed on drop.
pub(crate) struct RawObject(c_int);

impl Drop for RawObject {
    fn drop(&mut self) {
        // SAFETY: this value is the descriptor's only owner.
        unsafe { close(self.0) };
    }
}

pub(crate) fn create_exclusive(name: &CStr) -> io::Result<RawObject> {
    // SAFETY: `name` is NUL-terminated, and all flags/mode bits are Darwin POSIX constants.
    let descriptor = unsafe {
        superposition_shm_open_create(name.as_ptr(), O_RDWR | O_CREAT | O_EXCL, SHARED_MEMORY_MODE)
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(RawObject(descriptor))
}

pub(crate) fn open_existing(name: &CStr) -> io::Result<RawObject> {
    // SAFETY: `name` is NUL-terminated and `O_RDWR` is a Darwin POSIX constant.
    let descriptor = unsafe { shm_open(name.as_ptr(), O_RDWR) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(RawObject(descriptor))
}

pub(crate) fn set_len(object: &RawObject, length: i64) -> io::Result<()> {
    // SAFETY: the descriptor is open; `length` is checked by the caller.
    if unsafe { ftruncate(object.0, length) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn len(object: &RawObject) -> io::Result<i64> {
    let mut length = 0_i64;
    // SAFETY: the descriptor is open and `length` is valid writable storage for the shim's
    // `int64_t` result. The shim calls `fstat` and writes only on success.
    if unsafe { superposition_shm_length(object.0, &raw mut length) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(length)
}

pub(crate) fn map(object: &RawObject, length: usize) -> io::Result<NonNull<c_void>> {
    // SAFETY: the descriptor is open and the caller checked its backing length. POSIX returns
    // either `MAP_FAILED` or a page-aligned valid mapping.
    let address = unsafe {
        mmap(
            ptr::null_mut(),
            length,
            PROT_READ | PROT_WRITE,
            MAP_SHARED,
            object.0,
            0,
        )
    };
    if address == MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    NonNull::new(address).ok_or_else(|| {
        // SAFETY: a non-failed mapping of `length` bytes; release it before failing.
        unsafe { munmap(address, length) };
        io::Error::other("mmap returned null")
    })
}

/// # Safety
///
/// `address` and `length` must describe one live mapping returned by [`map`].
pub(crate) unsafe fn unmap(address: NonNull<c_void>, length: usize) {
    // SAFETY: upheld by the caller.
    unsafe { munmap(address.as_ptr(), length) };
}

pub(crate) fn unlink(name: &CStr) {
    // SAFETY: `name` is NUL-terminated. Callers unlink only names they created.
    unsafe { shm_unlink(name.as_ptr()) };
}

pub(crate) fn page_size() -> i64 {
    // SAFETY: `getpagesize` takes no arguments and returns the VM page size.
    i64::from(unsafe { getpagesize() })
}

pub(crate) fn clock_now() -> u64 {
    // SAFETY: `mach_continuous_time` takes no arguments and has no preconditions.
    unsafe { mach_continuous_time() }
}

pub(crate) fn clock_timebase() -> io::Result<(u32, u32)> {
    let mut info = MaybeUninit::<MachTimebaseInfo>::uninit();
    // SAFETY: Darwin writes one `MachTimebaseInfo` to the valid out pointer.
    let status = unsafe { mach_timebase_info(info.as_mut_ptr()) };
    if status != 0 {
        return Err(io::Error::other(format!(
            "mach_timebase_info failed with status {status}"
        )));
    }
    // SAFETY: a zero status documents that Darwin initialized the output record.
    let info = unsafe { info.assume_init() };
    Ok((info.numer, info.denom))
}

pub(crate) fn wake_supported() -> bool {
    // SAFETY: the shim has no arguments or side effects and checks OS availability.
    unsafe { superposition_request_wake_supported() != 0 }
}

/// Wakes one waiter on a shared four-byte word. Returns a negative errno on failure.
///
/// # Safety
///
/// `word` must point into a live `MAP_SHARED` mapping.
pub(crate) unsafe fn notify(word: &AtomicU32) -> c_int {
    // SAFETY: the shim never stores through the pointer and makes one shared-address wake call.
    unsafe { superposition_notify_request(ptr::from_ref(word).cast_mut().cast()) }
}

/// Waits while `word` equals `observed`. Returns zero on wake, timeout, or interruption.
///
/// # Safety
///
/// `word` must point into a live `MAP_SHARED` mapping.
pub(crate) unsafe fn wait(word: &AtomicU32, observed: u32, timeout_ns: u64) -> c_int {
    // SAFETY: the shim compares the word without storing and waits at most `timeout_ns`.
    unsafe {
        superposition_wait_for_request(ptr::from_ref(word).cast_mut().cast(), observed, timeout_ns)
    }
}
