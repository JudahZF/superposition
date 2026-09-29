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
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

use sp_protocol::{
    BlockRequest, BlockTicket, ProtocolError,
    control::{
        BankIdentity, ControlOperation, ControlResponse, ControlResponseStatus, ControlTarget,
        RackIdentity, SlotIdentity,
    },
    payload::{
        ControlPayloadCodec, EditorGeometry, EditorPosition, MAX_PARAMETER_BATCH_SIZE, ParameterId,
        ParameterIds, ParameterMetadata, ParameterValues, ParameterWrite, PluginSlotConfiguration,
        RackTopology,
    },
};
use sp_shared_memory_macos::{MonotonicClock, SharedMemoryRegion};
use sp_supervisor::{PluginCatalog, Scanner, WorkerControlClient};

const LAUNCH_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const BLOCK_TIMEOUT: Duration = Duration::from_secs(5);
const BANK_GENERATION: u64 = 1;
const RACK_GENERATION: u64 = 1;

/// Well-behaved free plug-ins used when no explicit test bundle is selected. Every candidate
/// is described in an isolated scanner helper, never in the test process.
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
    parameter_ids: Vec<u32>,
    writable_parameters: Vec<u32>,
}

fn plugin_directories() -> Vec<PathBuf> {
    let mut directories = vec![PathBuf::from("/Library/Audio/Plug-Ins/VST3")];
    if let Some(home) = std::env::var_os("HOME") {
        directories.push(PathBuf::from(home).join("Library/Audio/Plug-Ins/VST3"));
    }
    directories
}

/// Resolves the scanner beside the deployed worker unless a test deployment specifies it.
fn scanner_executable() -> Result<PathBuf, String> {
    let scanner = std::env::var_os("SUPERPOSITION_TEST_SCANNER").map_or_else(
        || Path::new(env!("CARGO_BIN_EXE_sp-plugin-worker")).with_file_name("sp-plugin-scanner"),
        PathBuf::from,
    );
    if !scanner.is_file() {
        return Err(format!(
            "scanner helper is missing at {}; build sp-plugin-scanner or set SUPERPOSITION_TEST_SCANNER",
            scanner.display()
        ));
    }
    Ok(scanner)
}

/// Scans in a disposable helper, then applies the fixed stereo rack contract to its SDK-free
/// metadata. Third-party code never enters the libtest process (which runs tests off main thread).
fn describe_candidate(bundle: &Path) -> Result<TestPlugin, String> {
    let catalog_path =
        std::env::temp_dir().join(format!("sp-e2e-catalog-{}.json", unique_suffix()));
    let catalog = PluginCatalog::open(&catalog_path).map_err(|error| error.to_string())?;
    let mut scanner = Scanner::new(scanner_executable()?, LAUNCH_TIMEOUT, catalog);
    let scan = scanner.scan(bundle).map_err(|error| error.to_string());
    let _ = std::fs::remove_file(&catalog_path);
    let scan = scan?;
    if !scan.is_supported() {
        return Err(format!(
            "isolated scan of {} returned {:?}: {}",
            bundle.display(),
            scan.outcome(),
            scan.metadata.detail.as_deref().unwrap_or("no detail")
        ));
    }
    let class =
        if let Ok(class_id) = std::env::var("SUPERPOSITION_TEST_VST3_CLASS") {
            scan.metadata
                .classes
                .into_iter()
                .find(|class| class.identity.unique_id == class_id)
                .ok_or_else(|| {
                    format!(
                        "selected VST3 class {class_id} not found in {}",
                        bundle.display()
                    )
                })?
        } else {
            scan.metadata.classes.into_iter().next().ok_or_else(|| {
                format!("isolated scan of {} returned no classes", bundle.display())
            })?
        };
    let inputs = class
        .buses
        .inputs
        .iter()
        .find(|bus| bus.main && !bus.event)
        .map_or(0, |bus| bus.channels);
    let outputs = class
        .buses
        .outputs
        .iter()
        .find(|bus| bus.main && !bus.event)
        .map_or(0, |bus| bus.channels);
    if outputs == 0 || outputs > 2 || inputs > 2 {
        return Err(format!(
            "class {} has unsupported main audio topology {inputs} in / {outputs} out",
            class.identity.unique_id
        ));
    }
    Ok(TestPlugin {
        bundle: bundle.to_path_buf(),
        class_id: class.identity.unique_id,
        input_channels: inputs,
        output_channels: outputs,
        parameter_ids: class
            .parameters
            .iter()
            .map(|parameter| parameter.id)
            .collect(),
        writable_parameters: class
            .parameters
            .iter()
            .filter(|parameter| parameter.automatable && !parameter.bypass && !parameter.read_only)
            .map(|parameter| parameter.id)
            .collect(),
    })
}

/// Discovery is computed once and shared so parallel tests do not launch duplicate scanners.
fn discover_plugin() -> Option<TestPlugin> {
    static DISCOVERED: std::sync::OnceLock<Option<TestPlugin>> = std::sync::OnceLock::new();
    DISCOVERED.get_or_init(discover_plugin_uncached).clone()
}

fn discover_plugin_uncached() -> Option<TestPlugin> {
    if let Ok(bundle) = std::env::var("SUPERPOSITION_TEST_VST3") {
        let bundle = PathBuf::from(bundle);
        return Some(
            describe_candidate(&bundle).unwrap_or_else(|error| {
                panic!("explicit test bundle {}: {error}", bundle.display())
            }),
        );
    }
    for directory in plugin_directories() {
        for name in KNOWN_SIMPLE_PLUGINS {
            let bundle = directory.join(name);
            if bundle.exists() {
                match describe_candidate(&bundle) {
                    Ok(plugin) => return Some(plugin),
                    Err(error) => eprintln!("skipping {}: {error}", bundle.display()),
                }
            }
        }
    }
    None
}

fn unique_suffix() -> String {
    static NEXT_SOCKET_ID: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        NEXT_SOCKET_ID.fetch_add(1, Ordering::Relaxed)
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

fn continuous_writable_parameter(
    harness: &mut WorkerHarness,
    plugin: &TestPlugin,
    slot: SlotIdentity,
) -> Result<Option<ParameterId>, String> {
    for &id in &plugin.writable_parameters {
        let id = ParameterId {
            value: u64::from(id),
        };
        let response = harness.request_ok(
            ControlOperation::ParameterMetadata,
            Some(slot),
            &id.encode().map_err(|error| error.to_string())?,
        )?;
        let metadata =
            ParameterMetadata::decode(response.payload()).map_err(|error| error.to_string())?;
        if metadata.step_count == 0 {
            return Ok(Some(id));
        }
    }
    Ok(None)
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
    client: WorkerControlClient,
    socket: PathBuf,
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
                let mut child = child;
                let _ = child.kill();
                let status = child.wait();
                let mut stderr = String::new();
                if let Some(output) = child.stderr.as_mut() {
                    let _ = output.read_to_string(&mut stderr);
                }
                let _ = std::fs::remove_file(&socket);
                return Err(format!("{error}; worker status {status:?}; {stderr}"));
            }
        };
        let mut harness = Self {
            child,
            region,
            client,
            socket,
        };
        let health = match harness.request(ControlOperation::QueryHealth, None, &[]) {
            Ok(health) => health,
            Err(error) => return Err(format!("{error}; {}", harness.drain_output())),
        };
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
        self.client
            .request(operation, slot, payload)
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
                    sidechain_active: false,
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
            sidechain_slots: 0,
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
                    return Err(format!(
                        "worker exited with {status}; {}",
                        self.drain_output()
                    ));
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

fn connect_with_retry(socket: &Path) -> Result<WorkerControlClient, String> {
    let deadline = Instant::now() + LAUNCH_TIMEOUT;
    loop {
        match WorkerControlClient::connect(socket, target(), REQUEST_TIMEOUT) {
            Ok(client) => return Ok(client),
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
        parameter_ids: plugin.parameter_ids.clone(),
        writable_parameters: plugin.writable_parameters.clone(),
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
#[allow(
    clippy::too_many_lines,
    reason = "complete state roundtrip keeps the worker lifecycle in one test"
)]
fn capture_and_restore_complete_state_through_worker_control() {
    let Some(plugin) = discover_plugin() else {
        return;
    };
    let mut harness = WorkerHarness::launch(&plugin).expect("launch worker");
    harness
        .rebuild_topology(&plugin, &[0])
        .expect("rebuild with one real plug-in");
    let slot = slot_identity(0);
    let parameter = continuous_writable_parameter(&mut harness, &plugin, slot)
        .expect("query writable parameter metadata");
    if let Some(id) = parameter {
        let write = ParameterWrite {
            id,
            normalized: 0.25,
        };
        harness
            .request_ok(
                ControlOperation::WriteParameter,
                Some(slot),
                &write.encode().expect("parameter payload"),
            )
            .expect("write parameter before capture");
        harness.process_block(128, 0.25).expect("apply parameter");
    }
    // The app deactivates before state capture and restore. vst3-host 0.9 puts both VST3
    // streams in a versioned envelope carried by the outer component field.
    harness
        .request_ok(ControlOperation::DeactivateSlot, Some(slot), &[])
        .expect("deactivate");
    let state = harness
        .client
        .capture_state(slot)
        .expect("capture complete state through chunked client");
    assert!(state.component.starts_with(b"VST3HOST_STATE\0\0"));
    assert!(state.controller.is_empty());
    let captured_parameter = parameter.map(|id| {
        let response = harness
            .request_ok(
                ControlOperation::ReadParameter,
                Some(slot),
                &id.encode().expect("parameter ID"),
            )
            .expect("read captured parameter");
        ParameterWrite::decode(response.payload())
            .expect("parameter value")
            .normalized
    });
    if let Some(id) = parameter {
        harness
            .request_ok(ControlOperation::ActivateSlot, Some(slot), &[])
            .expect("reactivate to change parameter");
        let write = ParameterWrite {
            id,
            normalized: 0.75,
        };
        harness
            .request_ok(
                ControlOperation::WriteParameter,
                Some(slot),
                &write.encode().expect("parameter payload"),
            )
            .expect("change parameter after capture");
        harness
            .process_block(128, 0.25)
            .expect("apply later parameter");
        let changed = harness
            .request_ok(
                ControlOperation::ReadParameter,
                Some(slot),
                &id.encode().expect("parameter ID"),
            )
            .expect("read changed parameter");
        let changed = ParameterWrite::decode(changed.payload())
            .expect("changed parameter value")
            .normalized;
        if let Some(captured) = captured_parameter {
            assert!(
                (changed - captured).abs() > 0.1,
                "parameter change must be observable before restore: captured {captured}, changed {changed}"
            );
        }
        harness
            .request_ok(ControlOperation::DeactivateSlot, Some(slot), &[])
            .expect("deactivate before restore");
    }
    harness
        .client
        .restore_state(slot, &state)
        .expect("restore complete state through chunked client");
    let restored = harness
        .client
        .capture_state(slot)
        .expect("capture after restore");
    assert!(restored.component.starts_with(b"VST3HOST_STATE\0\0"));
    harness
        .request_ok(ControlOperation::ActivateSlot, Some(slot), &[])
        .expect("reactivate after restore");
    if let (Some(id), Some(expected)) = (parameter, captured_parameter) {
        let response = harness
            .request_ok(
                ControlOperation::ReadParameter,
                Some(slot),
                &id.encode().expect("parameter ID"),
            )
            .expect("read restored parameter");
        let restored = ParameterWrite::decode(response.payload()).expect("parameter value");
        assert!(
            (restored.normalized - expected).abs() < 0.01,
            "opaque restore must recover the captured parameter: expected {expected}, got {}",
            restored.normalized
        );
    }
    harness
        .request_ok(ControlOperation::QueryHealth, None, &[])
        .expect("health after restore");
    let output = harness
        .process_block(128, 0.25)
        .expect("audio after restore");
    assert!(output.iter().all(|sample| sample.is_finite()));
    harness.shutdown().expect("clean shutdown");
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "opens a native editor window; run only with explicit approval"]
fn native_editor_opens_resizes_and_closes_without_stopping_worker() {
    let bundle = Path::new("/Library/Audio/Plug-Ins/VST3/ValhallaSupermassive.vst3");
    let plugin = describe_candidate(bundle).expect("ValhallaSupermassive test plug-in installed");
    let mut harness = WorkerHarness::launch(&plugin).expect("launch worker");
    harness
        .rebuild_topology(&plugin, &[0])
        .expect("rebuild with real plug-in");
    let slot = slot_identity(0);
    let position = EditorPosition {
        left: 120.0,
        top: 160.0,
    };
    harness
        .request_ok(
            ControlOperation::OpenNativeEditor,
            Some(slot),
            &position.encode().expect("valid editor position"),
        )
        .expect("attach native editor");
    // The first picture follows the editor's first paint.
    thread::sleep(Duration::from_secs(1));
    let opened = harness
        .client
        .capture_editor_preview(slot, 0)
        .expect("poll opened editor picture");
    assert!(opened.editor_open);
    let png = opened.png.expect("opened editor is pictured");
    assert_png_preview(&png);
    assert!(
        harness
            .client
            .capture_editor_preview(slot, opened.sequence)
            .expect("poll known picture")
            .png
            .is_none()
    );
    let geometry = EditorGeometry {
        width: 900,
        height: 600,
    };
    harness
        .request_ok(
            ControlOperation::ResizeNativeEditor,
            Some(slot),
            &geometry.encode().expect("valid editor geometry"),
        )
        .expect("resize native editor");
    harness
        .request_ok(ControlOperation::CloseNativeEditor, Some(slot), &[])
        .expect("detach native editor");
    let closed = harness
        .client
        .capture_editor_preview(slot, opened.sequence)
        .expect("poll closed editor picture");
    assert!(!closed.editor_open);
    assert!(
        closed.sequence > opened.sequence,
        "closing pictures the editor"
    );
    assert_png_preview(&closed.png.expect("closed editor is pictured"));
    harness
        .request_ok(ControlOperation::QueryHealth, None, &[])
        .expect("worker remains healthy after editor close");
    harness.shutdown().expect("clean shutdown");
}

/// Editor pictures are 320×200 PNGs.
fn assert_png_preview(png: &[u8]) {
    assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"), "PNG signature");
    assert_eq!(&png[12..16], b"IHDR");
    assert_eq!(png[16..20], 320_u32.to_be_bytes(), "picture width");
    assert_eq!(png[20..24], 200_u32.to_be_bytes(), "picture height");
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the parameter sweep and mutation share one worker lifecycle"
)]
fn parameter_metadata_and_writes_stay_in_protocol() {
    let Some(plugin) = discover_plugin() else {
        return;
    };
    let mut harness = WorkerHarness::launch(&plugin).expect("launch worker");
    harness
        .rebuild_topology(&plugin, &[0])
        .expect("rebuild with one real plug-in");
    let slot = slot_identity(0);
    let writable = continuous_writable_parameter(&mut harness, &plugin, slot)
        .expect("query writable parameter metadata")
        .expect("test plug-in needs a writable continuous parameter");
    let metadata = harness
        .request_ok(
            ControlOperation::ParameterMetadata,
            Some(slot),
            &writable.encode().expect("parameter ID"),
        )
        .expect("query writable parameter metadata");
    let metadata = ParameterMetadata::decode(metadata.payload()).expect("parameter metadata");
    assert_eq!(metadata.id, writable);
    assert_eq!(metadata.step_count, 0);

    let metadata_started = Instant::now();
    let mut slowest_metadata = Duration::ZERO;
    for &id in &plugin.parameter_ids {
        let parameter = ParameterId {
            value: u64::from(id),
        };
        let started = Instant::now();
        let response = harness
            .request_ok(
                ControlOperation::ParameterMetadata,
                Some(slot),
                &parameter.encode().expect("parameter ID"),
            )
            .unwrap_or_else(|error| panic!("parameter {id} metadata failed: {error}"));
        slowest_metadata = slowest_metadata.max(started.elapsed());
        let metadata = ParameterMetadata::decode(response.payload()).expect("parameter metadata");
        assert_eq!(metadata.id, parameter);
    }
    eprintln!(
        "parameter metadata sweep: bundle={} count={} elapsed={:?} slowest={slowest_metadata:?}",
        plugin.bundle.display(),
        plugin.parameter_ids.len(),
        metadata_started.elapsed()
    );

    let sweep_started = Instant::now();
    let mut slowest_read = Duration::ZERO;
    let mut individual_values = Vec::with_capacity(plugin.parameter_ids.len());
    for &id in &plugin.parameter_ids {
        let parameter = ParameterId {
            value: u64::from(id),
        };
        let started = Instant::now();
        let response = harness
            .request_ok(
                ControlOperation::ReadParameter,
                Some(slot),
                &parameter.encode().expect("parameter ID"),
            )
            .unwrap_or_else(|error| panic!("parameter {id} read failed: {error}"));
        slowest_read = slowest_read.max(started.elapsed());
        let read = ParameterWrite::decode(response.payload()).expect("parameter read payload");
        assert_eq!(read.id, parameter);
        assert!(
            read.normalized.is_finite() && (0.0..=1.0).contains(&read.normalized),
            "parameter {id} returned invalid normalized value {}",
            read.normalized
        );
        individual_values.push(read);
    }
    eprintln!(
        "parameter read sweep: bundle={} count={} elapsed={:?} slowest={slowest_read:?}",
        plugin.bundle.display(),
        plugin.parameter_ids.len(),
        sweep_started.elapsed()
    );

    let batch_started = Instant::now();
    let mut slowest_batch = Duration::ZERO;
    for (batch_index, ids) in plugin
        .parameter_ids
        .chunks(MAX_PARAMETER_BATCH_SIZE)
        .enumerate()
    {
        let request = ParameterIds {
            parameters: ids
                .iter()
                .copied()
                .map(|id| ParameterId {
                    value: u64::from(id),
                })
                .collect(),
        };
        let started = Instant::now();
        let response = harness
            .request_ok(
                ControlOperation::ReadParameters,
                Some(slot),
                &request.encode().expect("parameter batch"),
            )
            .unwrap_or_else(|error| panic!("parameter batch {batch_index} failed: {error}"));
        slowest_batch = slowest_batch.max(started.elapsed());
        let values = ParameterValues::decode(response.payload()).expect("parameter batch values");
        assert_eq!(values.parameters.len(), ids.len());
        for (index, value) in values.parameters.iter().enumerate() {
            let expected = individual_values[batch_index * MAX_PARAMETER_BATCH_SIZE + index];
            assert_eq!(value.id, expected.id);
            assert!(
                (value.normalized - expected.normalized).abs() < 0.01,
                "batched value for parameter {} diverged from individual read: {} vs {}",
                value.id.value,
                value.normalized,
                expected.normalized
            );
        }
    }
    eprintln!(
        "parameter batch sweep: bundle={} count={} batches={} elapsed={:?} slowest={slowest_batch:?}",
        plugin.bundle.display(),
        plugin.parameter_ids.len(),
        plugin
            .parameter_ids
            .len()
            .div_ceil(MAX_PARAMETER_BATCH_SIZE),
        batch_started.elapsed()
    );

    for target in [0.25, 0.75] {
        let write = ParameterWrite {
            id: writable,
            normalized: target,
        };
        harness
            .request_ok(
                ControlOperation::WriteParameter,
                Some(slot),
                &write.encode().expect("parameter write"),
            )
            .expect("write normalized parameter");
        harness
            .process_block(128, 0.25)
            .expect("apply normalized parameter in DSP block");
        let response = harness
            .request_ok(
                ControlOperation::ReadParameter,
                Some(slot),
                &writable.encode().expect("parameter ID"),
            )
            .expect("read written parameter");
        let read = ParameterWrite::decode(response.payload()).expect("parameter read payload");
        assert_eq!(read.id, writable);
        assert!(
            (read.normalized - target).abs() < 0.02,
            "controller value did not reflect write {target}: {}",
            read.normalized
        );
        let response = harness
            .request_ok(
                ControlOperation::ParameterMetadata,
                Some(slot),
                &writable.encode().expect("parameter ID"),
            )
            .expect("metadata after parameter write");
        let metadata = ParameterMetadata::decode(response.payload()).expect("parameter metadata");
        assert_eq!(metadata.id, writable);
        assert!(
            (metadata.normalized - read.normalized).abs() < 0.01,
            "metadata and value queries diverged after write"
        );
    }

    let unknown = (0..=u32::MAX)
        .find(|id| !plugin.parameter_ids.contains(id))
        .expect("a plug-in cannot define every u32 parameter ID");
    let unknown = ParameterId {
        value: u64::from(unknown),
    };
    for operation in [
        ControlOperation::ParameterMetadata,
        ControlOperation::ReadParameter,
    ] {
        let response = harness
            .request(
                operation,
                Some(slot),
                &unknown.encode().expect("unknown parameter ID"),
            )
            .expect("unknown ID still gets a correlated response");
        assert_eq!(response.status(), ControlResponseStatus::Failed);
        assert!(
            response
                .error_record()
                .is_some_and(|error| error.message().contains("unknown VST3 parameter")),
            "{operation:?} did not report an unknown parameter ID"
        );
    }
    let response = harness
        .request(
            ControlOperation::ReadParameters,
            Some(slot),
            &ParameterIds {
                parameters: vec![writable, unknown],
            }
            .encode()
            .expect("parameter batch with unknown ID"),
        )
        .expect("unknown batch ID still gets a correlated response");
    assert_eq!(response.status(), ControlResponseStatus::Failed);
    assert!(
        response
            .error_record()
            .is_some_and(|error| error.message().contains("unknown VST3 parameter")),
        "batched read did not reject an unknown parameter ID"
    );
    let beyond_vst3_range = ParameterId {
        value: 0x1_0000_0000,
    };
    for operation in [
        ControlOperation::ParameterMetadata,
        ControlOperation::ReadParameter,
    ] {
        let response = harness
            .request(
                operation,
                Some(slot),
                &beyond_vst3_range
                    .encode()
                    .expect("out-of-range parameter ID"),
            )
            .expect("out-of-range ID still gets a correlated response");
        assert_eq!(response.status(), ControlResponseStatus::Failed);
        assert!(
            response
                .error_record()
                .is_some_and(|error| error.message().contains("exceeds VST3 u32 range")),
            "{operation:?} did not reject an ID outside the VST3 range"
        );
    }
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
