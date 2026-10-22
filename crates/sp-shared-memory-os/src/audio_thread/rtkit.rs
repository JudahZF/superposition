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
    fn make_thread_realtime(&self, thread: u64, priority: u32) -> zbus::Result<()>;

    #[zbus(property)]
    fn max_realtime_priority(&self) -> zbus::Result<i32>;

    #[zbus(property, name = "RTTimeUSecMax")]
    fn rttime_usec_max(&self) -> zbus::Result<i64>;
}

/// Asks rtkit to make the calling thread real-time, at `priority` or rtkit's maximum.
pub(super) fn make_current_thread_realtime(
    connection: &Connection,
    priority: i32,
) -> io::Result<()> {
    let rtkit = RealtimeKit1Proxy::builder(connection)
        .cache_properties(CacheProperties::No)
        .build()
        .map_err(io::Error::other)?;
    limit_rttime(rtkit.rttime_usec_max().map_err(io::Error::other)?)?;
    let priority = priority.min(rtkit.max_realtime_priority().map_err(io::Error::other)?);
    // SAFETY: `gettid` has no preconditions.
    let thread = u64::try_from(unsafe { libc::gettid() }).map_err(io::Error::other)?;
    rtkit
        .make_thread_realtime(thread, u32::try_from(priority).map_err(io::Error::other)?)
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

    use super::make_current_thread_realtime;

    /// The calls rtkit received, as (thread, priority).
    type Calls = Arc<Mutex<Vec<(u64, u32)>>>;

    struct FakeRtkit {
        calls: Calls,
        max_priority: i32,
        max_rttime_us: i64,
    }

    #[zbus::interface(name = "org.freedesktop.RealtimeKit1")]
    impl FakeRtkit {
        fn make_thread_realtime(&self, thread: u64, priority: u32) {
            self.calls
                .lock()
                .expect("calls lock")
                .push((thread, priority));
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
    fn asks_rtkit_for_the_calling_thread_within_its_limits() {
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

        make_current_thread_realtime(&client, 70).expect("rtkit request");

        // SAFETY: `gettid` has no preconditions.
        let thread = u64::try_from(unsafe { libc::gettid() }).expect("thread id");
        assert_eq!(*calls.lock().expect("calls lock"), [(thread, 20)]);
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
