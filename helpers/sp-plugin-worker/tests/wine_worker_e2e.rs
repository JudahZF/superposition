//! A Windows plug-in in the Windows worker under Wine, driven by the Linux host exactly as the
//! app drives it: the Windows scanner catalogs the bundle under Wine, the supervisor launches
//! `sp-plugin-worker.exe` through Wine with control pipes, the pipelined rack dispatcher sends it
//! audio through the host's `/dev/shm` bank, and its editor opens on a private Xvfb display.
//!
//! It needs Wine, Xvfb, and the Windows helpers. From the dev shell:
//!
//! ```text
//! cargo xtask windows-helpers
//! SUPERPOSITION_TEST_WINDOWS_VST3="/path/to/Plugin.vst3" \
//!     cargo test -p sp-plugin-worker --test wine_worker_e2e -- --ignored --nocapture
//! ```
//!
//! `SUPERPOSITION_TEST_WINDOWS_HELPERS` names the helper directory that holds `windows/` and
//! `wine-bridge/` (default: beside the native worker). The test copies the bundle into a new
//! prefix under `target/tmp` and deletes the prefix when it ends.
#![cfg(target_os = "linux")]

use std::{
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use sp_audio_io::RackSharedMemoryDispatcher;
use sp_protocol::{
    control::{
        BankIdentity, ControlOperation, ControlResponseStatus, ControlTarget, RackIdentity,
        SlotIdentity,
    },
    payload::{ControlPayloadCodec, PluginSlotConfiguration, RackTopology},
};
use sp_shared_memory_os::{MonotonicClock, SharedMemoryRegion};
use sp_supervisor::{
    HelperKind, HelperLaunch, PluginCatalog, PluginPlatform, PrivateDisplay, ProcessSupervisor,
    Scanner, WorkerControlLaunch, WorkerControlSession, discover_windows_vst3_bundles,
};
use sp_wine::{WineDisplay, WineEnvironment};

mod common;

const FRAMES: usize = 128;
const SAMPLE_RATE: u32 = 48_000;
const BLOCKS: usize = 400;
const GENERATION: u64 = 1;

/// Deletes the test prefix, after its Wine processes and server have stopped.
struct Prefix {
    root: PathBuf,
    wine: Option<WineEnvironment>,
}

impl Drop for Prefix {
    fn drop(&mut self) {
        drop(self.wine.take());
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn helpers_dir() -> PathBuf {
    std::env::var_os("SUPERPOSITION_TEST_WINDOWS_HELPERS").map_or_else(
        || {
            Path::new(env!("CARGO_BIN_EXE_sp-plugin-worker"))
                .parent()
                .expect("worker directory")
                .to_path_buf()
        },
        PathBuf::from,
    )
}

fn copy_tree(from: &Path, to: &Path) {
    let status = Command::new("cp")
        .arg("-r")
        .arg(from)
        .arg(to)
        .status()
        .expect("run cp");
    assert!(status.success(), "copy {} into the prefix", from.display());
}

fn percentile(sorted: &[Duration], permille: usize) -> Duration {
    sorted[(sorted.len() - 1) * permille / 1000]
}

#[test]
#[ignore = "needs Wine, Xvfb, `cargo xtask windows-helpers`, and SUPERPOSITION_TEST_WINDOWS_VST3"]
#[allow(
    clippy::too_many_lines,
    reason = "one Wine prefix and worker lifecycle covers scan, audio, editor, and shutdown"
)]
fn windows_plugin_scans_processes_and_opens_its_editor_under_wine() {
    let bundle = PathBuf::from(
        std::env::var_os("SUPERPOSITION_TEST_WINDOWS_VST3")
            .expect("set SUPERPOSITION_TEST_WINDOWS_VST3 to a Windows .vst3 bundle"),
    );
    let helpers = helpers_dir();
    let worker_exe = helpers.join("windows/sp-plugin-worker.exe");
    let scanner_exe = helpers.join("windows/sp-plugin-scanner.exe");
    assert!(
        worker_exe.is_file() && scanner_exe.is_file(),
        "run `cargo xtask windows-helpers` first ({} is missing)",
        worker_exe.display()
    );

    // One private display and one prefix, like the app's.
    let display = PrivateDisplay::start().expect("start Xvfb");
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("wine-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("prefix root");
    let mut prefix = Prefix {
        root: root.clone(),
        wine: None,
    };
    let mut wine =
        WineEnvironment::new(&root, helpers.join("wine-bridge")).expect("Wine and the bridge");
    wine.set_display(Some(WineDisplay {
        display: display.name().into(),
        authority: display.authority().to_path_buf(),
    }));
    let started = Instant::now();
    wine.prepare().expect("prepare the prefix");
    println!("prefix ready after {:?}", started.elapsed());
    copy_tree(&bundle, &wine.vst3_dir());
    let launcher = wine.launcher().clone();
    prefix.wine = Some(wine);

    // Scan: discovered in the prefix, cataloged by the Windows scanner as a Windows plug-in.
    let discovered = discover_windows_vst3_bundles(&launcher.vst3_dirs());
    assert_eq!(discovered.len(), 1, "the prefix holds the one test bundle");
    let catalog = PluginCatalog::open(root.join("catalog.json")).expect("catalog");
    let mut scanner = Scanner::new(root.join("no-native-scanner"), Duration::from_secs(5), catalog)
        .with_windows_scanner(&scanner_exe, launcher.clone());
    scanner.set_windows_display(Some(display.environment()));
    let started = Instant::now();
    let scan = scanner
        .scan_windows(&discovered[0])
        .expect("scan under Wine");
    println!("scanned under Wine in {:?}", started.elapsed());
    assert_eq!(scan.platform, PluginPlatform::Windows);
    assert!(
        scan.is_supported(),
        "scan outcome {:?}: {:?}",
        scan.outcome(),
        scan.metadata.detail
    );
    let class = scan.metadata.classes.first().expect("one audio class");
    println!("cataloged {} by {}", class.identity.name, class.identity.vendor);
    let bundle_path = WineEnvironment::windows_path(&scan.bundle);

    // Launch: the app's own launch path, through Wine, with control pipes.
    let bank = SharedMemoryRegion::create(GENERATION).expect("rack bank");
    let target = ControlTarget::new(
        RackIdentity::new(0, GENERATION).unwrap(),
        BankIdentity::new(0, GENERATION).unwrap(),
    );
    let launch = WorkerControlLaunch {
        launch: HelperLaunch {
            kind: HelperKind::PluginWorker,
            executable: worker_exe,
            arguments: [
                "--worker",
                "--bank",
                bank.name(),
                "--worker-id",
                "1",
                "--rack-index",
                "0",
                "--rack-generation",
                "1",
                "--bank-index",
                "0",
                "--bank-generation",
                "1",
                "--sample-rate",
                &SAMPLE_RATE.to_string(),
                "--bundle",
                &bundle_path,
                "--class-id",
                &class.identity.unique_id,
            ]
            .map(str::to_owned)
            .to_vec(),
            wine: Some(launcher),
        },
        target,
        timeout: Duration::from_secs(10),
    };
    let mut processes = ProcessSupervisor::new();
    processes.set_display(Some(display.environment()));
    let started = Instant::now();
    let mut session = WorkerControlSession::launch(&mut processes, &launch).expect("Wine worker");
    println!("Wine worker answered after {:?}", started.elapsed());
    let topology = RackTopology {
        slots: vec![PluginSlotConfiguration {
            slot: 0,
            input_channels: 2,
            output_channels: 2,
            event_input_active: false,
            sidechain_active: false,
            bundle_path,
            class_id: Some(class.identity.unique_id.clone()),
        }],
    };
    let rebuilt = session
        .client_mut()
        .request(
            ControlOperation::RebuildRack,
            None,
            &topology.encode().unwrap(),
        )
        .expect("rebuild request");
    assert_eq!(rebuilt.status(), ControlResponseStatus::Ok, "{rebuilt:?}");

    // Audio: the pipelined dispatcher at 128 frames / 48 kHz, paced like a device callback.
    let mut dispatcher = RackSharedMemoryDispatcher::new_indexed(vec![(
        0,
        SharedMemoryRegion::open(bank.name()).expect("callback mapping"),
    )])
    .expect("dispatcher");
    assert!(dispatcher.set_pipelined(0, true));
    let timing = SharedMemoryRegion::open(bank.name()).expect("timing mapping");
    let clock = MonotonicClock::new().unwrap();
    let period = Duration::from_secs_f64(FRAMES as f64 / f64::from(SAMPLE_RATE));
    let input: Vec<f32> = (0..FRAMES * 2)
        .map(|sample| ((sample / 2) as f32 * 0.05).sin() * 0.5)
        .collect();
    let mut round_trips = Vec::with_capacity(BLOCKS);
    let mut processing = Vec::with_capacity(BLOCKS);
    let mut next = Instant::now();
    for _ in 0..BLOCKS {
        // The previous block's request had a whole period; record its timing before the
        // dispatcher consumes it.
        for slot in &timing.bank().slots {
            if let Ok(Some(completion)) = slot.completion_snapshot() {
                let ticks = completion.timing;
                round_trips.push(clock.ticks_to_duration(
                    ticks.completion_published_tick - ticks.request_published_tick,
                ));
                processing.push(clock.ticks_to_duration(
                    ticks.completion_published_tick - ticks.worker_claimed_tick,
                ));
            }
        }
        dispatcher.process_block(&input, &[], FRAMES, period * 3 / 4);
        next += period;
        thread::sleep(next.saturating_duration_since(Instant::now()));
    }
    let telemetry = dispatcher.telemetry(0).expect("rack telemetry");
    round_trips.sort_unstable();
    processing.sort_unstable();
    println!(
        "{BLOCKS} blocks of {FRAMES} frames at {SAMPLE_RATE} Hz (period {period:?}): \
         {} completed, {} deadline misses",
        telemetry.completed, telemetry.deadline_misses
    );
    println!(
        "round trip (publish to completion): p50 {:?}, p99 {:?}, p99.9 {:?}, max {:?}",
        percentile(&round_trips, 500),
        percentile(&round_trips, 990),
        percentile(&round_trips, 999),
        round_trips[round_trips.len() - 1]
    );
    println!(
        "worker processing (claim to completion): p50 {:?}, p99 {:?}, max {:?}",
        percentile(&processing, 500),
        percentile(&processing, 990),
        processing[processing.len() - 1]
    );
    assert_eq!(telemetry.deadline_misses, 0, "{telemetry:?}");
    assert!(telemetry.completed >= u64::try_from(BLOCKS - 2).unwrap());
    assert!(
        matches!(
            dispatcher.sources()[0],
            sp_engine::RackAudioSource::Wet(wet) if wet[..FRAMES * 2].iter().any(|s| *s != 0.0)
        ),
        "the last block mixes the previous block's processed result"
    );

    // Editor: opens on the private display and is pictured.
    let slot = SlotIdentity::new(1).unwrap();
    let opened = session
        .client_mut()
        .request(ControlOperation::OpenNativeEditor, Some(slot), &[])
        .expect("open editor request");
    assert_eq!(opened.status(), ControlResponseStatus::Ok, "{opened:?}");
    let deadline = Instant::now() + Duration::from_secs(20);
    let preview = loop {
        thread::sleep(Duration::from_millis(500));
        let poll = session
            .client_mut()
            .capture_editor_preview(slot, 0)
            .expect("poll the editor picture");
        if let Some(png) = poll.png {
            assert!(poll.editor_open);
            break png;
        }
        assert!(Instant::now() < deadline, "the editor was never pictured");
    };
    assert_png_preview(&preview);
    // Wine's X11 window has the Linux editor window's labels, and Wine's `_NET_WM_PID` is the
    // process the supervisor started, so the streamer maps it to rack 0, slot 0.
    common::assert_editor_window_identity(
        &display,
        u32::try_from(session.process_id()).expect("process ID"),
    );
    let picture = root.join("editor-preview.png");
    std::fs::write(&picture, &preview).unwrap();
    println!("editor picture: {} bytes", preview.len());
    if let Some(keep) = std::env::var_os("SUPERPOSITION_TEST_KEEP_PREVIEW") {
        std::fs::copy(&picture, keep).expect("keep the picture");
    }
    let closed = session
        .client_mut()
        .request(ControlOperation::CloseNativeEditor, Some(slot), &[])
        .expect("close editor request");
    assert_eq!(closed.status(), ControlResponseStatus::Ok, "{closed:?}");

    // Shutdown: an orderly reply, then the Wine process exits.
    let process_id = session.process_id();
    let response = session.shutdown(&mut processes).expect("shutdown");
    assert!(matches!(
        response.status(),
        ControlResponseStatus::Ok | ControlResponseStatus::ShuttingDown
    ));
    assert!(processes.is_reaped(process_id));
    drop(prefix);
    assert!(!root.exists(), "the test prefix is deleted");
}

/// Editor pictures are 320×200 PNGs of more than one colour.
fn assert_png_preview(png: &[u8]) {
    assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"), "PNG signature");
    let mut reader = png::Decoder::new(std::io::Cursor::new(png))
        .read_info()
        .expect("PNG header");
    let mut pixels = vec![0; reader.output_buffer_size().expect("PNG size")];
    let frame = reader.next_frame(&mut pixels).expect("PNG pixels");
    assert_eq!((frame.width, frame.height), (320, 200));
    let pixels = &pixels[..frame.buffer_size()];
    let first = &pixels[..3];
    assert!(
        pixels.chunks_exact(3).any(|pixel| pixel != first),
        "the editor picture is one flat colour; the plug-in did not draw"
    );
}
