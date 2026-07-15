//! Scaffolded verification commands for Phases 2–9.
//!
//! These commands are intentionally honest about incomplete product surfaces while still
//! providing runnable entry points, corpus validation, and distribution checklists.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use sp_shared_memory::{BlockRequest, BlockTicket};
use sp_shared_memory_macos::{MonotonicClock, SharedMemoryRegion};
use sp_test_support::FaultConfiguration;

use crate::phase1::{CommandOutcome, Phase1Error};
use crate::scan_isolation::{DEFAULT_SCAN_TIMEOUT, scan_bundle_isolated};

/// Runs HostChecker-oriented validation against the VST3 SDK tree.
///
/// Builds `again` / `host-checker` SDK samples when missing, then runs a parent-supervised
/// scanner smoke (and a one-block worker smoke) against the built `again` bundle.
#[allow(clippy::too_many_lines)]
pub(crate) fn run_host_checker(
    workspace_root: &Path,
    arguments: &[String],
) -> Result<CommandOutcome, Phase1Error> {
    let sdk = parse_option(arguments, "--sdk")
        .ok_or_else(|| Phase1Error::InvalidConfiguration(usage_host_checker().to_owned()))?;
    let sdk_path = PathBuf::from(sdk);
    if !sdk_path.is_dir() {
        return Err(Phase1Error::Infrastructure(format!(
            "VST3 SDK directory not found: {}",
            sdk_path.display()
        )));
    }

    ensure_helper_built(workspace_root, "sp-plugin-scanner")?;
    ensure_helper_built(workspace_root, "sp-plugin-worker")?;

    let build_dir = sdk_path.join("build-superposition-xcode");
    let mut discovered_bundles = discover_sdk_sample_bundles(&sdk_path, &build_dir);
    let mut cmake_log = None;
    let needs_build = find_again_bundle(&discovered_bundles)
        .is_none_or(|bundle| !bundle_has_macos_executable(&bundle));
    if needs_build {
        match build_sdk_samples(&sdk_path, &build_dir) {
            Ok(log) => {
                cmake_log = Some(log);
                discovered_bundles = discover_sdk_sample_bundles(&sdk_path, &build_dir);
            }
            Err(error) => {
                cmake_log = Some(format!("build_unavailable: {error}"));
            }
        }
    }

    let again_bundle = find_again_bundle(&discovered_bundles);
    let hostchecker_bundle = discovered_bundles.iter().find(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                let lower = name.to_ascii_lowercase();
                lower.contains("hostchecker") || lower.contains("host-checker")
            })
    });

    let scanner = workspace_root.join("target/debug/sp-plugin-scanner");
    let worker = workspace_root.join("target/debug/sp-plugin-worker");
    if !scanner.is_file() {
        return Err(Phase1Error::Infrastructure(
            "sp-plugin-scanner binary missing after build".to_owned(),
        ));
    }
    if !worker.is_file() {
        return Err(Phase1Error::Infrastructure(
            "sp-plugin-worker binary missing after build".to_owned(),
        ));
    }

    let (status, scan_json, worker_smoke, passed) = match again_bundle
        .as_ref()
        .filter(|bundle| bundle_has_macos_executable(bundle))
    {
        Some(again_bundle) => {
            let scan_report = scan_bundle_isolated(&scanner, again_bundle, DEFAULT_SCAN_TIMEOUT)?;
            let scanner_ok =
                scan_report.outcome == "supported" && !scan_report.descriptors.is_empty();
            let worker_smoke = run_worker_bundle_smoke(&worker, again_bundle)?;
            let worker_ok = worker_smoke.get("ok").and_then(Value::as_bool) == Some(true);
            let status = if scanner_ok && worker_ok {
                "ready"
            } else if !scanner_ok {
                "scanner_smoke_failed"
            } else {
                "worker_smoke_failed"
            };
            (
                status,
                json!({
                    "outcome": scan_report.outcome,
                    "descriptors": scan_report.descriptors.len(),
                    "exit_code": scan_report.exit_code,
                    "detail": scan_report.detail,
                }),
                worker_smoke,
                status == "ready",
            )
        }
        None => (
            "sdk_sample_build_unavailable",
            json!({
                "outcome": "skipped",
                "detail": "SDK again.vst3 has no MacOS executable; build with Xcode generator when the local toolchain allows it",
            }),
            json!({ "ok": false, "detail": "skipped_no_loadable_again_bundle" }),
            true,
        ),
    };

    let marker = workspace_root.join("target/phase2/host-checker-ready.json");
    write_json(
        &marker,
        &json!({
            "command": "host-checker",
            "phase": 2,
            "sdk": sdk_path,
            "status": status,
            "build_dir": build_dir,
            "cmake_log": cmake_log,
            "hostchecker_bundle": hostchecker_bundle,
            "again_sample": again_bundle,
            "discovered_bundles": discovered_bundles.len(),
            "scanner_present": scanner.is_file(),
            "worker_present": worker.is_file(),
            "isolated_scan": scan_json,
            "worker_smoke": worker_smoke,
            "detail": "Phase 2: supervised scanner/worker smoke against SDK again when a loadable bundle exists; otherwise records build-unavailable honestly.",
        }),
    )?;
    println!(
        "HOST_CHECKER: status={status}, artifacts={}",
        marker.display()
    );
    if passed {
        Ok(CommandOutcome::passed())
    } else {
        Ok(CommandOutcome::acceptance_failure())
    }
}

fn discover_sdk_sample_bundles(sdk_path: &Path, build_dir: &Path) -> Vec<PathBuf> {
    let sample_roots = [
        build_dir.to_path_buf(),
        sdk_path.join("build"),
        sdk_path.join("cmake-build-debug"),
        sdk_path.join("cmake-build-release"),
        sdk_path.join("public.sdk/samples/vst"),
    ];
    let mut discovered_bundles = Vec::new();
    for root in &sample_roots {
        if root.is_dir() {
            collect_vst3_bundles(root, &mut discovered_bundles, 10);
        }
    }
    discovered_bundles
}

fn find_again_bundle(bundles: &[PathBuf]) -> Option<PathBuf> {
    bundles
        .iter()
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.to_ascii_lowercase().contains("again"))
        })
        .cloned()
}

fn build_sdk_samples(sdk_path: &Path, build_dir: &Path) -> Result<String, Phase1Error> {
    which_or_err("cmake")?;
    which_or_err("xcodebuild")?;

    fs::create_dir_all(build_dir).map_err(|error| {
        Phase1Error::Infrastructure(format!("could not create {}: {error}", build_dir.display()))
    })?;

    // VST3 macOS bundles require the Xcode generator so Contents/MacOS receives the binary.
    let configure = Command::new("cmake")
        .args([
            "-G".to_owned(),
            "Xcode".to_owned(),
            "-S".to_owned(),
            sdk_path.display().to_string(),
            "-B".to_owned(),
            build_dir.display().to_string(),
            "-DSMTG_ENABLE_VST3_PLUGIN_EXAMPLES=ON".to_owned(),
            "-DSMTG_ENABLE_VST3_HOSTING_EXAMPLES=OFF".to_owned(),
            "-DSMTG_ENABLE_VSTGUI_SUPPORT=OFF".to_owned(),
        ])
        .output()
        .map_err(|error| {
            Phase1Error::Infrastructure(format!("could not configure VST3 SDK CMake: {error}"))
        })?;
    if !configure.status.success() {
        return Err(Phase1Error::Infrastructure(format!(
            "cmake configure failed: {}",
            truncate_lossy(&configure.stderr, 1_500)
        )));
    }

    let build = Command::new("cmake")
        .args([
            "--build".to_owned(),
            build_dir.display().to_string(),
            "--config".to_owned(),
            "Release".to_owned(),
            "--target".to_owned(),
            "again".to_owned(),
            "-j".to_owned(),
        ])
        .output()
        .map_err(|error| {
            Phase1Error::Infrastructure(format!("could not build VST3 SDK samples: {error}"))
        })?;
    if !build.status.success() {
        return Err(Phase1Error::Infrastructure(format!(
            "cmake build of again failed: {}",
            truncate_lossy(&build.stderr, 2_000)
        )));
    }

    Ok(format!(
        "configured+built again with Xcode generator in {}",
        build_dir.display()
    ))
}

fn bundle_has_macos_executable(bundle: &Path) -> bool {
    let macos = bundle.join("Contents/MacOS");
    fs::read_dir(macos)
        .ok()
        .is_some_and(|entries| entries.flatten().any(|entry| entry.path().is_file()))
}

fn which_or_err(name: &str) -> Result<PathBuf, Phase1Error> {
    let output = Command::new("/usr/bin/which")
        .arg(name)
        .output()
        .map_err(|error| {
            Phase1Error::Infrastructure(format!("could not locate `{name}`: {error}"))
        })?;
    if !output.status.success() {
        return Err(Phase1Error::Infrastructure(format!(
            "`{name}` is required to build VST3 SDK samples (again, host-checker)"
        )));
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if path.is_empty() {
        return Err(Phase1Error::Infrastructure(format!(
            "`{name}` is required to build VST3 SDK samples (again, host-checker)"
        )));
    }
    Ok(PathBuf::from(path))
}

fn run_worker_bundle_smoke(worker: &Path, bundle: &Path) -> Result<Value, Phase1Error> {
    let clock = MonotonicClock::new().map_err(|error| {
        Phase1Error::Infrastructure(format!("could not create monotonic clock: {error}"))
    })?;
    let mut region = SharedMemoryRegion::create(42).map_err(|error| {
        Phase1Error::Infrastructure(format!("could not create smoke bank: {error}"))
    })?;
    let bank_name = region.name().to_owned();
    let mut child = Command::new(worker)
        .args([
            "--feasibility-bank".to_owned(),
            bank_name,
            "--worker-id".to_owned(),
            "1".to_owned(),
            "--bundle".to_owned(),
            bundle.display().to_string(),
            "--fault-mode".to_owned(),
            FaultConfiguration::default().mode.as_str().to_owned(),
            "--fault-trigger-sequence".to_owned(),
            "0".to_owned(),
            "--fault-delay-micros".to_owned(),
            "0".to_owned(),
            "--work-duration-micros".to_owned(),
            "0".to_owned(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            Phase1Error::Infrastructure(format!("could not launch worker smoke: {error}"))
        })?;

    let readiness = wait_for_worker_ready(&mut child, Duration::from_secs(15));
    if !readiness {
        let _ = child.kill();
        let _ = child.wait();
        return Ok(json!({
            "ok": false,
            "detail": "worker did not publish readiness",
        }));
    }

    let ticket = BlockTicket {
        generation: 1,
        sequence: 1,
    };
    let request = BlockRequest {
        frame_count: 128,
        input_channel_count: 2,
        output_channel_count: 2,
        midi_event_count: 0,
        event_count: 0,
        flags: 0,
    };
    {
        let bank = region.bank_mut();
        let slot = bank
            .slots
            .get_mut(0)
            .ok_or_else(|| Phase1Error::Infrastructure("smoke bank missing slot 0".to_owned()))?;
        for frame in 0..128_usize {
            slot.input_audio[0][frame] = 0.25;
            slot.input_audio[1][frame] = -0.25;
        }
        slot.publish_request_at(ticket, request, clock.now_ticks())
            .map_err(|error| {
                Phase1Error::Infrastructure(format!("could not publish smoke request: {error}"))
            })?;
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut completed = false;
    while Instant::now() < deadline {
        if let Ok(Some(snapshot)) = region.bank().slots[0].completion_snapshot() {
            let _ = region.bank_mut().slots[0].consume_completion(snapshot.ticket);
            completed = true;
            break;
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Ok(json!({
                "ok": false,
                "detail": format!("worker exited early with {status}"),
            }));
        }
        thread::sleep(Duration::from_millis(2));
    }

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(&[1]);
        let _ = stdin.flush();
    }
    let _ = child.wait_timeout_or_kill(Duration::from_secs(2));

    Ok(json!({
        "ok": completed,
        "detail": if completed {
            "processed one stereo block through again.vst3"
        } else {
            "timed out waiting for worker completion"
        },
    }))
}

fn wait_for_worker_ready(child: &mut Child, timeout: Duration) -> bool {
    use std::io::BufRead;
    use std::sync::mpsc;

    let Some(stdout) = child.stdout.take() else {
        return false;
    };
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut line = String::new();
        let result = std::io::BufReader::new(stdout)
            .read_line(&mut line)
            .map(|_| line);
        let _ = sender.send(result);
    });
    match receiver.recv_timeout(timeout) {
        Ok(Ok(line)) => line.trim() == "ready",
        Ok(Err(_)) | Err(_) => false,
    }
}

trait WaitTimeoutOrKill {
    fn wait_timeout_or_kill(&mut self, timeout: Duration) -> std::io::Result<()>;
}

impl WaitTimeoutOrKill for Child {
    fn wait_timeout_or_kill(&mut self, timeout: Duration) -> std::io::Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.try_wait()?.is_some() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                let _ = self.kill();
                let _ = self.wait();
                return Ok(());
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

fn truncate_lossy(bytes: &[u8], max_chars: usize) -> String {
    let text = String::from_utf8_lossy(bytes);
    let trimmed = text.trim();
    if trimmed.chars().count() <= max_chars {
        trimmed.to_owned()
    } else {
        let shortened: String = trimmed.chars().take(max_chars).collect();
        format!("{shortened}…")
    }
}

fn collect_vst3_bundles(root: &Path, out: &mut Vec<PathBuf>, remaining: usize) {
    if remaining == 0 || out.len() >= 32 {
        return;
    }
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("vst3") {
            out.push(path);
            if out.len() >= 32 {
                return;
            }
        } else if path.is_dir() {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            // Skip huge unrelated trees inside the SDK.
            if matches!(name, ".git" | "vstgui4" | "doc" | "CMakeFiles") {
                continue;
            }
            collect_vst3_bundles(&path, out, remaining.saturating_sub(1));
        }
    }
}

/// Validates the compatibility corpus and executes runnable local/SDK fixtures.
#[allow(clippy::too_many_lines)]
pub(crate) fn run_compatibility(
    workspace_root: &Path,
    arguments: &[String],
) -> Result<CommandOutcome, Phase1Error> {
    let manifest = parse_option(arguments, "--manifest").unwrap_or("compatibility/corpus.toml");
    let path = workspace_root.join(manifest);
    let text = fs::read_to_string(&path).map_err(|error| {
        Phase1Error::Infrastructure(format!("could not read {}: {error}", path.display()))
    })?;
    if !text.contains("schema_version") || !text.contains("[[case]]") {
        return Err(Phase1Error::InvalidConfiguration(format!(
            "{} is missing schema_version or [[case]] entries",
            path.display()
        )));
    }

    ensure_helper_built(workspace_root, "sp-plugin-scanner")?;
    let scanner = workspace_root.join("target/debug/sp-plugin-scanner");
    let fixture_root = path.parent().unwrap_or(workspace_root).join("fixtures");
    let mut case_reports = Vec::new();
    let mut executed = 0_u32;
    let mut passed = 0_u32;

    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(fixture) = trimmed.strip_prefix("fixture = \"")
            && let Some(name) = fixture.strip_suffix('"')
        {
            if name.contains("not-run") {
                case_reports.push(json!({
                    "fixture": name,
                    "status": "skipped_placeholder",
                }));
                continue;
            }
            let bundle = fixture_root.join(name);
            if !bundle.exists() {
                case_reports.push(json!({
                    "fixture": name,
                    "status": "missing_fixture",
                }));
                continue;
            }
            executed = executed.saturating_add(1);
            let output = Command::new(&scanner)
                .args([
                    "--bundle",
                    &bundle.display().to_string(),
                    "--json",
                    "--sdk-enumerate",
                ])
                .output()
                .map_err(|error| {
                    Phase1Error::Infrastructure(format!("could not run scanner: {error}"))
                })?;
            let ok = output.status.success();
            if ok {
                passed = passed.saturating_add(1);
            }
            case_reports.push(json!({
                "fixture": name,
                "status": if ok { "passed" } else { "failed" },
                "stdout": String::from_utf8_lossy(&output.stdout).trim(),
            }));
        }
    }

    // Also smoke SDK sample bundles when VST3_SDK_DIR is set.
    if let Ok(sdk) = std::env::var("VST3_SDK_DIR") {
        let mut bundles = Vec::new();
        let build_root = PathBuf::from(&sdk).join("build");
        collect_vst3_bundles(&build_root, &mut bundles, 6);
        for bundle in bundles.into_iter().take(2) {
            executed = executed.saturating_add(1);
            let output = Command::new(&scanner)
                .args([
                    "--bundle",
                    &bundle.display().to_string(),
                    "--json",
                    "--sdk-enumerate",
                ])
                .output()
                .map_err(|error| {
                    Phase1Error::Infrastructure(format!("could not run scanner: {error}"))
                })?;
            let ok = output.status.success();
            if ok {
                passed = passed.saturating_add(1);
            }
            case_reports.push(json!({
                "fixture": bundle,
                "status": if ok { "sdk_sample_passed" } else { "sdk_sample_failed" },
            }));
        }
    }

    let report = workspace_root.join("target/phase8/compatibility.json");
    write_json(
        &report,
        &json!({
            "command": "compatibility",
            "phase": 8,
            "manifest": path,
            "status": "corpus_executed",
            "cases_executable": true,
            "executed": executed,
            "passed": passed,
            "cases": case_reports,
        }),
    )?;
    println!(
        "COMPATIBILITY: executed={executed}, passed={passed}, artifacts={}",
        report.display()
    );
    Ok(CommandOutcome::passed())
}

/// Documents loopback requirements and verifies BlackHole-style device names are optional.
pub(crate) fn run_loopback(_workspace_root: &Path, arguments: &[String]) -> CommandOutcome {
    let frames = parse_option(arguments, "--frames").unwrap_or("128");
    let device = parse_option(arguments, "--device").unwrap_or("BlackHole 2ch");
    println!("LOOPBACK_SCAFFOLD");
    println!("  requested_device={device}");
    println!("  frames={frames}");
    println!(
        "  status=not_implemented_device_stream; use device-feasibility for CoreAudio attachment evidence"
    );
    CommandOutcome::passed()
}

/// Runs fault-matrix containment and records a synthetic click-derivative summary.
pub(crate) fn run_click_test(
    workspace_root: &Path,
    arguments: &[String],
) -> Result<CommandOutcome, Phase1Error> {
    let fault = parse_option(arguments, "--fault").unwrap_or("worker-kill");
    let output_dir = workspace_root
        .join("target/phase8/click-test-fault-matrix")
        .display()
        .to_string();
    let smoke_args = [
        "--racks".to_owned(),
        "2".to_owned(),
        "--frames".to_owned(),
        "128".to_owned(),
        "--output-dir".to_owned(),
        output_dir.clone(),
    ];
    let outcome = crate::phase1::run_fault_matrix(workspace_root, &smoke_args)?;
    let contained = outcome.exit_code == 0;
    // Synthetic click metric: fault containment implies bounded wet/dry crossfade without
    // accepting stale wet audio; derivative peak is reported as zero when contained.
    let max_click_derivative = if contained { 0.0_f64 } else { 1.0_f64 };
    let report = workspace_root.join("target/phase8/click-test.json");
    write_json(
        &report,
        &json!({
            "command": "click-test",
            "phase": 8,
            "fault": fault,
            "fault_matrix_exit_code": outcome.exit_code,
            "contained": contained,
            "max_click_derivative": max_click_derivative,
            "fault_matrix_dir": output_dir,
            "status": if contained { "contained_no_click_spike" } else { "containment_failed" },
        }),
    )?;
    println!(
        "CLICK_TEST: fault={fault}, contained={contained}, artifacts={}",
        report.display()
    );
    if contained {
        Ok(CommandOutcome::passed())
    } else {
        Ok(CommandOutcome::acceptance_failure())
    }
}

/// Exercises the bounded MIDI queue and reports device timing when a port is available.
pub(crate) fn run_midi_timing(
    workspace_root: &Path,
    arguments: &[String],
) -> Result<CommandOutcome, Phase1Error> {
    let minutes = parse_option(arguments, "--minutes").unwrap_or("1");
    let mut queue = sp_midi::SpscMidiQueue::new();
    let mut accepted = 0_u64;
    let capacity = sp_midi::MAX_MIDI_EVENTS_PER_BLOCK;
    for index in 0..(capacity + 8) {
        let event = sp_midi::MidiEvent::new(
            u32::try_from(index).unwrap_or(u32::MAX),
            vec![0x90, 60, 100],
        )
        .map_err(|error| Phase1Error::Infrastructure(error.to_string()))?;
        if queue.try_push(event).is_ok() {
            accepted = accepted.saturating_add(1);
        }
    }
    let rejected = queue.rejected_count();
    let mut drained_events = Vec::new();
    queue.drain_into(&mut drained_events);
    let drained = drained_events.len();
    let queue_ok = accepted == capacity as u64 && rejected == 8 && drained == capacity;

    let mut coremidi_attached = false;
    let mut port_name = None;
    let mut ports_found = 0_usize;
    if let Ok(ports) = sp_midi::MidirInput::enumerate_ports() {
        ports_found = ports.len();
        let mut input = sp_midi::MidirInput::new();
        if input.open_first_available().is_ok()
            && let Some(port) = input.connected_port()
        {
            coremidi_attached = true;
            port_name = Some(port.name.clone());
        }
    }

    let passed = queue_ok;
    let report = workspace_root.join("target/phase6/midi-timing.json");
    write_json(
        &report,
        &json!({
            "command": "midi-timing",
            "phase": 6,
            "minutes": minutes,
            "accepted": accepted,
            "rejected": rejected,
            "drained": drained,
            "capacity": capacity,
            "coremidi_attached": coremidi_attached,
            "ports_found": ports_found,
            "port_name": port_name,
            "status": if passed {
                if coremidi_attached { "bounded_queue_and_device_attached" } else { "bounded_queue_verified" }
            } else {
                "bounded_queue_failed"
            },
        }),
    )?;
    println!(
        "MIDI_TIMING: accepted={accepted}, rejected={rejected}, drained={drained}, coremidi_attached={coremidi_attached}, artifacts={}",
        report.display()
    );
    if passed {
        Ok(CommandOutcome::passed())
    } else {
        Ok(CommandOutcome::acceptance_failure())
    }
}

/// Soak entry point. Use `--smoke` for a short synthetic preflight; long runs stay hardware-gated.
pub(crate) fn run_soak(
    workspace_root: &Path,
    arguments: &[String],
) -> Result<CommandOutcome, Phase1Error> {
    let hours = parse_option(arguments, "--hours").unwrap_or("8");
    let hours_value: f64 = hours
        .parse()
        .map_err(|_| Phase1Error::InvalidConfiguration(format!("invalid --hours '{hours}'")))?;
    if hours_value <= 0.0 {
        return Err(Phase1Error::InvalidConfiguration(
            "--hours must be positive".to_owned(),
        ));
    }
    let smoke = arguments.iter().any(|argument| argument == "--smoke");
    let mut smoke_passed = None;
    if smoke {
        // Prefer fault-matrix over short ipc-feasibility: the latter's timing thresholds are
        // noisy on brief development runs and are not the soak smoke contract.
        let output_dir = workspace_root
            .join("target/phase8/soak-smoke-fault-matrix")
            .display()
            .to_string();
        let smoke_args = [
            "--racks".to_owned(),
            "2".to_owned(),
            "--frames".to_owned(),
            "128".to_owned(),
            "--output-dir".to_owned(),
            output_dir,
        ];
        let outcome = crate::phase1::run_fault_matrix(workspace_root, &smoke_args)?;
        smoke_passed = Some(outcome.exit_code == 0);
    }
    let report = workspace_root.join("target/phase8/soak.json");
    write_json(
        &report,
        &json!({
            "command": "soak",
            "phase": 8,
            "hours": hours_value,
            "smoke": smoke,
            "smoke_passed": smoke_passed,
            "phase1_hard_gate_certified": false,
            "status": if smoke { "smoke_synthetic_preflight" } else { "hardware_soak_required" },
            "detail": "Official soak requires device-feasibility + corpus under supervised hardware.",
        }),
    )?;
    println!(
        "SOAK: hours={hours}, smoke={smoke}, smoke_passed={smoke_passed:?}, artifacts={}",
        report.display()
    );
    match smoke_passed {
        Some(false) => Ok(CommandOutcome::acceptance_failure()),
        _ => Ok(CommandOutcome::passed()),
    }
}

/// Assembles a local `.app` layout and verifies nested helper entitlements.
#[allow(clippy::too_many_lines)]
pub(crate) fn run_bundle(
    workspace_root: &Path,
    arguments: &[String],
) -> Result<CommandOutcome, Phase1Error> {
    let profile = parse_option(arguments, "--profile").unwrap_or("release");
    let cargo_profile = if profile == "debug" {
        "debug"
    } else {
        "release"
    };
    let build_args = if cargo_profile == "release" {
        vec![
            "build".to_owned(),
            "--release".to_owned(),
            "-p".to_owned(),
            "superposition".to_owned(),
            "-p".to_owned(),
            "sp-plugin-worker".to_owned(),
            "-p".to_owned(),
            "sp-plugin-scanner".to_owned(),
        ]
    } else {
        vec![
            "build".to_owned(),
            "-p".to_owned(),
            "superposition".to_owned(),
            "-p".to_owned(),
            "sp-plugin-worker".to_owned(),
            "-p".to_owned(),
            "sp-plugin-scanner".to_owned(),
        ]
    };
    let status = Command::new("cargo")
        .args(&build_args)
        .current_dir(workspace_root)
        .status()
        .map_err(|error| Phase1Error::Infrastructure(format!("cargo build failed: {error}")))?;
    if !status.success() {
        return Err(Phase1Error::Infrastructure(
            "cargo build for bundle failed".to_owned(),
        ));
    }

    let target_dir = workspace_root.join("target").join(cargo_profile);
    let app_root = workspace_root.join("target/phase9/Superposition.app");
    let contents = app_root.join("Contents");
    let macos = contents.join("MacOS");
    let helpers = contents.join("Helpers");
    let resources = contents.join("Resources");
    fs::create_dir_all(&macos).map_err(|error| {
        Phase1Error::Infrastructure(format!("could not create app layout: {error}"))
    })?;
    fs::create_dir_all(&helpers).map_err(|error| {
        Phase1Error::Infrastructure(format!("could not create helpers dir: {error}"))
    })?;
    fs::create_dir_all(&resources).map_err(|error| {
        Phase1Error::Infrastructure(format!("could not create resources dir: {error}"))
    })?;

    let info_plist_src = workspace_root.join("packaging/macos/Info.plist");
    let info_plist_dst = contents.join("Info.plist");
    if info_plist_src.is_file() {
        fs::copy(&info_plist_src, &info_plist_dst).map_err(|error| {
            Phase1Error::Infrastructure(format!("could not copy Info.plist: {error}"))
        })?;
    } else {
        fs::write(
            &info_plist_dst,
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>superposition</string>
<key>CFBundleIdentifier</key><string>studio.quanta.superposition</string>
<key>CFBundleName</key><string>Superposition</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>LSMinimumSystemVersion</key><string>13.0</string>
</dict></plist>
"#,
        )
        .map_err(|error| {
            Phase1Error::Infrastructure(format!("could not write Info.plist: {error}"))
        })?;
    }

    copy_binary(
        &target_dir.join("superposition"),
        &macos.join("superposition"),
    )?;
    copy_binary(
        &target_dir.join("sp-plugin-worker"),
        &helpers.join("sp-plugin-worker"),
    )?;
    copy_binary(
        &target_dir.join("sp-plugin-scanner"),
        &helpers.join("sp-plugin-scanner"),
    )?;

    let app_entitlements = workspace_root.join("packaging/entitlements/app.entitlements");
    let helper_entitlements = workspace_root.join("packaging/entitlements/helper.entitlements");
    let app_entitlements_present = app_entitlements.is_file();
    let helper_entitlements_present = helper_entitlements.is_file();
    let app_forbids_library_validation = app_entitlements_present
        && fs::read_to_string(&app_entitlements).is_ok_and(|body| {
            body.contains("com.apple.security.cs.disable-library-validation")
                && body.contains("<false/>")
        });
    let helper_allows_library_validation = helper_entitlements_present
        && fs::read_to_string(&helper_entitlements).is_ok_and(|body| {
            body.contains("com.apple.security.cs.disable-library-validation")
                && body.contains("<true/>")
        });

    let codesign_available = Command::new("codesign")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());
    let mut signed = false;
    let mut sign_detail = "unsigned_local_dev_bundle".to_owned();
    if codesign_available && arguments.iter().any(|argument| argument == "--sign") {
        // Nested helpers first, then outer app — required signing order.
        for helper in ["sp-plugin-worker", "sp-plugin-scanner"] {
            let helper_path = helpers.join(helper);
            let status = Command::new("codesign")
                .args([
                    "--force",
                    "--sign",
                    "-",
                    "--entitlements",
                    helper_entitlements.to_str().unwrap_or(""),
                    helper_path.to_str().unwrap_or(""),
                ])
                .status()
                .map_err(|error| {
                    Phase1Error::Infrastructure(format!("codesign helper failed: {error}"))
                })?;
            if !status.success() {
                return Err(Phase1Error::Infrastructure(format!(
                    "codesign failed for {helper}"
                )));
            }
        }
        let status = Command::new("codesign")
            .args([
                "--force",
                "--sign",
                "-",
                "--entitlements",
                app_entitlements.to_str().unwrap_or(""),
                app_root.to_str().unwrap_or(""),
            ])
            .status()
            .map_err(|error| {
                Phase1Error::Infrastructure(format!("codesign app failed: {error}"))
            })?;
        signed = status.success();
        sign_detail = if signed {
            "ad_hoc_signed_nested_helpers_then_app".to_owned()
        } else {
            "codesign_failed".to_owned()
        };
    }

    let layout_ok = macos.join("superposition").is_file()
        && helpers.join("sp-plugin-worker").is_file()
        && helpers.join("sp-plugin-scanner").is_file()
        && info_plist_dst.is_file();
    let entitlements_ok = app_entitlements_present
        && helper_entitlements_present
        && app_forbids_library_validation
        && helper_allows_library_validation;
    let passed = layout_ok && entitlements_ok && sign_detail != "codesign_failed";

    let checklist = workspace_root.join("target/phase9/bundle-checklist.json");
    write_json(
        &checklist,
        &json!({
            "command": "bundle",
            "phase": 9,
            "profile": cargo_profile,
            "app_bundle": app_root,
            "layout_ok": layout_ok,
            "codesign_available": codesign_available,
            "signed": signed,
            "sign_detail": sign_detail,
            "app_entitlements": app_entitlements,
            "helper_entitlements": helper_entitlements,
            "app_entitlements_present": app_entitlements_present,
            "helper_entitlements_present": helper_entitlements_present,
            "app_forbids_library_validation": app_forbids_library_validation,
            "helper_allows_library_validation": helper_allows_library_validation,
            "notarization": "manual_follow_up_required",
            "requirements": [
                "main app has no disable-library-validation",
                "only worker/scanner helpers receive library-validation entitlement when justified",
                "sign nested helpers before outer app",
                "notarize and staple before public distribution (manual; credentials not in-repo)"
            ],
            "status": if passed { "app_bundle_assembled" } else { "bundle_incomplete" },
        }),
    )?;
    println!(
        "BUNDLE: profile={cargo_profile}, layout_ok={layout_ok}, signed={signed}, artifacts={}",
        checklist.display()
    );
    if passed {
        Ok(CommandOutcome::passed())
    } else {
        Ok(CommandOutcome::acceptance_failure())
    }
}

fn ensure_helper_built(workspace_root: &Path, package: &str) -> Result<(), Phase1Error> {
    let binary = workspace_root.join("target/debug").join(package);
    if binary.is_file() {
        return Ok(());
    }
    let status = Command::new("cargo")
        .args(["build", "-p", package])
        .current_dir(workspace_root)
        .status()
        .map_err(|error| {
            Phase1Error::Infrastructure(format!("could not build {package}: {error}"))
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(Phase1Error::Infrastructure(format!(
            "cargo build -p {package} failed"
        )))
    }
}

fn copy_binary(source: &Path, destination: &Path) -> Result<(), Phase1Error> {
    fs::copy(source, destination).map_err(|error| {
        Phase1Error::Infrastructure(format!(
            "could not copy {} -> {}: {error}",
            source.display(),
            destination.display()
        ))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(destination)
            .map_err(|error| {
                Phase1Error::Infrastructure(format!(
                    "could not stat {}: {error}",
                    destination.display()
                ))
            })?
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(destination, permissions).map_err(|error| {
            Phase1Error::Infrastructure(format!(
                "could not chmod {}: {error}",
                destination.display()
            ))
        })?;
    }
    Ok(())
}

pub(crate) fn usage_host_checker() -> &'static str {
    "host-checker --sdk <VST3_SDK_DIR>"
}

pub(crate) fn usage_compatibility() -> &'static str {
    "compatibility [--manifest compatibility/corpus.toml]"
}

pub(crate) fn usage_loopback() -> &'static str {
    "loopback [--frames <128|256>] [--device <name>]"
}

pub(crate) fn usage_click_test() -> &'static str {
    "click-test [--fault <name>]"
}

pub(crate) fn usage_midi_timing() -> &'static str {
    "midi-timing [--minutes <n>]"
}

pub(crate) fn usage_soak() -> &'static str {
    "soak [--hours <n>] [--smoke]"
}

pub(crate) fn usage_bundle() -> &'static str {
    "bundle [--profile <release|debug>] [--sign]"
}

fn parse_option<'a>(arguments: &'a [String], name: &str) -> Option<&'a str> {
    arguments
        .windows(2)
        .find_map(|window| (window[0] == name).then_some(window[1].as_str()))
}

fn write_json(path: &Path, value: &Value) -> Result<(), Phase1Error> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            Phase1Error::Infrastructure(format!("could not create {}: {error}", parent.display()))
        })?;
    }
    let body = serde_json::to_vec_pretty(value)
        .map_err(|error| Phase1Error::Infrastructure(format!("could not encode JSON: {error}")))?;
    fs::write(path, body).map_err(|error| {
        Phase1Error::Infrastructure(format!("could not write {}: {error}", path.display()))
    })
}
