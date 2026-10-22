//! Windows side of `cargo xtask wine-check`.
//!
//! Under Wine, opens a bank that a Linux host created, publishes its clock in
//! `worker_heartbeat_tick`, and answers each request wake by storing the wake sequence in
//! `worker_busy_operations`. Exits after the requested number of answers. Opening the bank
//! also proves that the Windows build validates the Linux header and layout.

#[cfg(windows)]
fn main() -> std::io::Result<()> {
    use std::{io, sync::atomic::Ordering, time::Duration};

    use sp_shared_memory_os::{MonotonicClock, SharedMemoryRegion};

    let mut arguments = std::env::args().skip(1);
    let (Some(name), Some(rounds)) = (arguments.next(), arguments.next()) else {
        return Err(io::Error::other("usage: wine_ping <bank name> <rounds>"));
    };
    let rounds: u32 = rounds.parse().map_err(io::Error::other)?;
    let region = SharedMemoryRegion::open(&name)?;
    let clock = MonotonicClock::new()?;
    let header = &region.bank().header;
    let mut observed = region.request_wake_sequence();
    header
        .worker_heartbeat_tick
        .store(clock.now_ticks(), Ordering::Release);
    let mut answered = 0;
    while answered < rounds {
        region.wait_for_request(observed, Duration::from_millis(100))?;
        let current = region.request_wake_sequence();
        if current == observed {
            continue;
        }
        observed = current;
        header
            .worker_heartbeat_tick
            .store(clock.now_ticks(), Ordering::Relaxed);
        header
            .worker_busy_operations
            .store(u64::from(current), Ordering::Release);
        answered += 1;
    }
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!(
        "wine_ping is the Windows side of `cargo xtask wine-check`; build it for x86_64-pc-windows-gnu"
    );
    std::process::exit(2);
}
