//! The app's own path for Windows plug-ins, under real Wine.
//!
//! - A Windows rack: the scan catalogs the plug-in as Windows, the rack launches the Windows
//!   worker through Wine, joins audio as a pipelined lane, and restarts at a new session rate.
//! - A mixed rack of [Linux, Windows, Linux] plug-ins: three segment workers chained inside the
//!   pipelined lane's block. Killing the Windows worker bypasses only its segment, with the
//!   rack's latency kept, and a new worker takes its place. It prints the chain's timing.
//! - The same mixed rack under the product renderer: when a plug-in asks for a planned restart,
//!   the rack reloads live with new workers for every segment while audio keeps running.
//!
//! Ignored: they need Wine, Xvfb, the native and Windows helpers, and a Windows bundle. The
//! mixed rack needs Surge XT Effects as that bundle and a folder in `VST3_PATH` holding the Linux
//! `DragonflyPlateReverb.vst3`. From the dev shell:
//!
//! ```text
//! cargo build -p sp-plugin-worker -p sp-plugin-scanner -p sp-window-streamer
//! cargo xtask windows-helpers
//! SUPERPOSITION_HELPERS_DIR=$PWD/target/debug \
//! SUPERPOSITION_TEST_WINDOWS_VST3="/path/to/Plugin.vst3" \
//! VST3_PATH="$(nix build --no-link --print-out-paths --inputs-from . nixpkgs#dragonfly-reverb)/lib/vst3" \
//!     cargo test -p superposition wine_tests -- --ignored --nocapture --test-threads 1
//! ```
//!
//! The app data, prefix included, lives under `target/tmp` and is deleted afterwards.

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use sp_audio_io::{
    BlockFrames, InterleavedStereoF32, OutputRenderer, ProductRenderer, RackSharedMemoryDispatcher,
};
use sp_model::{
    ChannelLayout, Endpoint, EndpointId, Rack, RackId, RackTopology, Session, Source, SourceId,
};
use sp_protocol::SlotState;
use sp_shared_memory_os::{MonotonicClock, SharedMemoryRegion};

use crate::{CatalogPlugin, ProductRuntime, engine};

/// Runs `test` with the app main thread's stack: the renderer is large in a debug build.
fn on_large_stack(test: fn()) {
    thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(test)
        .expect("test thread")
        .join()
        .expect("the test passes");
}

/// A test's app data folder, prefix included, deleted when the test ends, pass or fail. Declare
/// it before the runtime, so the runtime and its Wine processes stop first.
struct AppData(PathBuf);

impl Drop for AppData {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A product runtime whose prefix holds the Windows test bundle, scanned with the native
/// plug-ins.
fn product_with_windows_bundle(name: &str) -> (AppData, ProductRuntime) {
    let bundle = PathBuf::from(
        std::env::var_os("SUPERPOSITION_TEST_WINDOWS_VST3")
            .expect("set SUPERPOSITION_TEST_WINDOWS_VST3 to a Windows .vst3 bundle"),
    );
    let support = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/tmp")
        .join(format!("{name}-{}", std::process::id()));
    let data = AppData(support);
    let mut product = ProductRuntime::open(&data.0).expect("open the product runtime");
    assert!(product.windows_available(), "{:?}", product.diagnostics());
    // The first scan prepares the prefix; the second finds the installed bundle.
    product
        .scan_standard_locations(true)
        .expect("prepare and scan");
    let vst3 = product.wine.as_ref().expect("Wine").vst3_dir();
    let status = std::process::Command::new("cp")
        .arg("-r")
        .arg(&bundle)
        .arg(&vst3)
        .status()
        .expect("run cp");
    assert!(status.success());
    product.scan_standard_locations(true).expect("rescan");
    (data, product)
}

fn session_with_rack(plugins: &[&CatalogPlugin]) -> Session {
    let mut model = Session::new();
    model.sources.push(Source {
        id: SourceId("input-1".to_owned()),
        name: "Input 1".to_owned(),
        layout: ChannelLayout::Stereo,
    });
    model.endpoints.push(Endpoint {
        id: EndpointId("output-1".to_owned()),
        name: "Output 1".to_owned(),
        layout: ChannelLayout::Stereo,
    });
    model.racks.push(Rack {
        id: RackId("rack-1".to_owned()),
        name: "Rack 1".to_owned(),
        source_id: SourceId("input-1".to_owned()),
        endpoint_id: EndpointId("output-1".to_owned()),
        topology: RackTopology::Serial,
        gain_db: sp_model::GainDb::default(),
        muted: false,
        bypassed: false,
        slots: plugins
            .iter()
            .enumerate()
            .map(|(slot, plugin)| sp_model::PluginSlot {
                id: sp_model::PluginInstanceId(format!("slot-{}", slot + 1)),
                plugin: plugin.descriptor.clone(),
                bypassed: false,
                parameters: plugin.parameters.clone(),
                sidechain: None,
            })
            .collect(),
    });
    model
}

#[test]
#[ignore = "needs Wine, Xvfb, SUPERPOSITION_HELPERS_DIR, and SUPERPOSITION_TEST_WINDOWS_VST3"]
fn windows_rack_runs_pipelined_in_the_wine_worker() {
    on_large_stack(windows_rack);
}

fn windows_rack() {
    let (data, mut product) = product_with_windows_bundle("app-wine");
    let plugin = product
        .catalog_plugins()
        .into_iter()
        .find(|plugin| plugin.windows)
        .expect("the Windows plug-in is cataloged as Windows");
    let model = session_with_rack(&[&plugin]);
    product
        .load_rack(0, &model.racks[0], &[])
        .expect("launch the Windows worker through Wine");
    assert!(product.worker_running(0));
    assert!(product.rack_is_pipelined(0));
    assert_eq!(product.audio_bank_mappings().unwrap().len(), 1);
    // Audio start maps the Windows rack and marks it pipelined.
    let prepared = engine::prepare_audio(&mut product, &model).expect("prepare audio");
    drop(prepared);

    // A new session rate restarts the worker at that rate, keeping its state.
    let before = product.workers[&0].session.as_ref().unwrap().process_id();
    product
        .apply_sample_rate(96_000)
        .expect("restart at 96 kHz");
    let worker = &product.workers[&0];
    assert_eq!(worker.sample_rate_hz, 96_000);
    assert_ne!(worker.session.as_ref().unwrap().process_id(), before);
    assert!(product.rack_is_pipelined(0));

    product.unload_all_racks();
    drop(product);
    drop(data);
}

const FRAMES: usize = 128;
const SAMPLE_RATE: u32 = 48_000;
/// Blocks after a change (a bypass fade, a link's warmup) before the output is checked.
const SETTLE_BLOCKS: usize = 20;

/// The left input sample at frame `n`: a ramp with a distinct value at each step of its period,
/// so a delayed copy matches at one delay only. The right channel is its negative.
#[allow(clippy::cast_precision_loss, reason = "the ramp's steps are below 997")]
fn input_sample(n: usize) -> f32 {
    ((n % 997) as f32 / 997.0 - 0.5) * 0.5
}

/// What the host lane saw for one block.
struct LaneBlock {
    /// The wet output: the previous block's request, through the whole chain.
    output: [f32; FRAMES * 2],
    /// Latency the first segment published (its own plus the chain's).
    latency: u32,
    /// Requests the third segment has received so far.
    last_segment_requests: u64,
    /// Ticks of the previous block, per segment: (claimed, completed). Index 0 also holds the
    /// host's publication tick.
    timing: Option<[(u64, u64, u64); 3]>,
}

/// The audio callback for the test: the rack's pipelined lane, paced at the block period, until
/// `stop`. `progress` counts blocks so the test can act at block boundaries.
fn run_lane(
    banks: [SharedMemoryRegion; 3],
    progress: &AtomicUsize,
    stop: &AtomicBool,
) -> (Vec<LaneBlock>, sp_audio_io::RackDispatchTelemetry) {
    let [first, second, third] = banks;
    let mut dispatcher = RackSharedMemoryDispatcher::new_indexed(vec![(
        0,
        SharedMemoryRegion::open(first.name()).expect("callback mapping"),
    )])
    .expect("dispatcher");
    assert!(dispatcher.set_pipelined(0, true));
    let period = Duration::from_nanos(
        u64::try_from(FRAMES).unwrap() * 1_000_000_000 / u64::from(SAMPLE_RATE),
    );
    let mut blocks = Vec::with_capacity(4096);
    let mut input = [0.0_f32; FRAMES * 2];
    let mut next = Instant::now();
    while !stop.load(Ordering::Acquire) {
        let block = blocks.len();
        for (frame, pair) in input.chunks_exact_mut(2).enumerate() {
            let sample = input_sample(block * FRAMES + frame);
            pair.copy_from_slice(&[sample, -sample]);
        }
        // The previous block's requests are complete in every bank until the next forward
        // frees them: the lane consumes the first, the workers the others. The ticks are read
        // without taking the slots, so the chain never waits on this observer.
        let completion = |bank: &SharedMemoryRegion| {
            bank.bank().slots.iter().find_map(|slot| {
                let metadata = &slot.metadata;
                (metadata.state() == Ok(SlotState::Complete)).then(|| {
                    (
                        metadata.request_published_tick.load(Ordering::Acquire),
                        metadata.worker_claimed_tick.load(Ordering::Acquire),
                        metadata.completion_published_tick.load(Ordering::Acquire),
                    )
                })
            })
        };
        let timing = completion(&first)
            .zip(completion(&second))
            .zip(completion(&third))
            .map(|((first, second), third)| [first, second, third]);
        dispatcher.process_block(&input, &[], FRAMES, period * 3 / 4);
        let mut output = [0.0; FRAMES * 2];
        if let sp_engine::RackAudioSource::Wet(wet) = dispatcher.sources()[0] {
            output.copy_from_slice(&wet[..FRAMES * 2]);
        }
        blocks.push(LaneBlock {
            output,
            latency: first
                .bank()
                .header
                .worker_latency_samples
                .load(Ordering::Acquire),
            last_segment_requests: third
                .bank()
                .header
                .next_request_sequence
                .load(Ordering::Acquire)
                - 1,
            timing,
        });
        progress.store(blocks.len(), Ordering::Release);
        next += period;
        thread::sleep(next.saturating_duration_since(Instant::now()));
    }
    (blocks, dispatcher.telemetry(0).expect("rack telemetry"))
}

/// Whether `blocks` carry the input unchanged, `delay` frames late.
fn passes_input(blocks: &[LaneBlock], first_block: usize, delay: usize) -> bool {
    blocks.iter().enumerate().all(|(offset, block)| {
        block
            .output
            .chunks_exact(2)
            .enumerate()
            .all(|(frame, pair)| {
                let n = (first_block + offset) * FRAMES + frame;
                let expected = n.checked_sub(delay).map_or(0.0, input_sample);
                pair == [expected, -expected]
            })
    })
}

fn percentile(sorted: &[Duration], permille: usize) -> Duration {
    sorted[(sorted.len() - 1) * permille / 1000]
}

#[test]
#[ignore = "needs Wine, Xvfb, SUPERPOSITION_HELPERS_DIR, SUPERPOSITION_TEST_WINDOWS_VST3, and VST3_PATH"]
fn mixed_rack_chains_segments_and_recovers_a_killed_one() {
    on_large_stack(mixed_rack);
}

#[allow(
    clippy::too_many_lines,
    reason = "one running chain is checked through four phases in order"
)]
fn mixed_rack() {
    let (_data, mut product) = product_with_windows_bundle("app-mixed");
    let plugins = product.catalog_plugins();
    let plate = plugins
        .iter()
        .find(|plugin| !plugin.windows && plugin.descriptor.identity.name.contains("Plate"))
        .expect("VST3_PATH holds DragonflyPlateReverb.vst3");
    let windows = plugins
        .iter()
        .find(|plugin| plugin.windows)
        .expect("the Windows plug-in is cataloged as Windows");
    let mut model = session_with_rack(&[plate, windows, plate]);
    // Surge XT Effects passes its input unchanged until its effect is mixed in.
    let mix = product
        .scene_parameter_metadata(&model.racks[0].slots[1])
        .iter()
        .find(|parameter| parameter.name == "Output Mix")
        .map(|parameter| sp_model::ParameterId(parameter.id.to_string()))
        .expect("the mixed-rack test's Windows plug-in is Surge XT Effects");
    model.racks[0].slots[1]
        .parameters
        .values
        .insert(mix, sp_model::NormalizedValue::new(1.0).unwrap());
    product
        .load_rack(0, &model.racks[0], &[])
        .expect("start the three segment workers");
    assert!(product.rack_is_pipelined(0));
    let banks = {
        let worker = &product.workers[&0];
        assert_eq!(
            worker
                .segments
                .iter()
                .map(|segment| segment.slots.clone())
                .collect::<Vec<_>>(),
            [1..2, 2..3],
            "the Windows plug-in and the second plate run in their own segments"
        );
        let open = |bank: &SharedMemoryRegion| SharedMemoryRegion::open(bank.name()).unwrap();
        [
            open(&worker.banks[worker.active_bank_index]),
            open(&worker.segments[0].banks[0]),
            open(&worker.segments[1].banks[0]),
        ]
    };
    for slot in 0..3 {
        product
            .set_slot_bypass(0, slot, true)
            .unwrap_or_else(|error| panic!("{error}: {:#?}", product.diagnostics()));
    }
    let windows_pid = |product: &ProductRuntime| {
        product.workers[&0].segments[0]
            .session
            .as_ref()
            .map(sp_supervisor::WorkerControlSession::process_id)
    };
    let killed_pid = windows_pid(&product).expect("the Windows worker runs");

    let progress = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let lane = thread::spawn({
        let (progress, stop) = (Arc::clone(&progress), Arc::clone(&stop));
        move || run_lane(banks, &progress, &stop)
    });
    // The control plane runs beside the lane, as the app's UI loop does.
    let wait_for_block = |product: &mut ProductRuntime, block: usize| loop {
        let reached = progress.load(Ordering::Acquire);
        if reached >= block {
            return reached;
        }
        product.poll();
        thread::sleep(Duration::from_millis(5));
    };
    // Phase 1: every plug-in bypassed, so the chain passes the input unchanged.
    let processing = wait_for_block(&mut product, 300);
    for slot in 0..3 {
        product
            .set_slot_bypass(0, slot, false)
            .unwrap_or_else(|error| panic!("{error}: {:#?}", product.diagnostics()));
    }
    // Phase 2: every plug-in processes. Phase 3: only the Windows one does.
    let windows_only = wait_for_block(&mut product, processing + 300);
    for slot in [0, 2] {
        product
            .set_slot_bypass(0, slot, true)
            .unwrap_or_else(|error| panic!("{error}: {:#?}", product.diagnostics()));
    }
    // Phase 4: kill the Windows worker; the chain plays it bypassed until its replacement runs.
    let killed = wait_for_block(&mut product, windows_only + 200);
    assert_eq!(
        windows_pid(&product),
        Some(killed_pid),
        "the Windows worker ran until now: {:?}",
        product.diagnostics()
    );
    let status = std::process::Command::new("kill")
        .args(["-KILL", &killed_pid.to_string()])
        .status()
        .expect("run kill");
    assert!(status.success());
    let deadline = Instant::now() + Duration::from_secs(30);
    let recovered = loop {
        product.poll();
        if windows_pid(&product).is_some_and(|pid| pid != killed_pid) {
            break progress.load(Ordering::Acquire);
        }
        assert!(Instant::now() < deadline, "{:?}", product.diagnostics());
        thread::sleep(Duration::from_millis(5));
    };
    wait_for_block(&mut product, recovered + 300);
    stop.store(true, Ordering::Release);
    let (blocks, telemetry) = lane.join().expect("lane thread");
    println!("{}", product.diagnostics().join("\n"));

    let latency = blocks[SETTLE_BLOCKS].latency;
    let delay = FRAMES + latency as usize;
    println!(
        "{} blocks of {FRAMES} frames at {SAMPLE_RATE} Hz; chain latency {latency} samples; \
         phases at blocks {processing}, {windows_only}, kill at {killed}, replaced by {recovered}",
        blocks.len()
    );
    println!(
        "lane: {} completed, {} deadline misses",
        telemetry.completed, telemetry.deadline_misses
    );
    assert_eq!(telemetry.deadline_misses, 0, "{telemetry:?}");
    assert!(
        blocks[SETTLE_BLOCKS..]
            .iter()
            .all(|block| block.latency == latency),
        "the rack's latency never changes"
    );

    // Phase 1: the input arrives unchanged, one block plus the chain's latency late, through
    // every segment in order.
    assert!(passes_input(
        &blocks[SETTLE_BLOCKS..processing],
        SETTLE_BLOCKS,
        delay
    ));
    let timings: Vec<_> = blocks[SETTLE_BLOCKS..killed]
        .iter()
        .filter_map(|block| block.timing)
        .collect();
    assert!(timings.len() > 400, "{} blocks were timed", timings.len());
    for [first, second, third] in &timings {
        assert!(
            first.1 <= second.1
                && second.1 <= third.1
                && third.2 <= second.2
                && second.2 <= first.2,
            "each segment starts after and finishes before the one that forwards to it"
        );
    }
    let clock = MonotonicClock::new().unwrap();
    let mut round_trips: Vec<_> = timings
        .iter()
        .map(|[first, ..]| clock.ticks_to_duration(first.2 - first.0))
        .collect();
    let mut forwards: Vec<_> = timings
        .iter()
        .map(|[_, second, _]| clock.ticks_to_duration(second.2 - second.1))
        .collect();
    let mut hops: Vec<_> = timings
        .iter()
        .flat_map(|[first, second, third]| {
            [
                clock.ticks_to_duration(second.1 - first.1),
                clock.ticks_to_duration(third.1 - second.1),
            ]
        })
        .collect();
    for samples in [&mut round_trips, &mut forwards, &mut hops] {
        samples.sort_unstable();
    }
    let period = Duration::from_nanos(
        u64::try_from(FRAMES).unwrap() * 1_000_000_000 / u64::from(SAMPLE_RATE),
    );
    for (name, samples) in [
        (
            "rack round trip (host publish to first segment's completion)",
            &round_trips,
        ),
        (
            "Windows segment, claim to completion (includes the last segment)",
            &forwards,
        ),
        ("claim to next segment's claim (one hop)", &hops),
    ] {
        println!(
            "{name}: p50 {:?}, p99 {:?}, max {:?} (period {period:?})",
            percentile(samples, 500),
            percentile(samples, 990),
            samples[samples.len() - 1]
        );
    }

    // Phase 2 and 3: the plug-ins change the signal.
    assert!(!passes_input(
        &blocks[processing + SETTLE_BLOCKS..windows_only],
        processing + SETTLE_BLOCKS,
        delay
    ));
    assert!(!passes_input(
        &blocks[windows_only + SETTLE_BLOCKS..killed],
        windows_only + SETTLE_BLOCKS,
        delay,
    ));

    // Phase 4: from the block after the kill, the Windows segment plays bypassed with its
    // latency, so the input passes unchanged with no gap; the last segment keeps receiving
    // blocks; then the replacement takes over.
    let gap_end = killed + 2;
    assert!(passes_input(&blocks[gap_end..recovered], gap_end, delay));
    let outage = &blocks[gap_end..recovered];
    let skipped_to_last = outage
        .windows(2)
        .filter(|pair| pair[1].last_segment_requests > pair[0].last_segment_requests)
        .count();
    println!(
        "Windows worker down for {} blocks; the last segment processed {skipped_to_last} of them",
        outage.len()
    );
    assert!(
        skipped_to_last * 2 > outage.len(),
        "the last segment keeps processing while the Windows segment is down"
    );
    assert_eq!(product.rack_restart_count(0), 1);
    assert!(!passes_input(
        &blocks[recovered + SETTLE_BLOCKS..],
        recovered + SETTLE_BLOCKS,
        delay
    ));

    product.unload_all_racks();
}

#[test]
#[ignore = "needs Wine, Xvfb, the helpers, Surge XT Effects, and Dragonfly; see the module docs"]
fn mixed_rack_reloads_live_when_a_plugin_requests_a_restart() {
    on_large_stack(mixed_rack_reload);
}

/// Every worker process of rack 0, first segment first.
fn rack_worker_pids(product: &ProductRuntime) -> Vec<u64> {
    let worker = &product.workers[&0];
    worker
        .session
        .iter()
        .chain(
            worker
                .segments
                .iter()
                .filter_map(|segment| segment.session.as_ref()),
        )
        .map(sp_supervisor::WorkerControlSession::process_id)
        .collect()
}

/// Renders paced blocks as an audio callback would, until `stop`, and returns the renderer.
fn run_callback(mut renderer: ProductRenderer, stop: &AtomicBool) -> ProductRenderer {
    let period =
        Duration::from_secs(1) * u32::try_from(FRAMES).expect("block frames") / SAMPLE_RATE;
    let mut buffer = vec![0.0_f32; FRAMES * 2];
    let mut next = Instant::now();
    while !stop.load(Ordering::Acquire) {
        let block = InterleavedStereoF32::new(&mut buffer, BlockFrames::Frames128)
            .expect("one stereo block");
        renderer.render(block);
        next += period;
        thread::sleep(next.saturating_duration_since(Instant::now()));
    }
    renderer
}

fn mixed_rack_reload() {
    let (_data, mut product) = product_with_windows_bundle("app-mixed-reload");
    let plugins = product.catalog_plugins();
    let plate = plugins
        .iter()
        .find(|plugin| !plugin.windows && plugin.descriptor.identity.name.contains("Plate"))
        .expect("VST3_PATH holds DragonflyPlateReverb.vst3");
    let windows = plugins
        .iter()
        .find(|plugin| plugin.windows)
        .expect("the Windows plug-in is cataloged as Windows");
    let model = session_with_rack(&[plate, windows, plate]);
    product
        .load_rack(0, &model.racks[0], &[])
        .expect("start the three segment workers");
    let prepared = engine::prepare_audio(&mut product, &model).expect("prepare audio");
    let telemetry = prepared.renderer.telemetry();
    let mut control = prepared.control;
    let stop = Arc::new(AtomicBool::new(false));
    // The renderer is large in a debug build, so it renders on a stack like the test's own.
    let callback = thread::Builder::new()
        .stack_size(64 << 20)
        .spawn({
            let (renderer, stop) = (prepared.renderer, Arc::clone(&stop));
            move || run_callback(renderer, &stop)
        })
        .expect("callback thread");
    let completed = || {
        telemetry
            .rack_diagnostics(0)
            .map_or(0, |rack| rack.completed)
    };
    let poll_until = |product: &mut ProductRuntime,
                      control: &mut sp_audio_io::ProductControl,
                      done: &dyn Fn(&ProductRuntime) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !done(product) {
            product.poll();
            product.finish_live_topology(control);
            assert!(Instant::now() < deadline, "{:?}", product.diagnostics());
            thread::sleep(Duration::from_millis(5));
        }
    };
    poll_until(&mut product, &mut control, &|_| completed() > 200);
    let before = rack_worker_pids(&product);
    assert_eq!(before.len(), 3, "one worker per segment");

    // The Windows plug-in, first in the second segment, asks for a planned restart through its
    // segment's bank, as its worker publishes it.
    {
        let segment = &product.workers[&0].segments[0];
        assert!(
            segment.banks[segment.active_bank_index]
                .bank()
                .feedback
                .publish_restart(0, 1),
            "publish kReloadComponent"
        );
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let reloads = loop {
        product.poll();
        let reloads = product.take_rack_reloads();
        if !reloads.is_empty() {
            break reloads;
        }
        assert!(Instant::now() < deadline, "{:?}", product.diagnostics());
        thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(reloads, [0]);
    let reload_at = completed();
    product
        .reload_rack_live(&mut control, &model, 0)
        .unwrap_or_else(|error| panic!("{error}: {:?}", product.diagnostics()));
    poll_until(&mut product, &mut control, &|product| {
        product.retiring_workers.is_empty() && rack_worker_pids(product).len() == 3
    });
    let after = rack_worker_pids(&product);
    assert!(
        after.iter().all(|pid| !before.contains(pid)),
        "every segment runs in a new worker: {before:?} -> {after:?}"
    );
    assert!(
        product.take_rack_reloads().is_empty(),
        "the request was taken once"
    );
    poll_until(&mut product, &mut control, &|_| {
        completed() > reload_at + 200
    });
    stop.store(true, Ordering::Release);
    drop(callback.join().expect("callback thread"));
    println!("{}", product.diagnostics().join("\n"));
    println!("reloaded at block {reload_at}; workers {before:?} -> {after:?}");
    product.unload_all_racks();
}
