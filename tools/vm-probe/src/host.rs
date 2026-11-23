//! The host side: one request per block on a VM port, as a rack lane makes, and the round
//! trip on the host's clock.

use crate::stats::{Histogram, Report};
use rustix::{
    thread::{ClockId, CpuSet, clock_nanosleep_absolute, sched_setaffinity},
    time::Timespec,
};
use sp_ivshmem::PortPeer;
use sp_protocol::BlockRequest;
use sp_shared_memory_os::{AudioThreadPolicy, MappedRackBanks, MonotonicClock, create_port_banks};
use std::{
    io,
    path::PathBuf,
    time::{Duration, Instant},
};

/// A request with no completion after this long is lost: the guest hung or crashed.
const HANG: Duration = Duration::from_millis(250);
/// The longest wait for the guest to connect to the port server.
const GUEST_TIMEOUT: Duration = Duration::from_mins(10);

pub(crate) struct HostOptions {
    pub socket: PathBuf,
    pub port: u32,
    pub frames: u32,
    pub sample_rate: u32,
    pub duration: Duration,
    pub window: Option<Duration>,
    pub cpu: Option<usize>,
    pub realtime: bool,
    pub interval: Duration,
}

pub(crate) fn run(options: &HostOptions) -> io::Result<Report> {
    let mut peer = PortPeer::connect(&options.socket, 1)?;
    let guest = wait_for_guest(&mut peer)?;
    eprintln!(
        "vm-probe: guest is peer {guest}; host is peer {}",
        peer.id()
    );
    let doorbell = peer.doorbell(guest, 0)?;
    // SAFETY: the probe's guest attaches only after the header's magic appears and answers
    // only requests; no worker runs on this port.
    #[allow(unsafe_code, reason = "the port banks' documented contract")]
    let mut banks = unsafe {
        create_port_banks(
            peer.memory().try_clone_to_owned()?,
            options.port,
            1,
            std::os::fd::AsFd::as_fd(&doorbell),
        )
    }?;

    pin(options.cpu)?;
    let mut policy = None;
    if options.realtime {
        let mut entered = AudioThreadPolicy::new()?;
        entered.enter(options.frames, options.sample_rate)?;
        policy = Some(entered);
    }

    let clock = MonotonicClock::new()?;
    let period =
        Duration::from_secs_f64(f64::from(options.frames) / f64::from(options.sample_rate));
    let window = options.window.unwrap_or(period);
    let window_ticks = u32::try_from(clock.duration_to_ticks(window)).unwrap_or(u32::MAX);
    let request = BlockRequest {
        frame_count: options.frames,
        input_channel_count: 2,
        output_channel_count: 2,
        midi_event_count: 0,
        event_count: 0,
        flags: 0,
        sidechain_slots: 0,
    };

    let mut histogram = Histogram::new();
    let mut interval_histogram = Histogram::new();
    let (mut misses, mut lost, mut host_overruns, mut ring_failures) = (0, 0, 0, 0);
    let started = Instant::now();
    let mut last_print = started;
    let mut next = clock.now_ticks() + clock.duration_to_ticks(period);
    let period_ticks = clock.duration_to_ticks(period);

    while started.elapsed() < options.duration {
        clock_nanosleep_absolute(ClockId::Monotonic, &timespec(next))?;
        let exchange = exchange(&mut banks, clock, request, window_ticks)?;
        if exchange.published > next + period_ticks / 2 {
            host_overruns += 1;
        }
        if !exchange.rang {
            ring_failures += 1;
        }
        let Some(completed) = exchange.completed else {
            lost += 1;
            eprintln!("vm-probe: no completion after {HANG:?}; stopping");
            break;
        };
        let round_trip = clock.ticks_to_duration(completed - exchange.published);
        if round_trip > window {
            misses += 1;
        }
        histogram.record(round_trip);
        interval_histogram.record(round_trip);

        next += period_ticks;
        let now = clock.now_ticks();
        if next < now {
            next = now + period_ticks;
        }
        if last_print.elapsed() >= options.interval {
            eprintln!(
                "vm-probe: {:>6.0} s  blocks {:>9}  {}  misses {misses}",
                started.elapsed().as_secs_f64(),
                histogram.total(),
                interval_histogram.summary(),
            );
            interval_histogram = Histogram::new();
            last_print = Instant::now();
        }
    }
    if let Some(mut policy) = policy {
        policy.leave()?;
    }
    Ok(Report {
        frames: options.frames,
        sample_rate: options.sample_rate,
        window_us: window.as_secs_f64() * 1e6,
        realtime: options.realtime,
        cpu: options.cpu,
        blocks: histogram.total(),
        misses,
        lost,
        host_overruns,
        ring_failures,
        p50_us: histogram.percentile_us(0.5),
        p99_us: histogram.percentile_us(0.99),
        p99_99_us: histogram.percentile_us(0.9999),
        max_us: histogram.max_us(),
        histogram_us: histogram.nonzero(),
    })
}

/// Pins this thread to `cpu`, if given.
fn pin(cpu: Option<usize>) -> io::Result<()> {
    if let Some(cpu) = cpu {
        let mut set = CpuSet::new();
        set.set(cpu);
        sched_setaffinity(None, &set)?;
    }
    Ok(())
}

/// One block's request and its completion.
struct Exchange {
    published: u64,
    rang: bool,
    /// When the host saw the completion, or `None` after [`HANG`].
    completed: Option<u64>,
}

/// Publishes one request in slot 0, rings the guest, and spins until its completion.
fn exchange(
    banks: &mut MappedRackBanks,
    clock: MonotonicClock,
    request: BlockRequest,
    window_ticks: u32,
) -> io::Result<Exchange> {
    let published = clock.now_ticks();
    let bank = banks.active_bank_mut().bank_mut();
    let Ok(ticket) = bank.request_timed_block_at(0, request, published, window_ticks) else {
        return Err(io::Error::other("slot 0 is not free: the guest kept it"));
    };
    let rang = banks.active_bank().notify_request();
    let slot = banks
        .active_bank()
        .bank()
        .slot(0)
        .ok_or_else(|| io::Error::other("bank has no slot 0"))?;
    let hang_ticks = clock.duration_to_ticks(HANG);
    let completed = loop {
        let now = clock.now_ticks();
        if let Ok(Some(snapshot)) = slot.completion_snapshot()
            && snapshot.ticket == ticket
        {
            break Some(now);
        }
        if now - published > hang_ticks {
            let _ = slot.abandon_request(ticket);
            break None;
        }
        std::hint::spin_loop();
    };
    if completed.is_some() {
        slot.consume_completion_timing(ticket)
            .map_err(|error| io::Error::other(format!("bad completion: {error}")))?;
    }
    Ok(Exchange {
        published,
        rang,
        completed,
    })
}

/// Waits for exactly one other peer on the port: the guest.
fn wait_for_guest(peer: &mut PortPeer) -> io::Result<u16> {
    let started = Instant::now();
    loop {
        peer.poll_events()?;
        if let Some(guest) = peer.only_other_peer() {
            return Ok(guest);
        }
        if started.elapsed() > GUEST_TIMEOUT {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "no guest connected to the port server",
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn timespec(ticks: u64) -> Timespec {
    Timespec {
        tv_sec: i64::try_from(ticks / 1_000_000_000).unwrap_or(i64::MAX),
        tv_nsec: i64::try_from(ticks % 1_000_000_000).unwrap_or_default(),
    }
}

/// The Linux stand-in for the Windows guest: a peer of the port server that attaches to
/// bank 0 once the host has written it, and answers.
pub(crate) fn fake_guest(
    socket: &std::path::Path,
    port: u32,
    cpu: Option<usize>,
    realtime: bool,
) -> io::Error {
    let result = (|| {
        let peer = PortPeer::connect(socket, 1)?;
        pin(cpu)?;
        // At 96 kHz, the shortest supported block: the policy only needs a valid format.
        let _policy = realtime
            .then(|| {
                let mut policy = AudioThreadPolicy::new()?;
                policy.enter(32, 96_000)?;
                io::Result::Ok(policy)
            })
            .transpose()?;
        let started = Instant::now();
        let mut region = loop {
            match sp_shared_memory_os::attach_port_bank(
                peer.memory().try_clone_to_owned()?,
                port,
                0,
                peer.interrupt(0)?.into(),
            ) {
                Ok(region) => break region,
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock
                        && started.elapsed() < GUEST_TIMEOUT =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error),
            }
        };
        Ok(crate::guest::serve(&mut region, MonotonicClock::new()?))
    })();
    result.unwrap_or_else(|error| error)
}
