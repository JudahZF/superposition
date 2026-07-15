//! CoreAudio-attached Phase 1 IPC feasibility harness.
//!
//! Unlike the synthetic preflight, this command starts a real AUHAL output callback and
//! performs shared-memory observe/dispatch work inside that callback. It certifies device
//! attachment for development evidence; long-duration hard-gate qualification still requires
//! dedicated hardware runs.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use sp_audio_io_macos::{
    ActiveOutput, InterleavedStereoF32, PhaseOneConfig, PhaseOneFrames, PhaseOneRenderer,
    RenderDisposition,
};
use sp_shared_memory::BlockRequest;
use sp_shared_memory_macos::MonotonicClock;

use crate::phase1::{
    self, DeviceBlockCounters, Harness, Phase1Error, build_worker, ensure_phase1_platform,
    fixed_request, process_device_callback_block, start_noop_harness,
};

/// Runs CoreAudio-attached IPC feasibility.
pub(crate) fn run_device_feasibility(
    workspace_root: &Path,
    arguments: &[String],
) -> Result<phase1::CommandOutcome, Phase1Error> {
    ensure_phase1_platform()?;
    let options = parse_options(arguments).map_err(Phase1Error::InvalidConfiguration)?;
    let clock = MonotonicClock::new().map_err(|error| {
        Phase1Error::Infrastructure(format!("could not initialize continuous clock: {error}"))
    })?;
    let worker = build_worker(workspace_root, false).map_err(Phase1Error::Infrastructure)?;
    let period = phase1::block_period(options.frame_count);
    let harness = start_noop_harness(options.rack_count, 1, &worker, period)
        .map_err(Phase1Error::Infrastructure)?;
    let request = fixed_request(options.frame_count);
    let frames = match options.frame_count {
        128 => PhaseOneFrames::Frames128,
        256 => PhaseOneFrames::Frames256,
        other => {
            return Err(Phase1Error::InvalidConfiguration(format!(
                "unsupported frame count {other}"
            )));
        }
    };

    let shared = Arc::new(SharedDeviceStats::default());
    let stop = Arc::new(AtomicBool::new(false));
    let renderer = DeviceIpcRenderer {
        harness: Some(harness),
        clock,
        request,
        block_index: 0,
        timing: phase1::TimingHistograms::default(),
        stats: Arc::clone(&shared),
        stop: Arc::clone(&stop),
        last_error: None,
    };

    let config = PhaseOneConfig::new(frames).allow_device_reconfiguration();
    let mut output = ActiveOutput::start(config, renderer).map_err(|error| {
        Phase1Error::Infrastructure(format!(
            "could not start CoreAudio output: {error}. \
             device-feasibility needs a default output that can run fixed 48 kHz stereo callbacks \
             at {} frames (BlackHole or a reconfigurable hardware device). \
             This failure is infrastructure, not synthetic acceptance evidence.",
            options.frame_count
        ))
    })?;

    let started = Instant::now();
    while started.elapsed() < options.duration {
        if shared.fatal_error.load(Ordering::Acquire) {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    stop.store(true, Ordering::Release);
    let stop_error = output.stop().err();
    let telemetry = output.telemetry();
    let callbacks = shared.callbacks.load(Ordering::Acquire);
    let deadline_misses = shared.deadline_misses.load(Ordering::Acquire);
    let accepted = shared.accepted_completions.load(Ordering::Acquire);
    let protocol_faults = shared.protocol_faults.load(Ordering::Acquire);
    let worker_exits = shared.worker_exits.load(Ordering::Acquire);
    let attached = callbacks > 0 && telemetry.callbacks > 0;

    println!("DEVICE_IPC_FEASIBILITY");
    println!(
        "  {} rack(s), {} frames, {} second(s), coreaudio_callback_attached={}",
        options.rack_count,
        options.frame_count,
        options.duration.as_secs(),
        attached
    );
    println!(
        "  device callbacks={} (telemetry={}), accepted_completions={}, deadline_misses={}, protocol_faults={}, worker_exits={}",
        callbacks, telemetry.callbacks, accepted, deadline_misses, protocol_faults, worker_exits
    );
    println!(
        "  frame_histogram_128={}, frame_histogram_256={}, phase1_hard_gate_certified=false",
        telemetry.frame_histogram_128, telemetry.frame_histogram_256
    );
    if let Some(error) = stop_error {
        println!("  stop_warning={error}");
    }
    if !attached {
        return Err(Phase1Error::Infrastructure(
            "CoreAudio callback never fired; device attachment failed".to_owned(),
        ));
    }
    if deadline_misses > 0 || protocol_faults > 0 || worker_exits > 0 {
        return Ok(phase1::CommandOutcome::acceptance_failure());
    }
    Ok(phase1::CommandOutcome::passed())
}

pub(crate) fn device_feasibility_usage() -> &'static str {
    "device-feasibility --racks <1|2|4|8> --frames <128|256> --duration-seconds <seconds>"
}

#[derive(Debug)]
struct DeviceOptions {
    rack_count: usize,
    frame_count: u32,
    duration: Duration,
}

fn parse_options(arguments: &[String]) -> Result<DeviceOptions, String> {
    let mut rack_count = None;
    let mut frame_count = None;
    let mut duration_seconds = None;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--racks" => {
                index += 1;
                let value = arguments
                    .get(index)
                    .ok_or_else(|| "missing value for --racks".to_owned())?;
                rack_count = Some(parse_racks(value)?);
            }
            "--frames" => {
                index += 1;
                let value = arguments
                    .get(index)
                    .ok_or_else(|| "missing value for --frames".to_owned())?;
                frame_count = Some(parse_frames(value)?);
            }
            "--duration-seconds" => {
                index += 1;
                let value = arguments
                    .get(index)
                    .ok_or_else(|| "missing value for --duration-seconds".to_owned())?;
                let seconds: u64 = value
                    .parse()
                    .map_err(|_| format!("invalid --duration-seconds '{value}'"))?;
                if seconds == 0 {
                    return Err("--duration-seconds must be positive".to_owned());
                }
                duration_seconds = Some(seconds);
            }
            other => return Err(format!("unknown argument '{other}'")),
        }
        index += 1;
    }
    Ok(DeviceOptions {
        rack_count: rack_count.ok_or_else(|| "missing --racks".to_owned())?,
        frame_count: frame_count.ok_or_else(|| "missing --frames".to_owned())?,
        duration: Duration::from_secs(
            duration_seconds.ok_or_else(|| "missing --duration-seconds".to_owned())?,
        ),
    })
}

fn parse_racks(value: &str) -> Result<usize, String> {
    match value {
        "1" | "2" | "4" | "8" => Ok(value.parse().expect("checked literal")),
        _ => Err(format!("--racks must be 1, 2, 4, or 8; got '{value}'")),
    }
}

fn parse_frames(value: &str) -> Result<u32, String> {
    match value {
        "128" | "256" => Ok(value.parse().expect("checked literal")),
        _ => Err(format!("--frames must be 128 or 256; got '{value}'")),
    }
}

#[derive(Default)]
struct SharedDeviceStats {
    callbacks: AtomicU64,
    accepted_completions: AtomicU64,
    deadline_misses: AtomicU64,
    protocol_faults: AtomicU64,
    worker_exits: AtomicU64,
    fatal_error: AtomicBool,
}

struct DeviceIpcRenderer {
    harness: Option<Harness>,
    clock: MonotonicClock,
    request: BlockRequest,
    block_index: u64,
    timing: phase1::TimingHistograms,
    stats: Arc<SharedDeviceStats>,
    stop: Arc<AtomicBool>,
    last_error: Option<String>,
}

impl PhaseOneRenderer for DeviceIpcRenderer {
    fn render(&mut self, _output: InterleavedStereoF32<'_>) -> RenderDisposition {
        if self.stop.load(Ordering::Acquire) || self.stats.fatal_error.load(Ordering::Acquire) {
            return RenderDisposition::Silence;
        }
        let Some(harness) = self.harness.as_mut() else {
            return RenderDisposition::Silence;
        };
        self.stats.callbacks.fetch_add(1, Ordering::Release);
        match process_device_callback_block(
            harness,
            self.block_index,
            self.request,
            self.clock,
            &mut self.timing,
        ) {
            Ok(counters) => {
                record_counters(&self.stats, counters);
                self.block_index = self.block_index.saturating_add(1);
            }
            Err(error) => {
                self.last_error = Some(error);
                self.stats.fatal_error.store(true, Ordering::Release);
            }
        }
        RenderDisposition::Silence
    }
}

fn record_counters(stats: &SharedDeviceStats, counters: DeviceBlockCounters) {
    stats
        .accepted_completions
        .fetch_add(counters.accepted_racks as u64, Ordering::Release);
    stats
        .deadline_misses
        .fetch_add(counters.deadline_misses, Ordering::Release);
    stats
        .protocol_faults
        .fetch_add(counters.protocol_faults, Ordering::Release);
    stats
        .worker_exits
        .fetch_add(counters.worker_exits, Ordering::Release);
}

/// Keeps the worker path type visible to clippy when the harness module changes.
#[allow(dead_code)]
fn _worker_path_type(_: PathBuf) {}
