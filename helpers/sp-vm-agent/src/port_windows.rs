//! A virtio-serial port on Windows, opened for overlapped I/O, and a worker's process mask.
//!
//! On a handle opened without `FILE_FLAG_OVERLAPPED`, Windows runs one I/O at a time per file
//! object: a read that waits for the host blocks every write on the port. The agent reads and
//! writes a port from different threads, so each call here is overlapped and then waited for.
//! A clone shares the handle; each call has its own event.

#![allow(unsafe_code, reason = "Win32 overlapped file I/O")]

use std::{
    io::{self, Read, Write},
    mem, ptr,
    sync::Arc,
};

use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    },
    Storage::FileSystem::{CreateFileW, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, ReadFile, WriteFile},
    System::{
        IO::GetOverlappedResult,
        IO::OVERLAPPED,
        Threading::{CreateEventW, SetProcessAffinityMask},
    },
};

use crate::PortIo;

struct Handle(HANDLE);

// SAFETY: a file handle may be used from any thread; overlapped calls do not share state.
unsafe impl Send for Handle {}
// SAFETY: as above.
unsafe impl Sync for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the last clone owns the handle and closes it once.
        unsafe { CloseHandle(self.0) };
    }
}

/// An open port. Clones share it.
pub struct OverlappedPort(Arc<Handle>);

impl OverlappedPort {
    /// Opens `path`, such as `\\.\Global\lambda.agent`.
    ///
    /// # Errors
    ///
    /// Returns the Windows error of `CreateFileW`.
    pub fn open(path: &str) -> io::Result<Self> {
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is NUL-terminated; the other arguments are documented constants.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(Arc::new(Handle(handle))))
    }

    /// One overlapped read (`read`) or write into or from `buffer`, waited for.
    fn transfer(&self, buffer: *mut u8, length: usize, read: bool) -> io::Result<usize> {
        let length = u32::try_from(length).unwrap_or(u32::MAX);
        // SAFETY: an unnamed manual-reset event, not signalled.
        let event = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: OVERLAPPED is plain data; zero is its documented initial state.
        let mut overlapped: OVERLAPPED = unsafe { mem::zeroed() };
        overlapped.hEvent = event;
        let handle = self.0.0;
        // SAFETY: `buffer` has `length` bytes for the whole call: this function waits for the
        // I/O to finish before it returns, so `overlapped` and `buffer` outlive it.
        let result = unsafe {
            let started = if read {
                ReadFile(handle, buffer, length, ptr::null_mut(), &raw mut overlapped)
            } else {
                WriteFile(handle, buffer, length, ptr::null_mut(), &raw mut overlapped)
            };
            let error = io::Error::last_os_error();
            if started == 0 && error.raw_os_error() != Some(ERROR_IO_PENDING.cast_signed()) {
                Err(error)
            } else {
                let mut transferred = 0_u32;
                if GetOverlappedResult(handle, &raw const overlapped, &raw mut transferred, 1) == 0
                {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(transferred as usize)
                }
            }
        };
        // SAFETY: the event is owned here and no I/O uses it any more.
        unsafe { CloseHandle(event) };
        result
    }
}

impl Read for OverlappedPort {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.transfer(buffer.as_mut_ptr(), buffer.len(), true)
    }
}

impl Write for OverlappedPort {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        // WriteFile only reads the buffer.
        self.transfer(buffer.as_ptr().cast_mut(), buffer.len(), false)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl PortIo for OverlappedPort {
    fn try_clone_port(&self) -> io::Result<Box<dyn PortIo>> {
        Ok(Box::new(Self(Arc::clone(&self.0))))
    }
}

/// Limits `child` to the vCPUs in `mask`: the Windows vCPUs and its port's real-time vCPU.
pub(crate) fn set_process_mask(child: &std::process::Child, mask: u64) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;

    let mask = usize::try_from(mask)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "vCPU mask is too wide"))?;
    // SAFETY: the child's process handle is open while `child` lives; a plain mask.
    if unsafe { SetProcessAffinityMask(child.as_raw_handle(), mask) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
