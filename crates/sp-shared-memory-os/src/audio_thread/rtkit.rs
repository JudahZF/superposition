//! Real-time priority through rtkit when `RLIMIT_RTPRIO` denies `SCHED_FIFO`.
//!
//! Desktop distributions often grant real-time priority only through the rtkit daemon on the
//! system bus. rtkit gives the thread `SCHED_RR | SCHED_RESET_ON_FORK` at most
//! `MaxRealtimePriority`, and only to a process whose `RLIMIT_RTTIME` is no larger than
//! `RTTimeUSecMax`.

use std::io;

use zbus::{blocking::Connection, proxy::CacheProperties};

#[zbus::proxy(
    interface = "org.freedesktop.RealtimeKit1",
    default_service = "org.freedesktop.RealtimeKit1",
    default_path = "/org/freedesktop/RealtimeKit1",
    gen_async = false
)]
trait RealtimeKit1 {
    #[zbus(name = "MakeThreadRealtimeWithPID")]
    fn make_thread_realtime_with_pid(
        &self,
        process: u64,
        thread: u64,
        priority: u32,
    ) -> zbus::Result<()>;

    #[zbus(property)]
    fn max_realtime_priority(&self) -> zbus::Result<i32>;

    #[zbus(property, name = "RTTimeUSecMax")]
    fn rttime_usec_max(&self) -> zbus::Result<i64>;
}

/// The calling thread's kernel thread ID. One system call, so an audio thread may record it.
#[must_use]
pub fn current_thread_id() -> u64 {
    // SAFETY: `gettid` has no preconditions.
    u64::try_from(unsafe { libc::gettid() }).unwrap_or(0)
}

/// Asks rtkit to make `thread` of this process real-time, at `priority` or rtkit's maximum.
pub(super) fn make_thread_realtime(
    connection: &Connection,
    thread: u64,
    priority: i32,
) -> io::Result<()> {
    let rtkit = RealtimeKit1Proxy::builder(connection)
        .cache_properties(CacheProperties::No)
        .build()
        .map_err(io::Error::other)?;
    limit_rttime(rtkit.rttime_usec_max().map_err(io::Error::other)?)?;
    let priority = priority.min(rtkit.max_realtime_priority().map_err(io::Error::other)?);
    rtkit
        .make_thread_realtime_with_pid(
            u64::from(std::process::id()),
            thread,
            u32::try_from(priority).map_err(io::Error::other)?,
        )
        .map_err(io::Error::other)
}

/// Makes another thread of this process real-time through rtkit, unless it already is: for
/// threads a library creates, such as a JACK callback thread. It waits for D-Bus, so call it off
/// the audio path. Returns whether it changed the thread.
///
/// # Errors
/// Returns an error when the thread does not exist, or rtkit is unreachable or refuses.
pub fn promote_thread(thread: u64, priority: i32) -> io::Result<bool> {
    let tid = libc::pid_t::try_from(thread).map_err(io::Error::other)?;
    // SAFETY: `sched_getscheduler` only reads the scheduling policy of `tid`.
    let policy = unsafe { libc::sched_getscheduler(tid) };
    if policy < 0 {
        return Err(io::Error::last_os_error());
    }
    if matches!(
        policy & !libc::SCHED_RESET_ON_FORK,
        libc::SCHED_FIFO | libc::SCHED_RR
    ) {
        return Ok(false);
    }
    let connection = Connection::system().map_err(io::Error::other)?;
    make_thread_realtime(&connection, thread, priority).map(|()| true)
}

/// The highest priority rtkit grants, which also shows that rtkit answers on the system bus.
///
/// # Errors
/// Returns an error when the system bus or rtkit is unreachable.
pub fn max_priority() -> io::Result<i32> {
    let connection = Connection::system().map_err(io::Error::other)?;
    RealtimeKit1Proxy::builder(&connection)
        .cache_properties(CacheProperties::No)
        .build()
        .and_then(|rtkit| rtkit.max_realtime_priority())
        .map_err(io::Error::other)
}

/// rtkit refuses a process that could run a real-time thread longer than `max_us` without
/// blocking. The processing thread blocks every block, so the limit never triggers.
fn limit_rttime(max_us: i64) -> io::Result<()> {
    let max_us = libc::rlim_t::try_from(max_us).map_err(io::Error::other)?;
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is a valid, writable record.
    if unsafe { libc::getrlimit(libc::RLIMIT_RTTIME, &raw mut limit) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if limit.rlim_max <= max_us {
        return Ok(());
    }
    limit.rlim_cur = limit.rlim_cur.min(max_us);
    limit.rlim_max = max_us;
    // SAFETY: `limit` is a valid record; lowering a limit needs no privilege.
    if unsafe { libc::setrlimit(libc::RLIMIT_RTTIME, &raw const limit) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        os::unix::net::UnixStream,
        sync::{Arc, Mutex},
    };

    use zbus::{Guid, blocking::connection::Builder};

    use super::{current_thread_id, make_thread_realtime};

    /// The calls rtkit received, as (process, thread, priority).
    type Calls = Arc<Mutex<Vec<(u64, u64, u32)>>>;

    struct FakeRtkit {
        calls: Calls,
        max_priority: i32,
        max_rttime_us: i64,
    }

    #[zbus::interface(name = "org.freedesktop.RealtimeKit1")]
    impl FakeRtkit {
        // rtkit's own spelling, so a wrong name in the proxy fails this test.
        #[zbus(name = "MakeThreadRealtimeWithPID")]
        fn make_thread_realtime_with_pid(&self, process: u64, thread: u64, priority: u32) {
            self.calls
                .lock()
                .expect("calls lock")
                .push((process, thread, priority));
        }

        #[zbus(property)]
        fn max_realtime_priority(&self) -> i32 {
            self.max_priority
        }

        #[zbus(property, name = "RTTimeUSecMax")]
        fn rttime_usec_max(&self) -> i64 {
            self.max_rttime_us
        }
    }

    /// Checks the request against rtkit's D-Bus interface over a peer-to-peer connection,
    /// because CI machines rarely run rtkit.
    #[test]
    fn asks_rtkit_for_a_thread_of_this_process_within_its_limits() {
        let (server_end, client_end) = UnixStream::pair().expect("socket pair");
        let calls = Calls::default();
        let fake = FakeRtkit {
            calls: calls.clone(),
            max_priority: 20,
            max_rttime_us: 200_000,
        };
        // Both ends authenticate during `build`, so the server builds on its own thread.
        let server = std::thread::spawn(move || {
            Builder::unix_stream(server_end)
                .server(Guid::generate())
                .and_then(|builder| {
                    builder
                        .p2p()
                        .serve_at("/org/freedesktop/RealtimeKit1", fake)
                })
                .and_then(Builder::build)
                .expect("fake rtkit")
        });
        let client = Builder::unix_stream(client_end)
            .p2p()
            .build()
            .expect("client connection");
        let _server = server.join().expect("fake rtkit thread");

        let thread = current_thread_id();
        make_thread_realtime(&client, thread, 70).expect("rtkit request");

        assert_eq!(
            *calls.lock().expect("calls lock"),
            [(u64::from(std::process::id()), thread, 20)]
        );
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: `limit` is a valid, writable record.
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_RTTIME, &raw mut limit) },
            0
        );
        assert!(limit.rlim_max <= 200_000);
    }
}
