//! Soaks rack lanes on Lambda's block timing and reports deadline misses and worker timing as
//! JSON. Run it through `cargo xtask lane-soak`, which builds the workers and the Wine bridge:
//!
//! ```text
//! cargo xtask lane-soak --frames 32 --racks mixed --duration 8h --load-threads 4 \
//!     --out lane-soak-32-mixed.json
//! ```
//!
//! Each run starts racks of one kind (`native`, `wine`, or `mixed`: Linux, Windows, Linux
//! segments): one in the first layer, one in the last layer (`--budget`, default 0.7), and one
//! per `--ladder` window. The schedule reads every rack at the start of the next block and
//! publishes each at its placement, so each rack gets exactly its window. A plug-in supervisor
//! thread replaces failed workers, so a miss costs one rack a replacement, not the run.
//!
//! A rack is clean when it has no deadline miss (no completion at its read), no late completion
//! (completed after its deadline, before a late read), and no later-segment replacement. The
//! report gives, per rack, those counts and the p99.99 and maximum claim-to-completion ticks
//! from the bank, and the smallest window whose rack stayed clean.

use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use sp_lane_adapter::{
    Insert, Launch, Platform, ProcessLaunch, RackControl, SegmentSpec, WineLaunch,
    histogram::Histogram,
    schedule::{Placement, Schedule},
};
use sp_rack_lane::{LaneLayout, LaneTelemetry};
use sp_shared_memory_os::{AudioThreadPolicy, MonotonicClock, SchedulingMode};

/// Histogram buckets: 1 µs wide, up to 20 ms.
const BUCKET: Duration = Duration::from_micros(1);
const BUCKETS: usize = 20_000;

struct Options {
    frames: usize,
    sample_rate: u32,
    racks: Vec<Platform>,
    kind: &'static str,
    duration: Duration,
    warmup: Duration,
    budget: f64,
    ladder: Vec<f64>,
    channels: Channels,
    load_threads: usize,
    worker_load: Duration,
    label: String,
    out: Option<PathBuf>,
    report_every: Duration,
    native_worker: PathBuf,
    wine: Option<WineLaunch>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Channels {
    Mono,
    Stereo,
    Both,
}

struct RackStats {
    placement: Placement,
    layout: LaneLayout,
    claim_to_completion: Histogram,
    publish_to_completion: Histogram,
    late_completions: u64,
    /// Lane telemetry when the warm-up ended.
    baseline: LaneTelemetry,
}

fn main() {
    match run() {
        Ok(()) => {}
        Err(error) => {
            eprintln!("sp-lane-soak: {error}");
            std::process::exit(1);
        }
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one linear run: start, soak, report, stop"
)]
fn run() -> Result<(), String> {
    let options = parse(std::env::args().skip(1))?;
    let clock = MonotonicClock::new().map_err(|error| error.to_string())?;
    let launch = Launch::Processes(ProcessLaunch {
        native_worker: options.native_worker.clone(),
        wine: options.wine.clone(),
    });
    let segments: Vec<SegmentSpec> = options
        .racks
        .iter()
        .map(|&platform| SegmentSpec {
            platform,
            gain: 1.0,
            load: options.worker_load,
        })
        .collect();
    let mut placements = vec![
        Placement::FIRST_LAYER,
        Placement::last_layer(options.budget),
    ];
    placements.extend(
        options
            .ladder
            .iter()
            .map(|&window| Placement::window(window)),
    );

    let mut inserts = Vec::new();
    let mut controls = Vec::new();
    let mut stats = Vec::new();
    for (index, &placement) in placements.iter().enumerate() {
        // With both, odd racks are mono.
        let mono = match options.channels {
            Channels::Mono => true,
            Channels::Stereo => false,
            Channels::Both => index % 2 == 1,
        };
        let layout = if mono {
            LaneLayout::Mono
        } else {
            LaneLayout::Stereo
        };
        eprintln!("starting rack {index} ({})", options.kind);
        let (insert, control) =
            RackControl::start(launch.clone(), &segments, layout, options.sample_rate)
                .map_err(|error| format!("rack {index} did not start: {error}"))?;
        inserts.push(insert);
        controls.push(control);
        stats.push(RackStats {
            placement,
            layout,
            claim_to_completion: Histogram::new(clock.duration_to_ticks(BUCKET), BUCKETS),
            publish_to_completion: Histogram::new(clock.duration_to_ticks(BUCKET), BUCKETS),
            late_completions: 0,
            baseline: LaneTelemetry::default(),
        });
    }

    let stop = Arc::new(AtomicBool::new(false));
    let load: Vec<_> = (0..options.load_threads)
        .map(|_| {
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                let mut value = 0_u64;
                while !stop.load(Ordering::Relaxed) {
                    value = std::hint::black_box(value.wrapping_mul(6_364_136_223_846_793_005) + 1);
                }
            })
        })
        .collect();
    let replacements: Arc<Vec<[AtomicU32; 2]>> = Arc::new(
        (0..controls.len())
            .map(|_| [AtomicU32::new(0), AtomicU32::new(0)])
            .collect(),
    );
    let supervisor_error = Arc::new(Mutex::new(None::<String>));
    let supervisor = thread::spawn({
        let stop = Arc::clone(&stop);
        let replacements = Arc::clone(&replacements);
        let supervisor_error = Arc::clone(&supervisor_error);
        move || {
            while !stop.load(Ordering::Acquire) {
                for (index, control) in controls.iter_mut().enumerate() {
                    if let Err(error) = control.service() {
                        *supervisor_error.lock().expect("error slot") =
                            Some(format!("rack {index}: {error}"));
                    }
                    let counters = control.counters();
                    replacements[index][0].store(counters.replacements, Ordering::Relaxed);
                    replacements[index][1].store(counters.segment_replacements, Ordering::Relaxed);
                }
                thread::sleep(Duration::from_millis(1));
            }
            controls
        }
    });

    let mut policy = AudioThreadPolicy::new().map_err(|error| error.to_string())?;
    policy
        .enter(
            u32::try_from(options.frames).unwrap_or(u32::MAX),
            options.sample_rate,
        )
        .map_err(|error| error.to_string())?;
    let scheduling = match policy.mode() {
        SchedulingMode::RealTime => "realtime",
        SchedulingMode::Normal => "normal",
    };
    let period = Duration::from_nanos(
        options.frames as u64 * 1_000_000_000 / u64::from(options.sample_rate),
    );
    let mut schedule = Schedule::new(clock, period);
    let mut input = vec![0.0_f32; options.frames * 2];
    let mut output = vec![0.0_f32; options.frames * 2];
    let started = Instant::now();
    let mut warm = options.warmup.is_zero();
    let mut blocks = 0_u64;
    let mut overruns = 0_u64;
    let mut max_overrun = 0_u64;
    let mut max_start_lateness = 0_u64;
    let mut last_report = Instant::now();
    eprintln!(
        "soaking {} racks at {} frames, {} Hz ({scheduling}) for {:?}",
        inserts.len(),
        options.frames,
        options.sample_rate,
        options.duration
    );
    while started.elapsed() < options.duration + options.warmup {
        if !warm && started.elapsed() >= options.warmup {
            warm = true;
            for (stat, insert) in stats.iter_mut().zip(&inserts) {
                stat.baseline = insert.lane().telemetry();
            }
            blocks = 0;
            overruns = 0;
            max_overrun = 0;
            max_start_lateness = 0;
        }
        #[allow(clippy::cast_precision_loss, reason = "a test signal")]
        let value = (blocks % 1000) as f32 / 1000.0;
        for frame in input.chunks_exact_mut(2) {
            frame[0] = value;
            frame[1] = -value;
        }
        let block_start = schedule.next_block_start();
        let report = schedule.run_block(
            &mut inserts,
            &placements,
            &input,
            options.frames,
            &mut output,
            |index, outcome, _| {
                let Some(timing) = outcome.timing else {
                    return;
                };
                if !warm {
                    return;
                }
                let stat = &mut stats[index];
                stat.claim_to_completion.record(
                    timing
                        .completion_published_tick
                        .saturating_sub(timing.worker_claimed_tick),
                );
                stat.publish_to_completion.record(
                    timing
                        .completion_published_tick
                        .saturating_sub(timing.request_published_tick),
                );
                if timing.completion_published_tick > block_start {
                    stat.late_completions += 1;
                }
            },
        );
        blocks += 1;
        max_start_lateness = max_start_lateness.max(report.start_lateness);
        if report.overrun > 0 {
            overruns += 1;
            max_overrun = max_overrun.max(report.overrun);
        }
        if last_report.elapsed() >= options.report_every {
            last_report = Instant::now();
            let interim = report_json(
                &options,
                clock,
                period,
                &stats,
                &inserts,
                &replacements,
                [blocks, overruns, max_overrun, max_start_lateness],
                scheduling,
                started.elapsed().saturating_sub(options.warmup),
            );
            write_report(&options, &interim)?;
        }
    }
    policy.leave().map_err(|error| error.to_string())?;
    let report = report_json(
        &options,
        clock,
        period,
        &stats,
        &inserts,
        &replacements,
        [blocks, overruns, max_overrun, max_start_lateness],
        scheduling,
        options.duration,
    );
    stop.store(true, Ordering::Release);
    for thread in load {
        let _ = thread.join();
    }
    let controls = supervisor.join().map_err(|_| "the supervisor panicked")?;
    drop(inserts);
    for control in controls {
        control.stop();
    }
    if let Some(error) = supervisor_error.lock().expect("error slot").take() {
        eprintln!("supervisor error: {error}");
    }
    write_report(&options, &report)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?
    );
    Ok(())
}

fn write_report(options: &Options, report: &Value) -> Result<(), String> {
    let Some(out) = options.out.as_ref() else {
        return Ok(());
    };
    let text = serde_json::to_string_pretty(report).map_err(|error| error.to_string())?;
    std::fs::write(out, text).map_err(|error| format!("could not write {}: {error}", out.display()))
}

#[allow(
    clippy::too_many_arguments,
    clippy::cast_precision_loss,
    reason = "one report from the run's state"
)]
fn report_json(
    options: &Options,
    clock: MonotonicClock,
    period: Duration,
    stats: &[RackStats],
    inserts: &[Insert],
    replacements: &[[AtomicU32; 2]],
    [blocks, overruns, max_overrun, max_start_lateness]: [u64; 4],
    scheduling: &str,
    elapsed: Duration,
) -> Value {
    let micros = |ticks: u64| clock.ticks_to_duration(ticks).as_secs_f64() * 1e6;
    let quantiles = |histogram: &Histogram| {
        json!({
            "count": histogram.count(),
            "p50_ticks": histogram.quantile(0.5),
            "p99_ticks": histogram.quantile(0.99),
            "p99_99_ticks": histogram.quantile(0.9999),
            "max_ticks": histogram.max(),
            "p99_99_us": histogram.quantile(0.9999).map(micros),
            "max_us": micros(histogram.max()),
        })
    };
    let mut smallest_clean: Option<f64> = None;
    let racks: Vec<Value> = stats
        .iter()
        .zip(inserts)
        .zip(replacements)
        .map(|((stat, insert), replaced)| {
            let telemetry = insert.lane().telemetry();
            let misses = telemetry.deadline_misses - stat.baseline.deadline_misses;
            let replacements = replaced[0].load(Ordering::Relaxed);
            let segment_replacements = replaced[1].load(Ordering::Relaxed);
            let window_us = stat.placement.window_of(period).as_secs_f64() * 1e6;
            let clean = misses == 0 && stat.late_completions == 0 && segment_replacements == 0;
            if clean && smallest_clean.is_none_or(|smallest| window_us < smallest) {
                smallest_clean = Some(window_us);
            }
            json!({
                "placement": stat.placement.fraction(),
                "window_us": window_us,
                "channels": match stat.layout {
                    LaneLayout::Mono => "mono",
                    LaneLayout::Stereo => "stereo",
                },
                "published": telemetry.published - stat.baseline.published,
                "completed": telemetry.completed - stat.baseline.completed,
                "deadline_misses": misses,
                "missed_unclaimed": telemetry.missed_unclaimed - stat.baseline.missed_unclaimed,
                "missed_in_progress": telemetry.missed_in_progress - stat.baseline.missed_in_progress,
                "late_completions": stat.late_completions,
                "worker_replacements": replacements,
                "segment_replacements": segment_replacements,
                "wake_failures": telemetry.wake_failures,
                "max_wake_us": micros(telemetry.max_wake_ticks),
                "clean": clean,
                "claim_to_completion": quantiles(&stat.claim_to_completion),
                "publish_to_completion": quantiles(&stat.publish_to_completion),
            })
        })
        .collect();
    json!({
        "label": options.label,
        "racks_kind": options.kind,
        "segments": options.racks.iter().map(|platform| platform.name()).collect::<Vec<_>>(),
        "frames": options.frames,
        "sample_rate": options.sample_rate,
        "period_us": period.as_secs_f64() * 1e6,
        "budget": options.budget,
        "duration_s": elapsed.as_secs_f64(),
        "warmup_s": options.warmup.as_secs_f64(),
        "load_threads": options.load_threads,
        "worker_load_us": options.worker_load.as_secs_f64() * 1e6,
        "tick_ns": clock.ticks_to_duration(1_000_000).as_nanos() as f64 / 1e6,
        "host": {
            "cpu": cpu_model(),
            "kernel": std::fs::read_to_string("/proc/sys/kernel/osrelease")
                .map(|release| release.trim().to_owned())
                .ok(),
            "scheduling": scheduling,
        },
        "schedule": {
            "blocks": blocks,
            "overruns": overruns,
            "max_overrun_us": micros(max_overrun),
            "max_start_lateness_us": micros(max_start_lateness),
        },
        "racks": racks,
        "smallest_clean_window_us": smallest_clean,
    })
}

fn cpu_model() -> Option<String> {
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()?
        .lines()
        .find_map(|line| {
            line.strip_prefix("model name")
                .and_then(|rest| rest.split_once(':'))
                .map(|(_, model)| model.trim().to_owned())
        })
}

fn parse(mut arguments: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut options = Options {
        frames: 32,
        sample_rate: 96_000,
        racks: vec![Platform::Native],
        kind: "native",
        duration: Duration::from_mins(1),
        warmup: Duration::from_secs(5),
        budget: 0.7,
        ladder: Vec::new(),
        channels: Channels::Both,
        load_threads: 0,
        worker_load: Duration::ZERO,
        label: "smoke".to_owned(),
        out: None,
        report_every: Duration::from_mins(10),
        native_worker: std::env::current_exe()
            .map_err(|error| error.to_string())?
            .with_file_name("sp-lane-worker"),
        wine: None,
    };
    let mut wine_worker = None;
    let mut wine_bridge = None;
    let mut wine_prefix = None;
    while let Some(option) = arguments.next() {
        let value = arguments
            .next()
            .ok_or_else(|| format!("`{option}` needs a value"))?;
        let invalid = || format!("invalid value for `{option}`: {value}");
        match option.as_str() {
            "--frames" => options.frames = value.parse().map_err(|_| invalid())?,
            "--sample-rate" => options.sample_rate = value.parse().map_err(|_| invalid())?,
            "--racks" => {
                (options.racks, options.kind) = match value.as_str() {
                    "native" => (vec![Platform::Native], "native"),
                    "wine" => (vec![Platform::Windows], "wine"),
                    "mixed" => (
                        vec![Platform::Native, Platform::Windows, Platform::Native],
                        "mixed",
                    ),
                    _ => return Err(invalid()),
                };
            }
            "--duration" => options.duration = parse_duration(&value).ok_or_else(invalid)?,
            "--warmup" => options.warmup = parse_duration(&value).ok_or_else(invalid)?,
            "--report-every" => {
                options.report_every = parse_duration(&value).ok_or_else(invalid)?;
            }
            "--budget" => options.budget = value.parse().map_err(|_| invalid())?,
            "--ladder" => {
                options.ladder = value
                    .split(',')
                    .map(str::parse)
                    .collect::<Result<_, _>>()
                    .map_err(|_| invalid())?;
            }
            "--channels" => {
                options.channels = match value.as_str() {
                    "mono" => Channels::Mono,
                    "stereo" => Channels::Stereo,
                    "both" => Channels::Both,
                    _ => return Err(invalid()),
                };
            }
            "--load-threads" => options.load_threads = value.parse().map_err(|_| invalid())?,
            "--worker-load-us" => {
                options.worker_load = Duration::from_micros(value.parse().map_err(|_| invalid())?);
            }
            "--label" => options.label = value,
            "--out" => options.out = Some(PathBuf::from(value)),
            "--native-worker" => options.native_worker = PathBuf::from(value),
            "--wine-worker" => wine_worker = Some(PathBuf::from(value)),
            "--wine-bridge" => wine_bridge = Some(PathBuf::from(value)),
            "--wine-prefix" => wine_prefix = Some(PathBuf::from(value)),
            _ => return Err(format!("unknown option `{option}`")),
        }
    }
    if options.racks.contains(&Platform::Windows) {
        options.wine = Some(WineLaunch {
            wine: "wine".into(),
            worker_exe: wine_worker.ok_or("Windows racks need --wine-worker")?,
            bridge_dir: wine_bridge.ok_or("Windows racks need --wine-bridge")?,
            prefix: wine_prefix.ok_or("Windows racks need --wine-prefix")?,
        });
    }
    if 2 + options.ladder.len() > 64 {
        return Err("at most 62 ladder windows".to_owned());
    }
    Ok(options)
}

/// `90s`, `10m`, `8h`, or plain seconds.
fn parse_duration(value: &str) -> Option<Duration> {
    let (number, unit) = value
        .find(|character: char| !character.is_ascii_digit())
        .map_or((value, "s"), |split| value.split_at(split));
    let number: u64 = number.parse().ok()?;
    match unit {
        "s" => Some(Duration::from_secs(number)),
        "m" => Some(Duration::from_mins(number)),
        "h" => Some(Duration::from_hours(number)),
        _ => None,
    }
}
