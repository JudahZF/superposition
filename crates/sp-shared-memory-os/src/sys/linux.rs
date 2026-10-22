//! Linux POSIX shared memory in `/dev/shm`, `CLOCK_MONOTONIC`, and shared futexes.
//!
//! The futex word is process-shared (no `FUTEX_PRIVATE_FLAG`), so a worker under Wine can wait
//! on the same word through `sp_wine_bridge`. Ticks are nanoseconds, the same domain the
//! bridge reports to a Wine worker.

use std::{
    ffi::CStr,
    io,
    os::raw::{c_int, c_void},
    ptr::{self, NonNull},
    sync::atomic::AtomicU32,
};

const SHARED_MEMORY_MODE: libc::mode_t = 0o600;

/// An open POSIX shared-memory descriptor, closed on drop.
pub(crate) struct RawObject(c_int);

impl Drop for RawObject {
    fn drop(&mut self) {
        // SAFETY: this value is the descriptor's only owner.
        unsafe { libc::close(self.0) };
    }
}

pub(crate) fn create_exclusive(name: &CStr) -> io::Result<RawObject> {
    // SAFETY: `name` is NUL-terminated; flags and mode are libc constants.
    let descriptor = unsafe {
        libc::shm_open(
            name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC,
            SHARED_MEMORY_MODE,
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(RawObject(descriptor))
}

pub(crate) fn open_existing(name: &CStr) -> io::Result<RawObject> {
    // SAFETY: `name` is NUL-terminated; flags are libc constants.
    let descriptor = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC, 0) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(RawObject(descriptor))
}

pub(crate) fn set_len(object: &RawObject, length: i64) -> io::Result<()> {
    // SAFETY: the descriptor is open; `length` is checked by the caller.
    if unsafe { libc::ftruncate(object.0, length) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn len(object: &RawObject) -> io::Result<i64> {
    let mut status = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: the descriptor is open and `status` is valid writable storage.
    if unsafe { libc::fstat(object.0, status.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fstat` succeeded and initialized the record.
    Ok(unsafe { status.assume_init() }.st_size)
}

pub(crate) fn map(object: &RawObject, length: usize) -> io::Result<NonNull<c_void>> {
    // SAFETY: the descriptor is open and the caller checked its backing length.
    let address = unsafe {
        libc::mmap(
            ptr::null_mut(),
            length,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            object.0,
            0,
        )
    };
    if address == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    NonNull::new(address).ok_or_else(|| {
        // SAFETY: a non-failed mapping of `length` bytes; release it before failing.
        unsafe { libc::munmap(address, length) };
        io::Error::other("mmap returned null")
    })
}

/// # Safety
///
/// `address` and `length` must describe one live mapping returned by [`map`].
pub(crate) unsafe fn unmap(address: NonNull<c_void>, length: usize) {
    // SAFETY: upheld by the caller.
    unsafe { libc::munmap(address.as_ptr(), length) };
}

pub(crate) fn unlink(name: &CStr) {
    // SAFETY: `name` is NUL-terminated. Callers unlink only names they created.
    unsafe { libc::shm_unlink(name.as_ptr()) };
}

pub(crate) fn page_size() -> i64 {
    // SAFETY: `sysconf` has no preconditions.
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) }
}

pub(crate) fn clock_now() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `now` is valid writable storage; CLOCK_MONOTONIC is always available (vDSO).
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut now) };
    u64::try_from(now.tv_sec).unwrap_or_default() * 1_000_000_000
        + u64::try_from(now.tv_nsec).unwrap_or_default()
}

#[allow(
    clippy::unnecessary_wraps,
    reason = "every backend shares this signature; Darwin's timebase query can fail"
)]
pub(crate) fn clock_timebase() -> io::Result<(u32, u32)> {
    Ok((1, 1))
}

pub(crate) fn wake_supported() -> bool {
    true
}

/// Wakes one waiter on a shared four-byte word. Returns a negative errno on failure.
///
/// # Safety
///
/// `word` must point into a live `MAP_SHARED` mapping.
pub(crate) unsafe fn notify(word: &AtomicU32) -> c_int {
    // SAFETY: FUTEX_WAKE only reads the address; the word is four-byte aligned.
    let woken = unsafe {
        libc::syscall(
            libc::SYS_futex,
            ptr::from_ref(word),
            libc::FUTEX_WAKE,
            1,
            ptr::null::<libc::timespec>(),
            ptr::null::<u32>(),
            0,
        )
    };
    if woken < 0 {
        -io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO)
    } else {
        c_int::try_from(woken).unwrap_or(c_int::MAX)
    }
}

/// Waits while `word` equals `observed`. Returns zero on wake, timeout, or interruption.
///
/// # Safety
///
/// `word` must point into a live `MAP_SHARED` mapping.
pub(crate) unsafe fn wait(word: &AtomicU32, observed: u32, timeout_ns: u64) -> c_int {
    let timeout = libc::timespec {
        tv_sec: libc::time_t::try_from(timeout_ns / 1_000_000_000).unwrap_or(libc::time_t::MAX),
        tv_nsec: libc::c_long::try_from(timeout_ns % 1_000_000_000).unwrap_or_default(),
    };
    // SAFETY: FUTEX_WAIT compares the aligned word and sleeps at most `timeout`.
    let status = unsafe {
        libc::syscall(
            libc::SYS_futex,
            ptr::from_ref(word),
            libc::FUTEX_WAIT,
            observed,
            &raw const timeout,
            ptr::null::<u32>(),
            0,
        )
    };
    if status == 0 {
        return 0;
    }
    match io::Error::last_os_error().raw_os_error() {
        Some(libc::EAGAIN | libc::ETIMEDOUT | libc::EINTR) => 0,
        Some(code) => code,
        None => libc::EIO,
    }
}
