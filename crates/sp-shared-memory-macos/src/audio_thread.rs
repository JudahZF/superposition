//! Scoped Mach scheduling for the dedicated, paced plug-in processing thread.

use std::{io, marker::PhantomData, ptr::NonNull, rc::Rc};

unsafe extern "C" {
    fn superposition_audio_thread_policy_new(out: *mut *mut std::ffi::c_void) -> i32;
    fn superposition_audio_thread_policy_enter(
        policy: *mut std::ffi::c_void,
        frames: u32,
        sample_rate: u32,
    ) -> i32;
    fn superposition_audio_thread_policy_leave(policy: *mut std::ffi::c_void) -> i32;
    fn superposition_audio_thread_policy_destroy(policy: *mut std::ffi::c_void);
}

/// Owns the current thread's time-constraint scheduling state.
///
/// Construct, enter, and leave this on the same thread. It cannot be sent to another thread.
/// `Drop` attempts to restore ordinary scheduling if `leave` was not called.
pub struct AudioThreadPolicy {
    handle: NonNull<std::ffi::c_void>,
    _thread_bound: PhantomData<Rc<()>>,
}

impl AudioThreadPolicy {
    /// Captures the current thread and its ordinary scheduling policy.
    ///
    /// # Errors
    /// Returns an operating-system error if the thread port or current policy is unavailable, or
    /// if the thread already has a real-time time constraint that this guard cannot restore.
    pub fn new() -> io::Result<Self> {
        let mut handle = std::ptr::null_mut();
        // SAFETY: the C shim writes one owned opaque handle to this valid output pointer.
        let status = unsafe { superposition_audio_thread_policy_new(&raw mut handle) };
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status));
        }
        let handle = NonNull::new(handle).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "audio thread policy handle is null",
            )
        })?;
        Ok(Self {
            handle,
            _thread_bound: PhantomData,
        })
    }

    /// Applies a time constraint based on the actual callback cadence.
    ///
    /// Repeated calls with the same format do not reapply the Mach policy.
    ///
    /// # Errors
    /// Returns an error unless the format is 48 kHz with 32, 64, 128, or 256 frames, or Mach rejects
    /// the policy. Call only on the thread that created this value.
    pub fn enter(&mut self, frames: u32, sample_rate: u32) -> io::Result<()> {
        // SAFETY: the handle is owned by this thread-bound value; the shim validates the format.
        let status = unsafe {
            superposition_audio_thread_policy_enter(self.handle.as_ptr(), frames, sample_rate)
        };
        if status == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(status))
        }
    }

    /// Restores ordinary scheduling after real-time processing.
    ///
    /// # Errors
    /// Returns an operating-system error if Mach rejects the restoration.
    pub fn leave(&mut self) -> io::Result<()> {
        // SAFETY: the handle is owned by this thread-bound value.
        let status = unsafe { superposition_audio_thread_policy_leave(self.handle.as_ptr()) };
        if status == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(status))
        }
    }
}

impl Drop for AudioThreadPolicy {
    fn drop(&mut self) {
        // SAFETY: this is the sole owner of the handle; the shim restores scheduling first.
        unsafe { superposition_audio_thread_policy_destroy(self.handle.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::AudioThreadPolicy;

    #[test]
    fn policy_validates_format_and_restores_the_dedicated_thread() {
        std::thread::spawn(|| {
            let mut policy = AudioThreadPolicy::new().expect("thread policy");
            assert!(policy.enter(127, 48_000).is_err());
            assert!(policy.enter(128, 44_100).is_err());
            for frames in [32, 64, 128, 256] {
                policy.enter(frames, 48_000).expect("time constraint");
                assert!(
                    AudioThreadPolicy::new().is_err(),
                    "a second guard cannot restore this thread's active time constraint"
                );
                policy
                    .enter(frames, 48_000)
                    .expect("cached time constraint");
                policy.leave().expect("ordinary scheduling");
                policy.leave().expect("already ordinary");
            }
        })
        .join()
        .expect("thread joined");
    }
}
