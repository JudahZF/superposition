//! `SCHED_FIFO` for the processing thread on Linux, or under Wine through `sp_wine_bridge`.
//! On Linux, rtkit is the fallback when `RLIMIT_RTPRIO` denies it.

use std::{io, marker::PhantomData, rc::Rc, sync::Once};

use super::{SchedulingMode, validate_format};

/// Default `SCHED_FIFO` priority. Override with `SUPERPOSITION_RT_PRIORITY` (1–99).
const DEFAULT_PRIORITY: i32 = 70;
const EPERM: i32 = 1;

/// Owns the current thread's real-time scheduling state.
///
/// Construct, enter, and leave this on the same thread. It cannot be sent to another thread.
/// `Drop` restores ordinary scheduling if `leave` was not called.
pub struct AudioThreadPolicy {
    active: Option<(u32, u32)>,
    mode: SchedulingMode,
    _thread_bound: PhantomData<Rc<()>>,
}

impl AudioThreadPolicy {
    /// Captures the current thread.
    ///
    /// # Errors
    /// Under Wine, returns `Unsupported` when `sp_wine_bridge.dll` is not loaded.
    pub fn new() -> io::Result<Self> {
        #[cfg(windows)]
        if crate::sys::bridge().is_none() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "sp_wine_bridge.dll is required for worker scheduling",
            ));
        }
        Ok(Self {
            active: None,
            mode: SchedulingMode::RealTime,
            _thread_bound: PhantomData,
        })
    }

    /// Applies `SCHED_FIFO` for the given processing format.
    ///
    /// Repeated calls with the same format do not reapply the policy.
    ///
    /// # Errors
    /// Returns an error for an unsupported format, for a scheduler failure other than a
    /// permission denial, or for a denial that rtkit does not lift when
    /// `SUPERPOSITION_REQUIRE_REALTIME=1`.
    pub fn enter(&mut self, frames: u32, sample_rate: u32) -> io::Result<()> {
        validate_format(frames, sample_rate)?;
        if self.active == Some((frames, sample_rate)) {
            return Ok(());
        }
        if self.mode == SchedulingMode::RealTime {
            match set_fifo(priority()) {
                0 => {}
                EPERM => {
                    if let Err(error) = request_from_rtkit(priority()) {
                        if realtime_required() {
                            return Err(io::Error::from_raw_os_error(EPERM));
                        }
                        self.mode = SchedulingMode::Normal;
                        warn_normal_scheduling(&error);
                    }
                }
                code => return Err(io::Error::from_raw_os_error(code)),
            }
        }
        self.active = Some((frames, sample_rate));
        Ok(())
    }

    /// The scheduling this thread received.
    #[must_use]
    pub fn mode(&self) -> SchedulingMode {
        self.mode
    }

    /// Restores ordinary scheduling after real-time processing.
    ///
    /// # Errors
    /// Returns an operating-system error if the scheduler rejects the restoration.
    pub fn leave(&mut self) -> io::Result<()> {
        if self.active.take().is_some() && self.mode == SchedulingMode::RealTime {
            match set_other() {
                0 => {}
                code => return Err(io::Error::from_raw_os_error(code)),
            }
        }
        Ok(())
    }
}

impl Drop for AudioThreadPolicy {
    fn drop(&mut self) {
        let _ = self.leave();
    }
}

/// The highest `SCHED_FIFO` priority this process may set itself (`RLIMIT_RTPRIO`).
#[cfg(target_os = "linux")]
#[must_use]
pub fn realtime_priority_limit() -> u64 {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is a valid, writable record.
    if unsafe { libc::getrlimit(libc::RLIMIT_RTPRIO, &raw mut limit) } == 0 {
        limit.rlim_cur
    } else {
        0
    }
}

fn priority() -> i32 {
    std::env::var("SUPERPOSITION_RT_PRIORITY")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|priority| (1..=99).contains(priority))
        .unwrap_or(DEFAULT_PRIORITY)
}

fn realtime_required() -> bool {
    std::env::var_os("SUPERPOSITION_REQUIRE_REALTIME").is_some_and(|value| value == "1")
}

fn warn_normal_scheduling(rtkit_error: &io::Error) {
    static WARNED: Once = Once::new();
    WARNED.call_once(|| {
        eprintln!(
            "superposition: real-time priority denied (raise RLIMIT_RTPRIO; rtkit: {rtkit_error}); \
             the processing thread uses ordinary scheduling"
        );
    });
}

#[cfg(target_os = "linux")]
fn request_from_rtkit(priority: i32) -> io::Result<()> {
    let connection = zbus::blocking::Connection::system().map_err(io::Error::other)?;
    super::rtkit::make_current_thread_realtime(&connection, priority)
}

#[cfg(windows)]
fn request_from_rtkit(_priority: i32) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "a Wine worker cannot reach rtkit",
    ))
}

#[cfg(target_os = "linux")]
fn set_fifo(priority: i32) -> i32 {
    let parameter = libc::sched_param {
        sched_priority: priority,
    };
    // SAFETY: `pthread_self` is the calling thread and `parameter` is a valid record.
    unsafe {
        libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_FIFO, &raw const parameter)
    }
}

/// rtkit sets `SCHED_RESET_ON_FORK`, which an unprivileged thread cannot clear, so it is kept.
#[cfg(target_os = "linux")]
fn set_other() -> i32 {
    // SAFETY: 0 names the calling thread.
    let current = unsafe { libc::sched_getscheduler(0) };
    if current < 0 {
        return io::Error::last_os_error().raw_os_error().unwrap_or(EPERM);
    }
    let parameter = libc::sched_param { sched_priority: 0 };
    let policy = libc::SCHED_OTHER | (current & libc::SCHED_RESET_ON_FORK);
    // SAFETY: 0 names the calling thread and `parameter` is a valid record.
    if unsafe { libc::sched_setscheduler(0, policy, &raw const parameter) } == 0 {
        0
    } else {
        io::Error::last_os_error().raw_os_error().unwrap_or(EPERM)
    }
}

#[cfg(windows)]
fn set_fifo(priority: i32) -> i32 {
    let bridge = crate::sys::bridge().expect("checked in AudioThreadPolicy::new");
    // SAFETY: the bridge applies the policy to the calling Linux thread.
    unsafe { (bridge.set_scheduler)(1, priority) }
}

#[cfg(windows)]
fn set_other() -> i32 {
    let bridge = crate::sys::bridge().expect("checked in AudioThreadPolicy::new");
    // SAFETY: the bridge applies the policy to the calling Linux thread.
    unsafe { (bridge.set_scheduler)(0, 0) }
}
