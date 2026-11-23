//! Operating-system primitives behind the platform-neutral mapping types.
//!
//! Every backend exposes the same functions:
//!
//! - named object lifecycle: `create_exclusive`, `open_existing`, `set_len`, `len`, `map`,
//!   `unmap`, `close`, `unlink`, `page_size`;
//! - the shared clock: `clock_now`, `clock_timebase`;
//! - the request wake: `wake_supported`, `notify`, `wait`.
//!
//! macOS and Linux create and open POSIX objects. The Windows backend runs as a plug-in
//! worker in one of two places. Under Wine it opens the Linux host's `/dev/shm` object
//! through the `Z:` drive and reaches the futex and clock through the `sp_wine_bridge`
//! unixlib. In Lambda's VM it opens a VM port's IVSHMEM device (`windows_vm`), waits on the
//! port's doorbell event, and reads the guest's performance counter.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) mod windows_vm;

#[cfg(target_os = "linux")]
pub(crate) use linux::*;
#[cfg(target_os = "macos")]
pub(crate) use macos::*;
#[cfg(windows)]
pub(crate) use windows::*;

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
compile_error!("sp-shared-memory-os supports macOS, Linux, and Windows workers");
