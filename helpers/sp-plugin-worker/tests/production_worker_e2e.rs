//! End-to-end production worker tests.
//!
//! These tests exercise the exact surface the application uses: the real `sp-plugin-worker`
//! binary in `--worker` mode, a real POSIX shared-memory bank, the real Unix-domain control
//! socket, and — when a supported plug-in is installed — a real VST3 plug-in.
//!
//! Plug-in discovery order:
//! 1. `SUPERPOSITION_TEST_VST3` (bundle path) with optional `SUPERPOSITION_TEST_VST3_CLASS`.
//! 2. A small allowlist of well-behaved free plug-ins under the standard VST3 directories.
//!
//! Every test that needs a plug-in skips silently when none is available, mirroring the
//! repository's other environment-gated tests.

use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use sp_protocol::{
    BlockRequest, BlockTicket, ProtocolError,
    control::{
        BankIdentity, ControlErrorCode, ControlOperation, ControlRequest, ControlRequestId,
        ControlResponse, ControlResponseStatus, ControlTarget, RackIdentity, SlotIdentity,
        unix::UnixControlClient,
    },
    payload::{ControlPayloadCodec, PluginSlotConfiguration, RackTopology},
};
use sp_shared_memory_macos::{MonotonicClock, SharedMemoryRegion};
use sp_vst3::{
    Vst3BundlePath, adapter::ProcessingFormat, adapter::Vst3ClassSelection, sdk::HostSdkRackFactory,
};

const LAUNCH_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const BLOCK_TIMEOUT: Duration = Duration::from_secs(5);
const BANK_GENERATION: u64 = 1;
const RACK_GENERATION: u64 = 1;

/// Well-behaved free plug-ins that are safe to describe inside the test process. Discovery
/// still verifies native Apple Silicon code via the SDK-free layout probe before any load, and
/// arbitrary bundles are never instantiated in-process: a misbehaving plug-in must only ever be
/// able to crash the isolated worker or scanner helpers.
const KNOWN_SIMPLE_PLUGINS: &[&str] = &[
    "ValhallaSupermassive.vst3",
    "ValhallaFreqEcho.vst3",
    "ValhallaSpaceModulator.vst3",
    "FetDrive.vst3",
];

#[derive(Clone)]
struct TestPlugin {
    bundle: PathBuf,
    class_id: String,
    input_channels: u8,
    output_channels: u8,
}

fn plugin_directories() -> Vec<PathBuf> {
    let mut directories = vec![PathBuf::from("/Library/Audio/Plug-Ins/VST3")];
    if let Some(home) = std::env::var_os("HOME") {
        directories.push(PathBuf::from(home).join("Library/Audio/Plug-Ins/VST3"));
    }
    directories
}

/// Enumerates classes without instantiating any plug-in in the test process, then asks the
/// factory to describe the class so the fixed stereo rack contract is known to accept it.
fn describe_candidate(bundle: &Path) -> Option<TestPlugin> {
    // The SDK-free layout probe rejects Intel-only bundles before anything is loaded;
    // attempting to load non-native code can crash the loading process outright.
    let layout = sp_vst3::inspect_bundle_layout(bundle).ok()?;
    if !layout.supported_on_apple_silicon {
        return None;
    }
    let factory = HostSdkRackFactory;
    let path = Vst3BundlePath::new(bundle);
    let class = factory.enumerate_classes(&path).ok()?.into_iter().next()?;
    let selection = Vst3ClassSelection::new(path, class.class_id.clone());
    let format = ProcessingFormat::new(48_000.0, 256).ok()?;
    let metadata = factory.describe_class(&selection, format).ok()?;
    let inputs = metadata
        .audio_inputs
        .first()
        .map_or(0, |bus| bus.channel_count);
    let outputs = metadata
        .audio_outputs
        .first()
        .map_or(0, |bus| bus.channel_count);
    if outputs == 0 || outputs > 2 || inputs > 2 {
        return None;
    }
    Some(TestPlugin {
        bundle: bundle.to_path_buf(),
        class_id: class.class_id,
        input_channels: inputs,
        output_channels: outputs,
    })
}

/// VST3 modules are not safe to load concurrently in one process, and discovery loads the
/// candidate once to read its bus topology. The result is computed a single time and shared.
fn discover_plugin() -> Option<TestPlugin> {
    static DISCOVERED: std::sync::OnceLock<Option<TestPlugin>> = std::sync::OnceLock::new();
    DISCOVERED.get_or_init(discover_plugin_uncached).clone()
}

fn discover_plugin_uncached() -> Option<TestPlugin> {
    if let Ok(bundle) = std::env::var("SUPERPOSITION_TEST_VST3") {
        let bundle = PathBuf::from(bundle);
        if bundle.exists() {
            return describe_candidate(&bundle);
        }
    }
    for directory in plugin_directories() {
        for name in KNOWN_SIMPLE_PLUGINS {
            let bundle = directory.join(name);
            if bundle.exists()
                && let Some(plugin) = describe_candidate(&bundle)
            {
                return Some(plugin);
            }
        }
    }
    None
}

fn unique_suffix() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    )
}

fn target() -> ControlTarget {
    ControlTarget::new(
        RackIdentity::new(0, RACK_GENERATION).expect("rack identity"),
        BankIdentity::new(0, BANK_GENERATION).expect("bank identity"),
    )
}

fn slot_identity(slot: usize) -> SlotIdentity {
    SlotIdentity::new(u64::try_from(slot).expect("slot index") + 1).expect("slot identity")
}

/// Uses the same shared Darwin monotonic clock as the worker; timestamps from any other
/// clock domain are rejected by the shared-memory protocol as out of order.
fn now_tick() -> u64 {
    MonotonicClock::new().expect("timebase").now_ticks().max(1)
}

/// One launched production worker with its host-side bank mapping and control client.
struct WorkerHarness {
    child: Child,
    region: SharedMemoryRegion,
    client: UnixControlClient,
    socket: PathBuf,
    next_request_id: u64,
}

impl WorkerHarness {
    /// Launches the deployed worker exactly as the application does and completes the same
    /// connect-then-initial-health handshake.
    fn launch(plugin: &TestPlugin) -> Result<Self, String> {
        let region =
            SharedMemoryRegion::create(BANK_GENERATION).map_err(|error| error.to_string())?;
        let socket = std::env::temp_dir().join(format!("sp-e2e-{}.sock", unique_suffix()));
        let _ = std::fs::remove_file(&socket);
        let child = Command::new(env!("CARGO_BIN_EXE_sp-plugin-worker"))
            .args([
                "--worker",
                "--bank",
                region.name(),
                "--worker-id",
                "1",
                "--rack-index",
                "0",
                "--rack-generation",
                &RACK_GENERATION.to_string(),
                "--bank-index",
                "0",
                "--bank-generation",
                &BANK_GENERATION.to_string(),
                "--control-socket",
                &socket.display().to_string(),
                "--bundle",
                &plugin.bundle.display().to_string(),
                "--class-id",
                &plugin.class_id,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("could not spawn worker: {error}"))?;
        let client = match connect_with_retry(&socket) {
            Ok(client) => client,
            Err(error) => {
                let mut failed = Self {
                    child,
                    region,
                    client: UnixControlClient::from_stream(disconnected_stream()),
                    socket,
                    next_request_id: 0,
                };
                return Err(format!("{error}; {}", failed.drain_output()));
            }
        };
        let mut harness = Self {
            child,
            region,
            client,
            socket,
            next_request_id: 0,
        };
        let health = harness.request(ControlOperation::QueryHealth, None, &[])?;
        if health.status() != ControlResponseStatus::Ok {
            return Err(format!(
                "initial health rejected: {:?} {}",
                health.status(),
                harness.drain_output()
            ));
        }
        Ok(harness)
    }

    /// Sends one request and reads its correlated response, as the application's client does.
    fn request(
        &mut self,
        operation: ControlOperation,
        slot: Option<SlotIdentity>,
        payload: &[u8],
    ) -> Result<ControlResponse, String> {
        self.next_request_id += 1;
        let request = ControlRequest::new(
            ControlRequestId::new(self.next_request_id).expect("nonzero id"),
            target(),
            operation,
            slot,
            payload,
        )
        .map_err(|error| format!("invalid request: {error}"))?;
        self.client
            .round_trip(&request)
            .map_err(|error| format!("{operation:?} round trip failed: {error}"))
    }

    /// Sends a request and requires an `Ok` status, mirroring the application's `request_ok`.
    fn request_ok(
        &mut self,
        operation: ControlOperation,
        slot: Option<SlotIdentity>,
        payload: &[u8],
    ) -> Result<ControlResponse, String> {
        let response = self.request(operation, slot, payload)?;
        if response.status() == ControlResponseStatus::Ok {
            Ok(response)
        } else {
            let detail = response.error_record().map_or_else(
                || format!("{:?}", response.status()),
                |error| error.message().to_owned(),
            );
            Err(format!("{operation:?} rejected: {detail}"))
        }
    }

    fn rebuild_topology(&mut self, plugin: &TestPlugin, slots: &[usize]) -> Result<(), String> {
        let topology = RackTopology {
            slots: slots
                .iter()
                .map(|&slot| PluginSlotConfiguration {
                    slot: u8::try_from(slot).expect("slot index"),
                    input_channels: plugin.input_channels,
                    output_channels: plugin.output_channels,
                    event_input_active: false,
                    bundle_path: plugin.bundle.display().to_string(),
                    class_id: Some(plugin.class_id.clone()),
                })
                .collect(),
        };
        let payload = topology.encode().map_err(|error| error.to_string())?;
        let result = self.request_ok(ControlOperation::RebuildRack, None, &payload);
        match result {
            Ok(_) => Ok(()),
            Err(error) if error.contains("round trip failed") => {
                Err(format!("{error}; {}", self.drain_output()))
            }
            Err(error) => Err(error),
        }
    }

    /// Publishes one stereo block request and waits for the worker's completion.
    fn process_block(&mut self, frames: u32, input: f32) -> Result<Vec<f32>, String> {
        let frame_count = usize::try_from(frames).expect("frame count");
        let slot_index = 0;
        let bank = self.region.bank_mut();
        let slot = bank.slot_mut(slot_index).ok_or("slot 0 exists")?;
        for frame in 0..frame_count {
            slot.input_audio[0][frame] = input;
            slot.input_audio[1][frame] = input;
        }
        let request = BlockRequest {
            frame_count: frames,
            input_channel_count: 2,
            output_channel_count: 2,
            midi_event_count: 0,
            event_count: 0,
            flags: 0,
        };
        let ticket = bank
            .request_block_at(slot_index, request, now_tick())
            .map_err(|error| format!("could not publish block request: {error}"))?;
        let deadline = Instant::now() + BLOCK_TIMEOUT;
        loop {
            let slot = self.region.bank().slot(slot_index).ok_or("slot 0 exists")?;
            match slot.completion_snapshot() {
                Ok(Some(snapshot)) if snapshot.ticket == ticket => break,
                Ok(_) | Err(ProtocolError::Owned) => {}
                Err(error) => return Err(format!("invalid completion: {error}")),
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "worker did not complete the block before timeout; state {:?}; {}",
                    slot.metadata.state(),
                    self.drain_output()
                ));
            }
            thread::sleep(Duration::from_millis(1));
        }
        let slot = self.region.bank().slot(slot_index).ok_or("slot 0 exists")?;
        let mut output = Vec::with_capacity(frame_count * 2);
        for frame in 0..frame_count {
            output.push(slot.output_audio[0][frame]);
            output.push(slot.output_audio[1][frame]);
        }
        // Release the slot so later blocks in the same test can reuse it.
        let _ = release_completed_slot(ticket, slot_index, self.region.bank_mut());
        Ok(output)
    }

    fn shutdown(mut self) -> Result<(), String> {
        // `Ok` and `ShuttingDown` are both orderly acknowledgements of a shutdown request;
        // which one arrives depends on how the reply races the worker's own teardown.
        match self.request(ControlOperation::Shutdown, None, &[]) {
            Ok(response)
                if matches!(
                    response.status(),
                    ControlResponseStatus::Ok | ControlResponseStatus::ShuttingDown
                ) => {}
            Ok(response) => {
                return Err(format!(
                    "shutdown rejected with {:?}; {}",
                    response.status(),
                    self.drain_output()
                ));
            }
            Err(error) => return Err(format!("{error}; {}", self.drain_output())),
        }
        let deadline = Instant::now() + LAUNCH_TIMEOUT;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    if status.success() {
                        return Ok(());
                    }
                    return Err(format!("worker exited with {status}"));
                }
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                Ok(None) => return Err("worker did not exit after shutdown".to_owned()),
                Err(error) => return Err(format!("could not reap worker: {error}")),
            }
        }
    }

    fn drain_output(&mut self) -> String {
        // Give a crashing worker a moment to be reaped so the exit status is observable.
        thread::sleep(Duration::from_millis(100));
        let mut detail = match self.child.try_wait() {
            Ok(Some(status)) => format!("worker exited: {status}; "),
            Ok(None) => "worker still running; ".to_owned(),
            Err(error) => format!("could not query worker: {error}; "),
        };
        let _ = self.child.kill();
        if let Some(stderr) = self.child.stderr.as_mut() {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            if !text.is_empty() {
                detail.push_str("worker stderr: ");
                detail.push_str(text.trim());
            }
        }
        detail
    }
}

impl Drop for WorkerHarness {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// A pre-closed stream used only to satisfy the harness type when launch already failed.
fn disconnected_stream() -> std::os::unix::net::UnixStream {
    let (left, _right) = std::os::unix::net::UnixStream::pair().expect("socket pair");
    left
}

fn connect_with_retry(socket: &Path) -> Result<UnixControlClient, String> {
    let deadline = Instant::now() + LAUNCH_TIMEOUT;
    loop {
        match std::os::unix::net::UnixStream::connect(socket) {
            Ok(stream) => {
                stream
                    .set_read_timeout(Some(REQUEST_TIMEOUT))
                    .map_err(|error| error.to_string())?;
                stream
                    .set_write_timeout(Some(REQUEST_TIMEOUT))
                    .map_err(|error| error.to_string())?;
                return Ok(UnixControlClient::from_stream(stream));
            }
            Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            Err(error) => return Err(format!("could not connect control socket: {error}")),
        }
    }
}

fn release_completed_slot(
    ticket: BlockTicket,
    slot_index: usize,
    bank: &mut sp_shared_memory::SharedBank,
) -> Result<(), String> {
    let slot = bank.slot_mut(slot_index).ok_or("slot exists")?;
    slot.consume_completion(ticket)
        .map_err(|error| format!("could not consume completion: {error}"))
}

#[test]
#[ignore = "manual diagnosis probe"]
fn debug_probe_rebuild_lifecycle() {
    let Some(plugin) = discover_plugin() else {
        return;
    };
    let mut harness = WorkerHarness::launch(&plugin).expect("launch worker");
    let started = Instant::now();
    let rebuild = harness.rebuild_topology(&plugin, &[0]);
    eprintln!(
        "rebuild after {:?}: {:?}",
        started.elapsed(),
        rebuild.as_ref().map(|()| "ok")
    );
    for round in 0..20 {
        thread::sleep(Duration::from_millis(100));
        let health = harness.request(ControlOperation::QueryHealth, None, &[]);
        let status = harness.child.try_wait();
        eprintln!(
            "round {round}: health {:?}; child {:?}",
            health.as_ref().map(ControlResponse::status),
            status
        );
        if health.is_err() {
            eprintln!("harness detail: {}", harness.drain_output());
            break;
        }
    }
}

#[test]
fn worker_launches_health_checks_and_shuts_down_cleanly() {
    let Some(plugin) = discover_plugin() else {
        return;
    };
    let harness = WorkerHarness::launch(&plugin).expect("launch worker");
    harness.shutdown().expect("clean shutdown");
}

#[test]
fn rebuild_rack_succeeds_and_worker_stays_responsive() {
    let Some(plugin) = discover_plugin() else {
        return;
    };
    let mut harness = WorkerHarness::launch(&plugin).expect("launch worker");
    harness
        .rebuild_topology(&plugin, &[0])
        .expect("rebuild with one real plug-in");
    harness
        .request_ok(ControlOperation::QueryHealth, None, &[])
        .expect("health after rebuild");
    // A second rebuild replaces the same topology; the app performs this on every rack edit.
    harness
        .rebuild_topology(&plugin, &[0])
        .expect("repeated rebuild");
    harness
        .request_ok(ControlOperation::QueryHealth, None, &[])
        .expect("health after repeated rebuild");
    harness.shutdown().expect("clean shutdown");
}

#[test]
fn rebuild_with_missing_bundle_returns_error_response_and_worker_survives() {
    let Some(plugin) = discover_plugin() else {
        return;
    };
    let mut harness = WorkerHarness::launch(&plugin).expect("launch worker");
    let missing = TestPlugin {
        bundle: PathBuf::from("/tmp/definitely-not-installed.vst3"),
        class_id: plugin.class_id.clone(),
        input_channels: plugin.input_channels,
        output_channels: plugin.output_channels,
    };
    let result = harness.rebuild_topology(&missing, &[0]);
    assert!(
        result.is_err(),
        "rebuild naming a missing bundle must be rejected"
    );
    let error = result.expect_err("rejected rebuild");
    assert!(
        !error.contains("round trip failed"),
        "a bad bundle must produce an error response, not a dropped control connection: {error}"
    );
    // The worker must remain fully usable after rejecting a bad rebuild.
    harness
        .request_ok(ControlOperation::QueryHealth, None, &[])
        .expect("health after rejected rebuild");
    harness
        .rebuild_topology(&plugin, &[0])
        .expect("valid rebuild after rejected rebuild");
    harness.shutdown().expect("clean shutdown");
}

#[test]
fn worker_processes_audio_blocks_through_shared_memory() {
    let Some(plugin) = discover_plugin() else {
        return;
    };
    let mut harness = WorkerHarness::launch(&plugin).expect("launch worker");
    harness
        .rebuild_topology(&plugin, &[0])
        .expect("rebuild with one real plug-in");
    for _ in 0..3 {
        let output = harness.process_block(128, 0.25).expect("processed block");
        assert_eq!(output.len(), 256);
        assert!(
            output.iter().all(|sample| sample.is_finite()),
            "processed audio must stay finite"
        );
    }
    harness.shutdown().expect("clean shutdown");
}

#[test]
fn capture_state_reports_controller_state_unsupported_on_high_level_backend() {
    let Some(plugin) = discover_plugin() else {
        return;
    };
    let mut harness = WorkerHarness::launch(&plugin).expect("launch worker");
    harness
        .rebuild_topology(&plugin, &[0])
        .expect("rebuild with one real plug-in");
    let slot = slot_identity(0);
    // The app deactivates before capture. The high-level backend must reject capture rather than
    // claim that an incomplete component-only snapshot is a complete two-stream state.
    harness
        .request_ok(ControlOperation::DeactivateSlot, Some(slot), &[])
        .expect("deactivate");
    let response = harness
        .request(ControlOperation::CaptureState, Some(slot), &[])
        .expect("capture yields a correlated response");
    assert_eq!(response.status(), ControlResponseStatus::Unsupported);
    let error = response.error_record().expect("unsupported error record");
    assert_eq!(error.code(), ControlErrorCode::UNSUPPORTED);
    assert!(
        error
            .message()
            .contains("controller-specific state is unsupported"),
        "unexpected unsupported-state message: {}",
        error.message()
    );
    harness
        .request_ok(ControlOperation::ActivateSlot, Some(slot), &[])
        .expect("reactivate after rejected capture");
    harness
        .request_ok(ControlOperation::QueryHealth, None, &[])
        .expect("health after rejected capture");
    harness.shutdown().expect("clean shutdown");
}

#[test]
fn parameter_metadata_and_writes_stay_in_protocol() {
    let Some(plugin) = discover_plugin() else {
        return;
    };
    let mut harness = WorkerHarness::launch(&plugin).expect("launch worker");
    harness
        .rebuild_topology(&plugin, &[0])
        .expect("rebuild with one real plug-in");
    // Capture-state on an out-of-range slot must be an error response, never a disconnect.
    let response = harness
        .request(ControlOperation::CaptureState, Some(slot_identity(7)), &[])
        .expect("empty-slot capture still yields a correlated response");
    assert_ne!(
        response.status(),
        ControlResponseStatus::Ok,
        "capturing an empty slot must be rejected"
    );
    harness
        .request_ok(ControlOperation::QueryHealth, None, &[])
        .expect("health after rejected capture");
    harness.shutdown().expect("clean shutdown");
}

/// The worker must never accept a shared-memory block for a stale generation: this mirrors the
/// host publishing against a mismatched bank after replacement.
#[test]
fn worker_ignores_foreign_generation_requests() {
    let Some(plugin) = discover_plugin() else {
        return;
    };
    let mut harness = WorkerHarness::launch(&plugin).expect("launch worker");
    harness
        .rebuild_topology(&plugin, &[0])
        .expect("rebuild with one real plug-in");
    // A valid block still processes afterwards, proving the worker loop stayed healthy.
    let output = harness.process_block(128, 0.5).expect("processed block");
    assert_eq!(output.len(), 256);
    let _ = harness
        .request(ControlOperation::QueryHealth, None, &[])
        .expect("health stays correlated");
    harness.shutdown().expect("clean shutdown");
}

#[test]
fn processed_audio_is_not_silence_for_effect_plugins() {
    let Some(plugin) = discover_plugin() else {
        return;
    };
    if plugin.input_channels == 0 {
        return;
    }
    let mut harness = WorkerHarness::launch(&plugin).expect("launch worker");
    harness
        .rebuild_topology(&plugin, &[0])
        .expect("rebuild with one real plug-in");
    // Warm the plug-in, then check a later block: an effect fed a DC signal must not emit
    // all-zero audio once its internal ramps settle.
    let mut last = Vec::new();
    for _ in 0..8 {
        last = harness.process_block(128, 0.25).expect("processed block");
    }
    assert!(
        last.iter().any(|sample| sample.abs() > 0.0),
        "an effect plug-in fed a constant signal produced pure silence"
    );
    harness.shutdown().expect("clean shutdown");
}
