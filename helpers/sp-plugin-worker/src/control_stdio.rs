//! The worker's control channel: the stdin and stdout the supervisor started it with.
//!
//! Plug-ins print to stdout. Before any plug-in loads, the worker moves the inherited streams to
//! private handles, points the process's standard output at standard error, and gives standard
//! input nothing to read, so plug-in output cannot corrupt a control frame. The same pipes work
//! for a Windows worker under Wine: Wine's standard handles are the host's pipes.
#![allow(unsafe_code)]

use std::{fs::File, io};

/// `_dup2` in the Microsoft C runtimes.
#[cfg(windows)]
type Dup2 = unsafe extern "C" fn(i32, i32) -> i32;

/// Takes the control channel: `(requests, responses)`.
///
/// # Errors
/// Returns an error when the standard streams cannot be duplicated or redirected.
#[cfg(unix)]
pub fn take() -> io::Result<(File, File)> {
    use std::os::fd::{FromRawFd, OwnedFd};

    let duplicate = |fd| {
        // SAFETY: `fcntl` duplicates a descriptor the process owns into a new close-on-exec one.
        let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
        if copy < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `copy` is a fresh descriptor that nothing else owns.
        Ok(unsafe { OwnedFd::from_raw_fd(copy) })
    };
    let requests = duplicate(libc::STDIN_FILENO)?;
    let responses = duplicate(libc::STDOUT_FILENO)?;
    let null = File::open("/dev/null")?;
    // SAFETY: both calls replace standard descriptors with descriptors the process owns.
    let redirected = unsafe {
        libc::dup2(std::os::fd::AsRawFd::as_raw_fd(&null), libc::STDIN_FILENO) >= 0
            && libc::dup2(libc::STDERR_FILENO, libc::STDOUT_FILENO) >= 0
    };
    if !redirected {
        return Err(io::Error::last_os_error());
    }
    Ok((File::from(requests), File::from(responses)))
}

/// Takes the control channel: `(requests, responses)`.
///
/// C runtimes that already started keep their own copy of the standard output handle, so each
/// loaded one also moves its descriptor 1 to standard error. Runtimes that plug-ins load later
/// start from the redirected handles.
///
/// # Errors
/// Returns an error when the standard handles cannot be duplicated or redirected.
#[cfg(windows)]
pub fn take() -> io::Result<(File, File)> {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};

    use windows_sys::Win32::{
        Foundation::{
            DUPLICATE_SAME_ACCESS, DuplicateHandle, FALSE, GENERIC_READ, HANDLE,
            INVALID_HANDLE_VALUE,
        },
        Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, OPEN_EXISTING},
        System::{
            Console::{
                GetStdHandle, STD_ERROR_HANDLE, STD_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
                SetStdHandle,
            },
            LibraryLoader::{GetModuleHandleW, GetProcAddress},
            Threading::GetCurrentProcess,
        },
    };

    let duplicate = |which: STD_HANDLE| -> io::Result<OwnedHandle> {
        let mut copy: HANDLE = std::ptr::null_mut();
        // SAFETY: the source is this process's standard handle; `copy` receives a new handle.
        let duplicated = unsafe {
            let process = GetCurrentProcess();
            DuplicateHandle(
                process,
                GetStdHandle(which),
                process,
                &raw mut copy,
                0,
                FALSE,
                DUPLICATE_SAME_ACCESS,
            )
        };
        if duplicated == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `copy` is a fresh handle that nothing else owns.
        Ok(unsafe { OwnedHandle::from_raw_handle(copy) })
    };
    let requests = duplicate(STD_INPUT_HANDLE)?;
    let responses = duplicate(STD_OUTPUT_HANDLE)?;
    let nul: Vec<u16> = "NUL\0".encode_utf16().collect();
    // SAFETY: `nul` is NUL-terminated; the other arguments are documented constants. The
    // handle stays open for the process's lifetime as its standard input.
    let null = unsafe {
        CreateFileW(
            nul.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    // SAFETY: both calls replace this process's standard handles.
    let redirected = null != INVALID_HANDLE_VALUE
        && unsafe {
            SetStdHandle(STD_INPUT_HANDLE, null) != 0
                && SetStdHandle(STD_OUTPUT_HANDLE, GetStdHandle(STD_ERROR_HANDLE)) != 0
        };
    if !redirected {
        return Err(io::Error::last_os_error());
    }
    for runtime in ["msvcrt.dll", "ucrtbase.dll"] {
        let name: Vec<u16> = runtime.encode_utf16().chain([0]).collect();
        // SAFETY: `name` is NUL-terminated; a module that is not loaded returns null.
        let module = unsafe { GetModuleHandleW(name.as_ptr()) };
        if module.is_null() {
            continue;
        }
        // SAFETY: `_dup2` has the `Dup2` signature in both runtimes.
        if let Some(dup2) = unsafe { GetProcAddress(module, c"_dup2".as_ptr().cast()) } {
            // SAFETY: as above; descriptors 2 and 1 are the runtime's standard error and output.
            unsafe {
                std::mem::transmute::<unsafe extern "system" fn() -> isize, Dup2>(dup2)(2, 1);
            }
        }
    }
    Ok((File::from(requests), File::from(responses)))
}
