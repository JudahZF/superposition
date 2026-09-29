//! Verification commands: SDK host checks, compatibility corpus, loopback, click, MIDI timing,
//! soak evidence, and app bundling.
//!
//! Each command writes a JSON report under `target/verification/` (or the app under
//! `target/bundle/<profile>/`) and reports missing evidence as incomplete rather than passing.

use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use crate::outcome::{CommandError, CommandOutcome};
use crate::scan_isolation::{DEFAULT_SCAN_TIMEOUT, scan_bundle_isolated};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sp_shared_memory::{BlockRequest, BlockTicket};
use sp_shared_memory_macos::{MonotonicClock, SharedMemoryRegion};

/// Runs HostChecker-oriented validation against the VST3 SDK tree.
///
/// Builds `again` / `host-checker` SDK samples when missing, then runs a parent-supervised
/// scanner smoke (and a one-block worker smoke) against the built `again` bundle.
#[allow(clippy::too_many_lines)]
pub(crate) fn run_host_checker(
    workspace_root: &Path,
    arguments: &[String],
) -> Result<CommandOutcome, CommandError> {
    let sdk = parse_option(arguments, "--sdk")
        .ok_or_else(|| CommandError::InvalidConfiguration(usage_host_checker().to_owned()))?;
    let sdk_path = PathBuf::from(sdk);
    if !sdk_path.is_dir() {
        return Err(CommandError::Infrastructure(format!(
            "VST3 SDK directory not found: {}",
            sdk_path.display()
        )));
    }

    ensure_helper_built(workspace_root, "sp-plugin-scanner")?;
    ensure_helper_built(workspace_root, "sp-plugin-worker")?;

    // Keep build output under the caller-selected SDK tree. The source tree is never patched.
    let build_dir = sdk_path.join("build-superposition-xcode");
    let mut discovered_bundles = discover_sdk_sample_bundles(&sdk_path, &build_dir);
    let needs_build = find_again_bundle(&discovered_bundles)
        .is_none_or(|bundle| !bundle_has_macos_executable(&bundle));
    let cmake_log = if needs_build {
        match build_sdk_samples(&sdk_path, &build_dir) {
            Ok(log) => Some(log),
            Err(error) => Some(format!("build_failed: {error}")),
        }
    } else {
        Some("reused loadable SDK again.vst3 bundle".to_owned())
    };
    discovered_bundles = discover_sdk_sample_bundles(&sdk_path, &build_dir);

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
        return Err(CommandError::Infrastructure(
            "sp-plugin-scanner binary missing after build".to_owned(),
        ));
    }
    if !worker.is_file() {
        return Err(CommandError::Infrastructure(
            "sp-plugin-worker binary missing after build".to_owned(),
        ));
    }

    let build_succeeded = cmake_log
        .as_deref()
        .is_some_and(|detail| !detail.starts_with("build_failed:"));
    let (status, scan_json, worker_smoke, passed) = match again_bundle
        .as_ref()
        .filter(|bundle| build_succeeded && bundle_has_macos_executable(bundle))
    {
        Some(again_bundle) => {
            // The worker is never permitted to load a bundle that the disposable scanner did not
            // first accept. This holds for SDK fixtures as well as installed candidates.
            let scan_report = scan_bundle_isolated(&scanner, again_bundle, DEFAULT_SCAN_TIMEOUT)?;
            let scanner_ok =
                scan_report.outcome == "supported" && !scan_report.descriptors.is_empty();
            let worker_smoke = if scanner_ok {
                run_worker_bundle_smoke(&worker, again_bundle)?
            } else {
                json!({
                    "ok": false,
                    "detail": "worker was not launched because isolated scanner qualification failed",
                })
            };
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
        None if !build_succeeded => (
            "sdk_sample_build_failed",
            json!({
                "outcome": "not_run",
                "detail": cmake_log,
            }),
            json!({ "ok": false, "detail": "worker was not launched because SDK again build failed" }),
            false,
        ),
        None => (
            "sdk_again_fixture_missing",
            json!({
                "outcome": "not_run",
                "detail": "SDK build completed without a loadable again.vst3 fixture",
            }),
            json!({ "ok": false, "detail": "worker was not launched because no loadable again fixture exists" }),
            false,
        ),
    };

    let marker = workspace_root.join("target/verification/host-checker.json");
    write_json(
        &marker,
        &json!({
            "command": "host-checker",
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
            "detail": "supervised scanner/worker smoke against SDK again when a loadable bundle exists; otherwise records build-unavailable honestly.",
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
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case("again.vst3"))
        })
        .min()
        .cloned()
}

fn build_sdk_samples(sdk_path: &Path, build_dir: &Path) -> Result<String, CommandError> {
    which_or_err("cmake")?;
    which_or_err("xcodebuild")?;

    fs::create_dir_all(build_dir).map_err(|error| {
        CommandError::Infrastructure(format!("could not create {}: {error}", build_dir.display()))
    })?;

    // VST3 macOS bundles require the Xcode generator so Contents/MacOS receives the binary.
    // `again` itself needs VSTGUI. Xcode 27 diagnoses its legacy `wstring_convert` use as a
    // deprecation error because the SDK's VSTGUI target unconditionally adds `-Werror`.
    // Append only a targeted warning exception through the generated project; do not alter SDK
    // sources or suppress any other warnings.
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
            // The SDK defaults to 10.13, which Xcode 27 no longer accepts for arm64 builds.
            "-DCMAKE_OSX_DEPLOYMENT_TARGET=12.0".to_owned(),
            // Do not let an existing Xcode-generator cache select the Rosetta/x86_64 slice on
            // the reference Apple Silicon host. The qualification fixture must be arm64.
            "-DCMAKE_OSX_ARCHITECTURES=arm64".to_owned(),
            "-DSMTG_ENABLE_VSTGUI_SUPPORT=ON".to_owned(),
            "-DCMAKE_XCODE_ATTRIBUTE_OTHER_CPLUSPLUSFLAGS=-Wno-error=deprecated-declarations"
                .to_owned(),
            "-DCMAKE_XCODE_ATTRIBUTE_OTHER_CFLAGS=-Wno-error=deprecated-declarations".to_owned(),
        ])
        .output()
        .map_err(|error| {
            CommandError::Infrastructure(format!("could not configure VST3 SDK CMake: {error}"))
        })?;
    if !configure.status.success() {
        return Err(CommandError::Infrastructure(format!(
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
            "--".to_owned(),
            "ARCHS=arm64".to_owned(),
            // VSTGUI owns the legacy wstring_convert call and adds -Werror itself. Preserve
            // inherited flags while downgrading only that diagnostic during the SDK fixture
            // build; no SDK source is patched and all other warnings remain errors.
            "OTHER_CPLUSPLUSFLAGS=$(inherited) -Wno-error=deprecated-declarations".to_owned(),
        ])
        .output()
        .map_err(|error| {
            CommandError::Infrastructure(format!("could not build VST3 SDK samples: {error}"))
        })?;
    if !build.status.success() {
        return Err(CommandError::Infrastructure(format!(
            "cmake build of again failed: stdout: {}; stderr: {}",
            truncate_lossy(&build.stdout, 1_500),
            truncate_lossy(&build.stderr, 1_500)
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

fn which_or_err(name: &str) -> Result<PathBuf, CommandError> {
    let output = Command::new("/usr/bin/which")
        .arg(name)
        .output()
        .map_err(|error| {
            CommandError::Infrastructure(format!("could not locate `{name}`: {error}"))
        })?;
    if !output.status.success() {
        return Err(CommandError::Infrastructure(format!(
            "`{name}` is required to build VST3 SDK samples (again, host-checker)"
        )));
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if path.is_empty() {
        return Err(CommandError::Infrastructure(format!(
            "`{name}` is required to build VST3 SDK samples (again, host-checker)"
        )));
    }
    Ok(PathBuf::from(path))
}

#[allow(clippy::too_many_lines)]
fn run_worker_bundle_smoke(worker: &Path, bundle: &Path) -> Result<Value, CommandError> {
    const READY_TIMEOUT: Duration = Duration::from_secs(5);
    const HEARTBEAT_TIMEOUT: Duration = Duration::from_millis(250);
    const COMPLETION_TIMEOUT: Duration = Duration::from_secs(5);
    const SMOKE_FRAMES: usize = 128;
    const LEFT_INPUT: f32 = 0.25;
    const RIGHT_INPUT: f32 = -0.25;

    let clock = MonotonicClock::new().map_err(|error| {
        CommandError::Infrastructure(format!("could not create monotonic clock: {error}"))
    })?;
    let mut region = SharedMemoryRegion::create(42).map_err(|error| {
        CommandError::Infrastructure(format!("could not create smoke bank: {error}"))
    })?;
    let bank_name = region.name().to_owned();
    let mut child = Command::new(worker)
        .args([
            "--bank".to_owned(),
            bank_name,
            "--worker-id".to_owned(),
            "1".to_owned(),
            "--bundle".to_owned(),
            bundle.display().to_string(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            CommandError::Infrastructure(format!("could not launch worker smoke: {error}"))
        })?;

    if !wait_for_worker_ready(&mut child, READY_TIMEOUT) {
        shutdown_worker(&mut child);
        return Ok(json!({
            "ok": false,
            "detail": "worker did not publish fixed readiness evidence",
        }));
    }
    let initial_heartbeat = region.bank().header.worker_heartbeat();
    if initial_heartbeat == 0
        || !wait_for_heartbeat_advance(&region, &mut child, initial_heartbeat, HEARTBEAT_TIMEOUT)
    {
        shutdown_worker(&mut child);
        return Ok(json!({
            "ok": false,
            "detail": "worker readiness was not followed by a progressing shared-memory heartbeat",
            "initial_heartbeat": initial_heartbeat,
        }));
    }

    let ticket = BlockTicket {
        generation: 1,
        sequence: 1,
    };
    let request = BlockRequest {
        frame_count: u32::try_from(SMOKE_FRAMES).expect("fixed smoke frame count fits u32"),
        input_channel_count: 2,
        output_channel_count: 2,
        midi_event_count: 0,
        event_count: 0,
        flags: 0,
        sidechain_slots: 0,
    };
    let publish_result = {
        let bank = region.bank_mut();
        let slot = bank
            .slots
            .get_mut(0)
            .ok_or_else(|| CommandError::Infrastructure("smoke bank missing slot 0".to_owned()));
        slot.and_then(|slot| {
            for frame in 0..SMOKE_FRAMES {
                slot.input_audio[0][frame] = LEFT_INPUT;
                slot.input_audio[1][frame] = RIGHT_INPUT;
            }
            slot.publish_request_at(ticket, request, clock.now_ticks())
                .map_err(|error| {
                    CommandError::Infrastructure(format!(
                        "could not publish smoke request: {error}"
                    ))
                })
        })
    };
    if let Err(error) = publish_result {
        shutdown_worker(&mut child);
        return Err(error);
    }

    let deadline = Instant::now() + COMPLETION_TIMEOUT;
    let mut result = None;
    while Instant::now() < deadline {
        match region.bank().slots[0].completion_snapshot() {
            Ok(Some(snapshot)) => {
                let ticket_matches = snapshot.ticket == ticket;
                let timing = snapshot.timing;
                let timing_valid = timing.is_valid()
                    && timing.request_published_tick > 0
                    && timing.worker_claimed_tick >= timing.request_published_tick
                    && timing.completion_published_tick >= timing.worker_claimed_tick;
                let output_finite = region.bank().slots[0].output_audio.iter().all(|channel| {
                    channel[..SMOKE_FRAMES]
                        .iter()
                        .all(|sample| sample.is_finite())
                });
                let expected_output = region.bank().slots[0].output_audio[0][0] == LEFT_INPUT
                    && region.bank().slots[0].output_audio[1][0] == RIGHT_INPUT;
                if ticket_matches {
                    let _ = region.bank_mut().slots[0].consume_completion(ticket);
                }
                result = Some(json!({
                    "ok": ticket_matches && timing_valid && output_finite && expected_output,
                    "ticket_matches": ticket_matches,
                    "timing_valid": timing_valid,
                    "request_published_tick": timing.request_published_tick,
                    "worker_claimed_tick": timing.worker_claimed_tick,
                    "completion_published_tick": timing.completion_published_tick,
                    "output_finite": output_finite,
                    "expected_unity_gain_output": expected_output,
                    "detail": if ticket_matches && timing_valid && output_finite && expected_output {
                        "processed one finite stereo 128-frame block through again.vst3 at default unity gain"
                    } else {
                        "worker completion failed ticket, timing, finite-sample, or default Again output validation"
                    },
                }));
                break;
            }
            Ok(None) => {}
            Err(error) => {
                result = Some(json!({
                    "ok": false,
                    "detail": format!("shared-memory completion was malformed: {error}"),
                }));
                break;
            }
        }
        if let Ok(Some(status)) = child.try_wait() {
            result = Some(json!({
                "ok": false,
                "detail": format!("worker exited before completion with {status}"),
            }));
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }

    shutdown_worker(&mut child);
    Ok(result.unwrap_or_else(|| {
        json!({
            "ok": false,
            "detail": "timed out waiting for one-block worker completion",
        })
    }))
}

fn wait_for_heartbeat_advance(
    region: &SharedMemoryRegion,
    child: &mut Child,
    initial: u64,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if region.bank().header.worker_heartbeat() > initial {
            return true;
        }
        if child.try_wait().ok().flatten().is_some() {
            return false;
        }
        thread::sleep(Duration::from_millis(1));
    }
    region.bank().header.worker_heartbeat() > initial
}

fn shutdown_worker(child: &mut Child) {
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(b"shutdown\n");
        let _ = stdin.flush();
    }
    let _ = child.wait_timeout_or_kill(Duration::from_secs(2));
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
) -> Result<CommandOutcome, CommandError> {
    let manifest = parse_option(arguments, "--manifest").unwrap_or("compatibility/corpus.toml");
    let path = workspace_root.join(manifest);
    let text = fs::read_to_string(&path).map_err(|error| {
        CommandError::Infrastructure(format!("could not read {}: {error}", path.display()))
    })?;
    let corpus = parse_compatibility_corpus(&text, &path)?;

    ensure_helper_built(workspace_root, "sp-plugin-scanner")?;
    let scanner = workspace_root.join("target/debug/sp-plugin-scanner");
    let fixture_root = path
        .parent()
        .unwrap_or(workspace_root)
        .join(&corpus.fixture_root);
    let mut case_reports = Vec::new();
    let mut executed = 0_u32;
    let mut passed = 0_u32;
    let mut incomplete = Vec::new();

    for case in &corpus.cases {
        let bundle = fixture_root.join(&case.fixture);
        if !bundle.exists() {
            if case.mandatory {
                incomplete.push(format!("mandatory case `{}` fixture is missing", case.id));
            }
            case_reports.push(json!({
                "id": case.id,
                "fixture": case.fixture,
                "mandatory": case.mandatory,
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
                CommandError::Infrastructure(format!("could not run scanner: {error}"))
            })?;
        let ok = output.status.success();
        if ok && case.expected == "pass" {
            passed = passed.saturating_add(1);
        } else if case.mandatory {
            incomplete.push(format!(
                "mandatory case `{}` did not meet expected outcome",
                case.id
            ));
        }
        case_reports.push(json!({
            "id": case.id,
            "fixture": case.fixture,
            "mandatory": case.mandatory,
            "expected": case.expected,
            "status": if ok { "passed" } else { "failed" },
            "stdout": String::from_utf8_lossy(&output.stdout).trim(),
        }));
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
                    CommandError::Infrastructure(format!("could not run scanner: {error}"))
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

    let mandatory_cases = corpus.cases.iter().filter(|case| case.mandatory).count();
    let qualified = incomplete.is_empty() && mandatory_cases > 0;
    let report = workspace_root.join("target/verification/compatibility.json");
    write_json(
        &report,
        &json!({
            "command": "compatibility",
            "schema_version": 1,
            "manifest": path,
            "corpus": corpus.name,
            "status": if qualified { "qualified" } else { "evidence_incomplete" },
            "qualified": qualified,
            "certified": false,
            "mandatory_cases": mandatory_cases,
            "executed": executed,
            "passed": passed,
            "incomplete": incomplete,
            "cases": case_reports,
        }),
    )?;
    println!(
        "COMPATIBILITY: executed={executed}, passed={passed}, artifacts={}",
        report.display()
    );
    Ok(if qualified {
        CommandOutcome::passed()
    } else {
        CommandOutcome::acceptance_failure()
    })
}

/// Correlates externally captured loopback audio. The AUHAL wrapper has no input API.
pub(crate) fn run_loopback(
    workspace_root: &Path,
    arguments: &[String],
) -> Result<CommandOutcome, CommandError> {
    let report = workspace_root.join("target/verification/loopback.json");
    let result = match (parse_option(arguments, "--capture"), parse_option(arguments, "--stimulus")) {
        (Some(capture), Some(stimulus)) => correlate_loopback(Path::new(capture), Path::new(stimulus), arguments),
        _ => Err("--capture and --stimulus f32le evidence are mandatory; output-only AUHAL cannot capture loopback".to_owned()),
    };
    match result {
        Ok(metrics) => {
            let qualified = metrics.correlation >= 0.95;
            write_json(
                &report,
                &json!({
                    "command":"loopback", "schema_version":1,
                    "capture_api":"external_required_output_only_auhal", "qualified":qualified,
                    "certified":false, "status":if qualified {"qualified"} else {"correlation_failed"},
                    "capture_samples":metrics.capture_samples, "stimulus_samples":metrics.stimulus_samples,
                    "lag_frames":metrics.lag_frames, "correlation":metrics.correlation,
                }),
            )?;
            Ok(if qualified {
                CommandOutcome::passed()
            } else {
                CommandOutcome::acceptance_failure()
            })
        }
        Err(detail) => {
            write_json(
                &report,
                &json!({"command":"loopback", "schema_version":1, "qualified":false, "certified":false, "status":"evidence_incomplete", "detail":detail}),
            )?;
            Ok(CommandOutcome::acceptance_failure())
        }
    }
}

/// Measures discontinuities in captured fault-transition audio.
pub(crate) fn run_click_test(
    workspace_root: &Path,
    arguments: &[String],
) -> Result<CommandOutcome, CommandError> {
    let fault = parse_option(arguments, "--fault").unwrap_or("unspecified");
    let threshold = parse_f64_option(arguments, "--max-derivative", 0.1)?;
    let report = workspace_root.join("target/verification/click-test.json");
    let Some(capture) = parse_option(arguments, "--capture") else {
        write_json(
            &report,
            &json!({"command":"click-test", "schema_version":1, "qualified":false, "certified":false, "status":"evidence_incomplete", "detail":"--capture f32le audio is mandatory; synthetic fault containment is not click evidence"}),
        )?;
        return Ok(CommandOutcome::acceptance_failure());
    };
    let samples = read_f32le(Path::new(capture))?;
    let Some((max_derivative, rms_derivative)) = derivative_metrics(&samples) else {
        return Err(CommandError::InvalidConfiguration(
            "captured audio must contain at least two finite f32 samples".to_owned(),
        ));
    };
    let qualified = max_derivative <= threshold;
    write_json(
        &report,
        &json!({"command":"click-test", "schema_version":1, "fault":fault, "capture":capture, "samples":samples.len(), "max_derivative":max_derivative, "rms_derivative":rms_derivative, "threshold":threshold, "qualified":qualified, "certified":false, "status":if qualified {"qualified"} else {"click_threshold_exceeded"}}),
    )?;
    Ok(if qualified {
        CommandOutcome::passed()
    } else {
        CommandOutcome::acceptance_failure()
    })
}

/// Calculates timing statistics from an externally recorded `CoreMIDI` event trace.
pub(crate) fn run_midi_timing(
    workspace_root: &Path,
    arguments: &[String],
) -> Result<CommandOutcome, CommandError> {
    let report = workspace_root.join("target/verification/midi-timing.json");
    let Some(trace) = parse_option(arguments, "--events") else {
        write_json(
            &report,
            &json!({"command":"midi-timing", "schema_version":1, "qualified":false, "certified":false, "status":"evidence_incomplete", "detail":"--events CoreMIDI timestamp trace is mandatory"}),
        )?;
        return Ok(CommandOutcome::acceptance_failure());
    };
    let mut input = sp_midi::MidirInput::new();
    let attached = input.open_first_available().is_ok() && input.connected_port().is_some();
    let port_name = input.connected_port().map(|port| port.name.clone());
    let latencies = parse_midi_trace(Path::new(trace))?;
    let stats = timing_statistics(&latencies).ok_or_else(|| {
        CommandError::InvalidConfiguration(
            "MIDI trace has no valid sent/received timestamps".to_owned(),
        )
    })?;
    let qualified = attached && stats.count >= 100;
    write_json(
        &report,
        &json!({"command":"midi-timing", "schema_version":1, "trace":trace, "coremidi_attached":attached, "port_name":port_name, "statistics":stats, "qualified":qualified, "certified":false, "status":if qualified {"qualified"} else {"evidence_incomplete"}}),
    )?;
    Ok(if qualified {
        CommandOutcome::passed()
    } else {
        CommandOutcome::acceptance_failure()
    })
}

/// Validates a supervisor-produced soak report with memory, process, and fault evidence.
pub(crate) fn run_soak(
    workspace_root: &Path,
    arguments: &[String],
) -> Result<CommandOutcome, CommandError> {
    let hours = parse_option(arguments, "--hours").unwrap_or("8");
    let hours_value: f64 = hours
        .parse()
        .map_err(|_| CommandError::InvalidConfiguration(format!("invalid --hours '{hours}'")))?;
    if hours_value <= 0.0 {
        return Err(CommandError::InvalidConfiguration(
            "--hours must be positive".to_owned(),
        ));
    }
    let report = workspace_root.join("target/verification/soak.json");
    let Some(metrics) = parse_option(arguments, "--metrics") else {
        write_json(
            &report,
            &json!({"command":"soak", "schema_version":1, "hours":hours_value, "qualified":false, "certified":false, "status":"evidence_incomplete", "detail":"--metrics supervisor report is mandatory; --smoke cannot qualify a soak"}),
        )?;
        return Ok(CommandOutcome::acceptance_failure());
    };
    let maximum_growth = parse_u64_option(arguments, "--max-memory-growth-bytes", u64::MAX)?;
    let maximum_growth_percent = parse_f64_option(arguments, "--max-memory-growth-percent", 5.0)?;
    let evidence = evaluate_soak_metrics(
        Path::new(metrics),
        hours_value,
        maximum_growth,
        maximum_growth_percent,
    )?;
    let qualified = evidence.qualified;
    write_json(
        &report,
        &json!({"command":"soak", "schema_version":1, "hours":hours_value, "metrics":metrics, "max_memory_growth_bytes":maximum_growth, "max_memory_growth_percent":maximum_growth_percent, "evidence":evidence, "qualified":qualified, "certified":false, "status":if qualified {"qualified"} else {"evidence_incomplete"}}),
    )?;
    Ok(if qualified {
        CommandOutcome::passed()
    } else {
        CommandOutcome::acceptance_failure()
    })
}

/// Builds either an explicitly non-distributable local development app or a fully signed
/// release candidate. Resource and brand-policy inputs are declarative under `packaging/resources`.
#[allow(clippy::too_many_lines)]
pub(crate) fn run_bundle(
    workspace_root: &Path,
    arguments: &[String],
) -> Result<CommandOutcome, CommandError> {
    let profile = parse_option(arguments, "--profile").unwrap_or("local-dev");
    if !matches!(profile, "local-dev" | "release") {
        return Err(CommandError::InvalidConfiguration(
            "--profile must be `local-dev` or `release`".to_owned(),
        ));
    }
    let release = profile == "release";
    let manifest_path = workspace_root.join(format!("packaging/resources/{profile}.json"));
    let manifest = read_resource_manifest(&manifest_path)?;
    if manifest.schema_version != 1 || manifest.profile != profile {
        return Err(CommandError::InvalidConfiguration(format!(
            "{} must declare schema_version 1 and profile `{profile}`",
            manifest_path.display()
        )));
    }

    let artifact_root = workspace_root.join("target/bundle").join(profile);
    let checklist = artifact_root.join("bundle-checklist.json");
    if release {
        let blockers = release_blockers(&manifest, workspace_root);
        if !blockers.is_empty() {
            write_json(
                &checklist,
                &json!({
                    "command": "bundle", "profile": profile,
                    "status": "release_blocked", "release_blockers": blockers,
                    "resource_manifest": manifest_path,
                }),
            )?;
            println!("BUNDLE: release blocked; artifacts={}", checklist.display());
            return Ok(CommandOutcome::acceptance_failure());
        }
    }

    let build_number = parse_build_number(arguments, release)?;
    let version = workspace_version(workspace_root)?;
    let cargo_profile = if release { "release" } else { "debug" };
    let mut build_args = vec!["build".to_owned()];
    if release {
        build_args.push("--release".to_owned());
    }
    for package in ["superposition", "sp-plugin-worker", "sp-plugin-scanner"] {
        build_args.extend(["-p".to_owned(), package.to_owned()]);
    }
    let status = Command::new("cargo")
        .args(&build_args)
        .current_dir(workspace_root)
        .status()
        .map_err(|error| CommandError::Infrastructure(format!("cargo build failed: {error}")))?;
    if !status.success() {
        return Err(CommandError::Infrastructure(
            "cargo build for bundle failed".to_owned(),
        ));
    }

    let target_dir = workspace_root.join("target").join(cargo_profile);
    let app_root = artifact_root.join("Superposition.app");
    if app_root.exists() {
        fs::remove_dir_all(&app_root).map_err(|error| {
            CommandError::Infrastructure(format!("could not reset {}: {error}", app_root.display()))
        })?;
    }
    let contents = app_root.join("Contents");
    let macos = contents.join("MacOS");
    let helpers = contents.join("Helpers");
    let resources = contents.join("Resources");
    for directory in [&macos, &helpers, &resources] {
        fs::create_dir_all(directory).map_err(|error| {
            CommandError::Infrastructure(format!("could not create app layout: {error}"))
        })?;
    }

    let plist_template = workspace_root.join("packaging/macos/Info.plist");
    let plist = fs::read_to_string(&plist_template).map_err(|error| {
        CommandError::Infrastructure(format!(
            "could not read {}: {error}",
            plist_template.display()
        ))
    })?;
    if !plist.contains("__VERSION__") || !plist.contains("__BUILD__") {
        return Err(CommandError::InvalidConfiguration(format!(
            "{} must contain __VERSION__ and __BUILD__ placeholders",
            plist_template.display()
        )));
    }
    fs::write(
        contents.join("Info.plist"),
        plist
            .replace("__VERSION__", &version)
            .replace("__BUILD__", &build_number),
    )
    .map_err(|error| {
        CommandError::Infrastructure(format!("could not write Info.plist: {error}"))
    })?;

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
    let staged_resources = stage_resources(workspace_root, &resources, &manifest.resources)?;

    let app_entitlements = workspace_root.join("packaging/entitlements/app.entitlements");
    let helper_entitlements = workspace_root.join("packaging/entitlements/helper.entitlements");
    validate_entitlement_templates(&app_entitlements, &helper_entitlements)?;

    let binary_paths = [
        ("app", macos.join("superposition")),
        ("sp-plugin-worker", helpers.join("sp-plugin-worker")),
        ("sp-plugin-scanner", helpers.join("sp-plugin-scanner")),
    ];
    let helper_manifest = helper_manifest(&binary_paths, workspace_root)?;
    let helper_manifest_path = resources.join("superposition-helper-manifest.json");
    write_json(&helper_manifest_path, &helper_manifest)?;
    let provenance = build_provenance(workspace_root, &version, &build_number, profile);
    write_json(
        &resources.join("superposition-build-provenance.json"),
        &provenance,
    )?;

    let mut signing = json!({ "mode": "unsigned-local-development" });
    if release {
        let identity = required_option(arguments, "--identity")?;
        let notary_profile = required_option(arguments, "--notary-profile")?;
        sign_release_bundle(
            &app_root,
            &binary_paths,
            &app_entitlements,
            &helper_entitlements,
            identity,
        )?;
        verify_signed_entitlements(&binary_paths)?;
        verify_codesign(&app_root)?;
        notarize_staple_and_assess(&app_root, &artifact_root, notary_profile)?;
        verify_codesign(&app_root)?;
        signing = json!({
            "mode": "developer-id", "identity": identity, "hardened_runtime": true,
            "notary_keychain_profile": notary_profile, "notarized": true,
            "stapled": true, "spctl_assessed": true,
        });
    } else if arguments.iter().any(|argument| argument == "--sign") {
        sign_local_bundle(
            &app_root,
            &binary_paths,
            &app_entitlements,
            &helper_entitlements,
        )?;
        verify_signed_entitlements(&binary_paths)?;
        verify_codesign(&app_root)?;
        signing = json!({ "mode": "ad-hoc-local-development", "hardened_runtime": false });
    }

    write_json(
        &checklist,
        &json!({
            "command": "bundle", "profile": profile,
            "status": if release { "release_ready" } else { "local_development_bundle_assembled" },
            "app_bundle": app_root, "resource_manifest": manifest_path,
            "staged_resources": staged_resources, "helper_protocol_manifest": helper_manifest_path,
            "build_provenance": provenance, "signing": signing,
            "release_distribution": release,
        }),
    )?;
    println!(
        "BUNDLE: profile={profile}, artifacts={}",
        checklist.display()
    );
    Ok(CommandOutcome::passed())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResourceManifest {
    schema_version: u32,
    profile: String,
    release_approvals: ReleaseApprovals,
    resources: Vec<ResourceEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseApprovals {
    logo: Approval,
    icon: Approval,
    fonts: Approval,
    fault_red: FaultRedApproval,
    provenance: Approval,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(
    clippy::struct_field_names,
    reason = "field names mirror the approved qualification file keys"
)]
struct Approval {
    approved: bool,
    approval_owner: Option<String>,
    approval_reference: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(
    clippy::struct_field_names,
    reason = "field names mirror the approved qualification file keys"
)]
struct FaultRedApproval {
    approved: bool,
    value: Option<String>,
    approval_owner: Option<String>,
    approval_reference: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResourceEntry {
    source: String,
    destination: String,
    sha256: Option<String>,
    provenance: ResourceProvenance,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResourceProvenance {
    source: Option<String>,
    license: Option<String>,
    rights_holder: Option<String>,
    approval_owner: Option<String>,
    approval_reference: Option<String>,
}

fn read_resource_manifest(path: &Path) -> Result<ResourceManifest, CommandError> {
    let bytes = fs::read(path).map_err(|error| {
        CommandError::Infrastructure(format!(
            "could not read resource manifest {}: {error}",
            path.display()
        ))
    })?;
    serde_json::from_slice(&bytes).map_err(|error| {
        CommandError::InvalidConfiguration(format!(
            "invalid resource manifest {}: {error}",
            path.display()
        ))
    })
}

fn release_blockers(manifest: &ResourceManifest, workspace_root: &Path) -> Vec<String> {
    let mut blockers = Vec::new();
    for (name, approval) in [
        ("approved production logo", &manifest.release_approvals.logo),
        ("approved production icon", &manifest.release_approvals.icon),
        (
            "licensed production fonts",
            &manifest.release_approvals.fonts,
        ),
        (
            "documented asset provenance",
            &manifest.release_approvals.provenance,
        ),
    ] {
        if !approval.approved
            || approval.approval_owner.as_deref().unwrap_or("").is_empty()
            || approval
                .approval_reference
                .as_deref()
                .unwrap_or("")
                .is_empty()
        {
            blockers.push(format!("{name} approval is incomplete"));
        }
    }
    let fault_red = &manifest.release_approvals.fault_red;
    if !fault_red.approved
        || fault_red.value.as_deref().unwrap_or("").is_empty()
        || fault_red.approval_owner.as_deref().unwrap_or("").is_empty()
        || fault_red
            .approval_reference
            .as_deref()
            .unwrap_or("")
            .is_empty()
    {
        blockers.push("approved fault-red token is incomplete".to_owned());
    }
    for resource in &manifest.resources {
        if resource
            .sha256
            .as_deref()
            .as_ref()
            .is_none_or(|hash| !is_sha256(hash))
        {
            blockers.push(format!("{} lacks a valid SHA-256", resource.source));
        }
        if [
            resource.provenance.source.as_deref(),
            resource.provenance.license.as_deref(),
            resource.provenance.rights_holder.as_deref(),
            resource.provenance.approval_owner.as_deref(),
            resource.provenance.approval_reference.as_deref(),
        ]
        .iter()
        .any(|field| field.is_none_or(str::is_empty))
        {
            blockers.push(format!("{} lacks complete provenance", resource.source));
        }
        if !workspace_root.join(&resource.source).is_file() {
            blockers.push(format!("required resource is missing: {}", resource.source));
        }
    }
    blockers
}

fn stage_resources(
    workspace_root: &Path,
    destination_root: &Path,
    resources: &[ResourceEntry],
) -> Result<Vec<Value>, CommandError> {
    let mut staged = Vec::new();
    for resource in resources {
        let source_relative = safe_relative_path(&resource.source, "resource source")?;
        let destination_relative =
            safe_relative_path(&resource.destination, "resource destination")?;
        let source = workspace_root.join(source_relative);
        let destination = destination_root.join(destination_relative);
        let expected = resource.sha256.as_deref().ok_or_else(|| {
            CommandError::InvalidConfiguration(format!("{} is missing sha256", source.display()))
        })?;
        if !is_sha256(expected) {
            return Err(CommandError::InvalidConfiguration(format!(
                "{} has invalid sha256",
                source.display()
            )));
        }
        let actual = sha256_file(&source)?;
        if actual != expected {
            return Err(CommandError::InvalidConfiguration(format!(
                "SHA-256 mismatch for {}",
                source.display()
            )));
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                CommandError::Infrastructure(format!(
                    "could not create resource directory: {error}"
                ))
            })?;
        }
        fs::copy(&source, &destination).map_err(|error| {
            CommandError::Infrastructure(format!("could not stage {}: {error}", source.display()))
        })?;
        staged.push(json!({"source": resource.source, "destination": resource.destination, "sha256": actual}));
    }
    Ok(staged)
}

fn safe_relative_path<'a>(value: &'a str, field: &str) -> Result<&'a Path, CommandError> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(CommandError::InvalidConfiguration(format!(
            "{field} must be a non-empty relative path"
        )));
    }
    Ok(path)
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn sha256_file(path: &Path) -> Result<String, CommandError> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = fs::read(path).map_err(|error| {
        CommandError::Infrastructure(format!("could not read {}: {error}", path.display()))
    })?;
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Ok(encoded)
}

fn validate_entitlement_templates(app: &Path, helper: &Path) -> Result<(), CommandError> {
    let app_body = fs::read_to_string(app).map_err(|error| {
        CommandError::Infrastructure(format!("could not read {}: {error}", app.display()))
    })?;
    let helper_body = fs::read_to_string(helper).map_err(|error| {
        CommandError::Infrastructure(format!("could not read {}: {error}", helper.display()))
    })?;
    if !has_library_validation_value(&app_body, false) {
        return Err(CommandError::InvalidConfiguration(
            "app entitlement must explicitly keep library validation enabled".to_owned(),
        ));
    }
    if !has_library_validation_value(&helper_body, true) {
        return Err(CommandError::InvalidConfiguration(
            "helper entitlement must explicitly disable library validation".to_owned(),
        ));
    }
    Ok(())
}

fn has_library_validation_value(entitlements: &str, expected: bool) -> bool {
    let key = "<key>com.apple.security.cs.disable-library-validation</key>";
    let Some(value) = entitlements.split_once(key).map(|(_, value)| value) else {
        return false;
    };
    value
        .trim_start()
        .starts_with(if expected { "<true/>" } else { "<false/>" })
}

fn sign_release_bundle(
    app_root: &Path,
    binaries: &[(&str, PathBuf)],
    app_entitlements: &Path,
    helper_entitlements: &Path,
    identity: &str,
) -> Result<(), CommandError> {
    for (_, helper) in &binaries[1..] {
        run_codesign(helper, helper_entitlements, identity, true)?;
    }
    run_codesign(app_root, app_entitlements, identity, true)
}

fn sign_local_bundle(
    app_root: &Path,
    binaries: &[(&str, PathBuf)],
    app_entitlements: &Path,
    helper_entitlements: &Path,
) -> Result<(), CommandError> {
    for (_, helper) in &binaries[1..] {
        run_codesign(helper, helper_entitlements, "-", false)?;
    }
    run_codesign(app_root, app_entitlements, "-", false)
}

fn run_codesign(
    path: &Path,
    entitlements: &Path,
    identity: &str,
    hardened_runtime: bool,
) -> Result<(), CommandError> {
    let mut command = Command::new("codesign");
    command
        .args(["--force", "--sign", identity, "--entitlements"])
        .arg(entitlements);
    if hardened_runtime {
        command.args(["--options", "runtime", "--timestamp"]);
    }
    let status = command.arg(path).status().map_err(|error| {
        CommandError::Infrastructure(format!("could not codesign {}: {error}", path.display()))
    })?;
    if status.success() {
        Ok(())
    } else {
        Err(CommandError::Infrastructure(format!(
            "codesign failed for {}",
            path.display()
        )))
    }
}

fn verify_signed_entitlements(binaries: &[(&str, PathBuf)]) -> Result<(), CommandError> {
    for (name, binary) in binaries {
        let output = Command::new("codesign")
            .args(["-d", "--entitlements", ":-"])
            .arg(binary)
            .output()
            .map_err(|error| {
                CommandError::Infrastructure(format!(
                    "could not inspect signed entitlements for {name}: {error}"
                ))
            })?;
        let output_text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let expected = *name != "app";
        if !output.status.success() || !has_library_validation_value(&output_text, expected) {
            return Err(CommandError::Infrastructure(format!(
                "signed entitlement verification failed for {name}"
            )));
        }
    }
    Ok(())
}

fn verify_codesign(app_root: &Path) -> Result<(), CommandError> {
    run_checked(
        Command::new("codesign")
            .args(["--verify", "--deep", "--strict", "--verbose=4"])
            .arg(app_root),
        "codesign deep/strict verification",
    )
}

fn notarize_staple_and_assess(
    app_root: &Path,
    artifact_root: &Path,
    profile: &str,
) -> Result<(), CommandError> {
    let archive = artifact_root.join("Superposition-notarization.zip");
    run_checked(
        Command::new("ditto")
            .args(["-c", "-k", "--keepParent"])
            .arg(app_root)
            .arg(&archive),
        "notarization archive creation",
    )?;
    run_checked(
        Command::new("xcrun")
            .args(["notarytool", "submit"])
            .arg(&archive)
            .args(["--keychain-profile", profile, "--wait"]),
        "notarytool submission",
    )?;
    run_checked(
        Command::new("xcrun")
            .args(["stapler", "staple"])
            .arg(app_root),
        "notary ticket stapling",
    )?;
    run_checked(
        Command::new("xcrun")
            .args(["stapler", "validate"])
            .arg(app_root),
        "staple validation",
    )?;
    run_checked(
        Command::new("spctl")
            .args(["--assess", "--type", "execute", "--verbose=4"])
            .arg(app_root),
        "spctl assessment",
    )
}

fn run_checked(command: &mut Command, operation: &str) -> Result<(), CommandError> {
    let status = command.status().map_err(|error| {
        CommandError::Infrastructure(format!("could not run {operation}: {error}"))
    })?;
    if status.success() {
        Ok(())
    } else {
        Err(CommandError::Infrastructure(format!("{operation} failed")))
    }
}

fn required_option<'a>(arguments: &'a [String], name: &str) -> Result<&'a str, CommandError> {
    parse_option(arguments, name)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            CommandError::InvalidConfiguration(format!("{name} is required for a release bundle"))
        })
}

fn parse_build_number(arguments: &[String], release: bool) -> Result<String, CommandError> {
    let build = parse_option(arguments, "--build-number").unwrap_or(if release { "" } else { "0" });
    if build.is_empty()
        || !build
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return Err(CommandError::InvalidConfiguration(
            "--build-number must be a dot-separated numeric CFBundleVersion".to_owned(),
        ));
    }
    Ok(build.to_owned())
}

fn workspace_version(workspace_root: &Path) -> Result<String, CommandError> {
    let manifest = fs::read_to_string(workspace_root.join("Cargo.toml")).map_err(|error| {
        CommandError::Infrastructure(format!("could not read workspace Cargo.toml: {error}"))
    })?;
    manifest
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("version = ")
                .map(|value| value.trim().trim_matches('"').to_owned())
        })
        .filter(|version| !version.is_empty())
        .ok_or_else(|| {
            CommandError::InvalidConfiguration("workspace package version is missing".to_owned())
        })
}

fn helper_manifest(
    binaries: &[(&str, PathBuf)],
    workspace_root: &Path,
) -> Result<Value, CommandError> {
    let helpers = binaries
        .iter()
        .map(|(name, path)| Ok(json!({"name": name, "sha256": sha256_file(path)?})))
        .collect::<Result<Vec<_>, CommandError>>()?;
    Ok(
        json!({"schema_version": 1, "protocol": protocol_versions(workspace_root)?, "helpers": helpers}),
    )
}

fn protocol_versions(workspace_root: &Path) -> Result<Value, CommandError> {
    let protocol = fs::read_to_string(workspace_root.join("crates/sp-protocol/src/lib.rs"))
        .map_err(|error| {
            CommandError::Infrastructure(format!("could not read protocol version: {error}"))
        })?;
    let control = fs::read_to_string(workspace_root.join("crates/sp-protocol/src/control.rs"))
        .map_err(|error| {
            CommandError::Infrastructure(format!(
                "could not read control protocol version: {error}"
            ))
        })?;
    Ok(json!({
        "shared_memory": const_value(&protocol, "PROTOCOL_VERSION")?,
        "control": const_value(&control, "CONTROL_PROTOCOL_VERSION")?,
    }))
}

fn const_value(source: &str, name: &str) -> Result<u64, CommandError> {
    source
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix(&format!("pub const {name}: "))
                .and_then(|value| {
                    value
                        .rsplit_once('=')
                        .map(|(_, value)| value.trim().trim_end_matches(';').parse::<u64>().ok())
                })
                .flatten()
        })
        .ok_or_else(|| CommandError::InvalidConfiguration(format!("could not parse {name}")))
}

fn build_provenance(workspace_root: &Path, version: &str, build: &str, profile: &str) -> Value {
    json!({
        "schema_version": 1, "version": version, "build_number": build, "profile": profile,
        "git_commit": command_output(workspace_root, "git", &["rev-parse", "HEAD"]),
        "git_dirty": command_output(workspace_root, "git", &["status", "--porcelain"]),
        "rustc": command_output(workspace_root, "rustc", &["-Vv"]),
        "xcode": command_output(workspace_root, "xcodebuild", &["-version"]),
    })
}

fn command_output(workspace_root: &Path, program: &str, arguments: &[&str]) -> Value {
    match Command::new(program)
        .args(arguments)
        .current_dir(workspace_root)
        .output()
    {
        Ok(output) if output.status.success() => {
            Value::String(String::from_utf8_lossy(&output.stdout).trim().to_owned())
        }
        Ok(output) => Value::String(format!(
            "unavailable: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
        Err(error) => Value::String(format!("unavailable: {error}")),
    }
}

fn ensure_helper_built(workspace_root: &Path, package: &str) -> Result<(), CommandError> {
    let binary = workspace_root.join("target/debug").join(package);
    if binary.is_file() {
        return Ok(());
    }
    let status = Command::new("cargo")
        .args(["build", "-p", package])
        .current_dir(workspace_root)
        .status()
        .map_err(|error| {
            CommandError::Infrastructure(format!("could not build {package}: {error}"))
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(CommandError::Infrastructure(format!(
            "cargo build -p {package} failed"
        )))
    }
}

fn copy_binary(source: &Path, destination: &Path) -> Result<(), CommandError> {
    fs::copy(source, destination).map_err(|error| {
        CommandError::Infrastructure(format!(
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
                CommandError::Infrastructure(format!(
                    "could not stat {}: {error}",
                    destination.display()
                ))
            })?
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(destination, permissions).map_err(|error| {
            CommandError::Infrastructure(format!(
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
    "loopback --capture <f32le> --stimulus <f32le> [--max-lag-frames <n>]"
}

pub(crate) fn usage_click_test() -> &'static str {
    "click-test --capture <f32le> [--fault <name>] [--max-derivative <value>]"
}

pub(crate) fn usage_midi_timing() -> &'static str {
    "midi-timing --events <coremidi-trace.json>"
}

pub(crate) fn usage_soak() -> &'static str {
    "soak [--hours <n>] --metrics <supervisor-report.json> [--max-memory-growth-percent <n>] [--max-memory-growth-bytes <n>]"
}

pub(crate) fn usage_bundle() -> &'static str {
    "bundle [--profile local-dev] [--sign] | bundle --profile release --build-number <n> --identity <Developer ID identity> --notary-profile <keychain profile>"
}

fn parse_option<'a>(arguments: &'a [String], name: &str) -> Option<&'a str> {
    arguments
        .windows(2)
        .find_map(|window| (window[0] == name).then_some(window[1].as_str()))
}

#[derive(Debug)]
struct CompatibilityCorpus {
    name: String,
    fixture_root: String,
    cases: Vec<CompatibilityCase>,
}

#[derive(Debug)]
struct CompatibilityCase {
    id: String,
    fixture: String,
    expected: String,
    mandatory: bool,
}

fn parse_compatibility_corpus(
    text: &str,
    path: &Path,
) -> Result<CompatibilityCorpus, CommandError> {
    let mut section = "root";
    let mut root = BTreeMap::new();
    let mut corpus = BTreeMap::new();
    let mut cases = Vec::new();
    let mut case = None;
    for (line_number, raw_line) in text.lines().enumerate() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line == "[[case]]" {
            if let Some(fields) = case.take() {
                cases.push(compatibility_case(&fields, path)?);
            }
            case = Some(BTreeMap::new());
            section = "case";
            continue;
        }
        if line == "[corpus]" {
            if case.is_some() {
                return Err(CommandError::InvalidConfiguration(format!(
                    "{}:{}: [corpus] must precede [[case]]",
                    path.display(),
                    line_number + 1
                )));
            }
            section = "corpus";
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(CommandError::InvalidConfiguration(format!(
                "{}:{}: expected key = value",
                path.display(),
                line_number + 1
            )));
        };
        let fields = match section {
            "root" => &mut root,
            "corpus" => &mut corpus,
            "case" => case.as_mut().expect("case section has fields"),
            _ => unreachable!(),
        };
        let key = key.trim().to_owned();
        if fields
            .insert(key.clone(), value.trim().to_owned())
            .is_some()
        {
            return Err(CommandError::InvalidConfiguration(format!(
                "{}:{}: duplicate `{key}`",
                path.display(),
                line_number + 1
            )));
        }
    }
    if let Some(fields) = case {
        cases.push(compatibility_case(&fields, path)?);
    }
    if root.keys().any(|key| key != "schema_version")
        || corpus
            .keys()
            .any(|key| key != "name" && key != "fixture_root")
    {
        return Err(CommandError::InvalidConfiguration(format!(
            "{}: unknown root or corpus field",
            path.display()
        )));
    }
    if root.get("schema_version").map(String::as_str) != Some("2") {
        return Err(CommandError::InvalidConfiguration(format!(
            "{}: schema_version must be 2",
            path.display()
        )));
    }
    let name = toml_string(corpus.get("name"), "corpus.name", path)?;
    let fixture_root = toml_string(corpus.get("fixture_root"), "corpus.fixture_root", path)?;
    if cases.is_empty() || !cases.iter().any(|case| case.mandatory) {
        return Err(CommandError::InvalidConfiguration(format!(
            "{}: at least one mandatory [[case]] is required",
            path.display()
        )));
    }
    Ok(CompatibilityCorpus {
        name,
        fixture_root,
        cases,
    })
}

fn compatibility_case(
    fields: &BTreeMap<String, String>,
    path: &Path,
) -> Result<CompatibilityCase, CommandError> {
    if fields
        .keys()
        .any(|key| !matches!(key.as_str(), "id" | "fixture" | "expected" | "mandatory"))
    {
        return Err(CommandError::InvalidConfiguration(format!(
            "{}: unknown case field",
            path.display()
        )));
    }
    let id = toml_string(fields.get("id"), "case.id", path)?;
    let fixture = toml_string(fields.get("fixture"), "case.fixture", path)?;
    if fixture.starts_with('/') || fixture.split('/').any(|component| component == "..") {
        return Err(CommandError::InvalidConfiguration(format!(
            "{}: case `{id}` fixture must be relative",
            path.display()
        )));
    }
    let expected = toml_string(fields.get("expected"), "case.expected", path)?;
    if expected != "pass" {
        return Err(CommandError::InvalidConfiguration(format!(
            "{}: case `{id}` expected must be `pass`",
            path.display()
        )));
    }
    let mandatory = fields.get("mandatory").map(String::as_str) == Some("true");
    if !mandatory {
        return Err(CommandError::InvalidConfiguration(format!(
            "{}: case `{id}` must set mandatory = true",
            path.display()
        )));
    }
    Ok(CompatibilityCase {
        id,
        fixture,
        expected,
        mandatory,
    })
}

fn toml_string(value: Option<&String>, field: &str, path: &Path) -> Result<String, CommandError> {
    value
        .and_then(|value| {
            value
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
        })
        .map(str::to_owned)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            CommandError::InvalidConfiguration(format!(
                "{}: `{field}` must be a non-empty TOML string",
                path.display()
            ))
        })
}

fn parse_f64_option(arguments: &[String], name: &str, default: f64) -> Result<f64, CommandError> {
    let value = parse_option(arguments, name)
        .map_or(Ok(default), str::parse::<f64>)
        .map_err(|_| {
            CommandError::InvalidConfiguration(format!(
                "{name} must be a finite non-negative number"
            ))
        })?;
    if value.is_finite() && value >= 0.0 {
        Ok(value)
    } else {
        Err(CommandError::InvalidConfiguration(format!(
            "{name} must be a finite non-negative number"
        )))
    }
}

fn parse_u64_option(arguments: &[String], name: &str, default: u64) -> Result<u64, CommandError> {
    parse_option(arguments, name)
        .map_or(Ok(default), str::parse)
        .map_err(|_| {
            CommandError::InvalidConfiguration(format!("{name} must be an unsigned integer"))
        })
}

fn read_f32le(path: &Path) -> Result<Vec<f32>, CommandError> {
    let bytes = fs::read(path).map_err(|error| {
        CommandError::Infrastructure(format!("could not read {}: {error}", path.display()))
    })?;
    if bytes.len() % 4 != 0 {
        return Err(CommandError::InvalidConfiguration(format!(
            "{} is not f32le audio",
            path.display()
        )));
    }
    let samples: Vec<f32> = bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect();
    if samples.iter().any(|sample| !sample.is_finite()) {
        return Err(CommandError::InvalidConfiguration(format!(
            "{} contains non-finite samples",
            path.display()
        )));
    }
    Ok(samples)
}

#[derive(Serialize)]
struct LoopbackMetrics {
    capture_samples: usize,
    stimulus_samples: usize,
    lag_frames: usize,
    correlation: f64,
}

fn correlate_loopback(
    capture_path: &Path,
    stimulus_path: &Path,
    arguments: &[String],
) -> Result<LoopbackMetrics, String> {
    let capture = read_f32le(capture_path).map_err(|error| error.to_string())?;
    let stimulus = read_f32le(stimulus_path).map_err(|error| error.to_string())?;
    if stimulus.is_empty() || stimulus.len() > 4096 || stimulus.len() > capture.len() {
        return Err("stimulus must contain 1..=4096 samples and fit within capture".to_owned());
    }
    let max_lag = usize::try_from(
        parse_u64_option(arguments, "--max-lag-frames", 96_000)
            .map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let mut best = (-1.0_f64, 0_usize);
    for lag in 0..=max_lag.min(capture.len() - stimulus.len()) {
        let (mut dot, mut captured_energy, mut stimulus_energy) = (0.0, 0.0, 0.0);
        for (captured, emitted) in capture[lag..lag + stimulus.len()].iter().zip(&stimulus) {
            let captured = f64::from(*captured);
            let emitted = f64::from(*emitted);
            dot += captured * emitted;
            captured_energy += captured * captured;
            stimulus_energy += emitted * emitted;
        }
        if captured_energy > 0.0 && stimulus_energy > 0.0 {
            let correlation = dot / (captured_energy * stimulus_energy).sqrt();
            if correlation > best.0 {
                best = (correlation, lag);
            }
        }
    }
    if best.0 < 0.0 {
        return Err("capture or stimulus has zero energy".to_owned());
    }
    Ok(LoopbackMetrics {
        capture_samples: capture.len(),
        stimulus_samples: stimulus.len(),
        lag_frames: best.1,
        correlation: best.0,
    })
}

#[allow(
    clippy::cast_precision_loss,
    reason = "sample counts stay far below f64 precision limits"
)]
fn derivative_metrics(samples: &[f32]) -> Option<(f64, f64)> {
    if samples.len() < 2 {
        return None;
    }
    let mut max = 0.0_f64;
    let mut squares = 0.0_f64;
    for pair in samples.windows(2) {
        let derivative = f64::from(pair[1] - pair[0]).abs();
        max = max.max(derivative);
        squares += derivative * derivative;
    }
    Some((max, (squares / (samples.len() - 1) as f64).sqrt()))
}

#[derive(Serialize)]
struct TimingStatistics {
    count: usize,
    min_micros: u64,
    mean_micros: f64,
    p50_micros: u64,
    p95_micros: u64,
    p99_micros: u64,
    max_micros: u64,
}

fn parse_midi_trace(path: &Path) -> Result<Vec<u64>, CommandError> {
    let bytes = fs::read(path).map_err(|error| {
        CommandError::Infrastructure(format!("could not read {}: {error}", path.display()))
    })?;
    let events: Value = serde_json::from_slice(&bytes).map_err(|error| {
        CommandError::InvalidConfiguration(format!("invalid MIDI trace: {error}"))
    })?;
    let events = events.as_array().ok_or_else(|| {
        CommandError::InvalidConfiguration("MIDI trace must be a JSON array".to_owned())
    })?;
    events
        .iter()
        .map(|event| {
            let object = event.as_object().ok_or_else(|| {
                CommandError::InvalidConfiguration("MIDI event must be an object".to_owned())
            })?;
            if object.get("source").and_then(Value::as_str) != Some("coremidi") {
                return Err(CommandError::InvalidConfiguration(
                    "MIDI event source must be `coremidi`".to_owned(),
                ));
            }
            let sent = object
                .get("sent_micros")
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    CommandError::InvalidConfiguration(
                        "MIDI event sent_micros must be u64".to_owned(),
                    )
                })?;
            let received = object
                .get("received_micros")
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    CommandError::InvalidConfiguration(
                        "MIDI event received_micros must be u64".to_owned(),
                    )
                })?;
            received.checked_sub(sent).ok_or_else(|| {
                CommandError::InvalidConfiguration(
                    "MIDI received_micros precedes sent_micros".to_owned(),
                )
            })
        })
        .collect()
}

#[allow(
    clippy::cast_precision_loss,
    reason = "latency samples in microseconds stay far below f64 precision limits"
)]
fn timing_statistics(latencies: &[u64]) -> Option<TimingStatistics> {
    let mut values = latencies.to_vec();
    values.sort_unstable();
    let count = values.len();
    let percentile = |numerator: usize| values[(count * numerator).div_ceil(100).saturating_sub(1)];
    Some(TimingStatistics {
        count,
        min_micros: *values.first()?,
        mean_micros: values.iter().map(|value| *value as f64).sum::<f64>() / count as f64,
        p50_micros: percentile(50),
        p95_micros: percentile(95),
        p99_micros: percentile(99),
        max_micros: *values.last()?,
    })
}

#[derive(Serialize)]
struct SoakEvidence {
    qualified: bool,
    duration_seconds: f64,
    processes: usize,
    maximum_observed_growth_bytes: u64,
    maximum_observed_growth_percent: f64,
    unexpected_faults: u64,
    failures: Vec<String>,
}

#[allow(
    clippy::too_many_lines,
    clippy::cast_precision_loss,
    reason = "one flat evidence sweep; byte counts are far below f64 precision limits"
)]
fn evaluate_soak_metrics(
    path: &Path,
    required_hours: f64,
    maximum_growth: u64,
    maximum_growth_percent: f64,
) -> Result<SoakEvidence, CommandError> {
    let bytes = fs::read(path).map_err(|error| {
        CommandError::Infrastructure(format!("could not read {}: {error}", path.display()))
    })?;
    let report: Value = serde_json::from_slice(&bytes).map_err(|error| {
        CommandError::InvalidConfiguration(format!("invalid soak metrics: {error}"))
    })?;
    let object = report.as_object().ok_or_else(|| {
        CommandError::InvalidConfiguration("soak metrics must be a JSON object".to_owned())
    })?;
    let mut failures = Vec::new();
    let supervised = object
        .get("supervised")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let attached = object
        .get("hardware_attached")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let acknowledged = object
        .get("operator_acknowledged")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let duration = object
        .get("duration_seconds")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    if !supervised {
        failures.push("supervised must be true".to_owned());
    }
    if !attached {
        failures.push("hardware_attached must be true".to_owned());
    }
    if !acknowledged {
        failures.push("operator_acknowledged must be true".to_owned());
    }
    if duration < required_hours * 3600.0 {
        failures.push("reported duration is shorter than requested soak".to_owned());
    }
    let unexpected_faults = object
        .get("faults")
        .and_then(Value::as_object)
        .and_then(|faults| faults.get("unexpected"))
        .and_then(Value::as_u64)
        .unwrap_or(u64::MAX);
    if unexpected_faults != 0 {
        failures.push("unexpected faults must be zero".to_owned());
    }
    let processes = object
        .get("processes")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            CommandError::InvalidConfiguration("soak metrics requires processes array".to_owned())
        })?;
    let mut maximum_observed_growth_bytes = 0;
    let mut maximum_observed_growth_percent = 0.0_f64;
    for process in processes {
        let process = process.as_object().ok_or_else(|| {
            CommandError::InvalidConfiguration("soak process must be an object".to_owned())
        })?;
        let pid = process.get("pid").and_then(Value::as_u64);
        let started = process.get("started").and_then(Value::as_bool);
        let exit_code = process.get("exit_code");
        let warmup = process.get("resident_bytes_warmup").and_then(Value::as_u64);
        let end = process.get("resident_bytes_end").and_then(Value::as_u64);
        if pid.is_none()
            || started != Some(true)
            || exit_code.is_none_or(|code| !code.is_null())
            || warmup.is_none()
            || end.is_none()
        {
            failures.push("each process requires pid, started=true, exit_code=null, and warm-up/end resident byte samples".to_owned());
            continue;
        }
        let warmup = warmup.expect("checked above");
        let growth = end.expect("checked above").saturating_sub(warmup);
        let growth_percent = if warmup == 0 {
            f64::MAX
        } else {
            growth as f64 / warmup as f64 * 100.0
        };
        maximum_observed_growth_bytes = maximum_observed_growth_bytes.max(growth);
        maximum_observed_growth_percent = maximum_observed_growth_percent.max(growth_percent);
        if growth > maximum_growth || growth_percent > maximum_growth_percent {
            failures.push(format!(
                "process {} exceeds memory-growth limit after warm-up",
                pid.expect("checked above")
            ));
        }
    }
    if processes.is_empty() {
        failures.push("at least one supervised process is required".to_owned());
    }
    Ok(SoakEvidence {
        qualified: failures.is_empty(),
        duration_seconds: duration,
        processes: processes.len(),
        maximum_observed_growth_bytes,
        maximum_observed_growth_percent,
        unexpected_faults,
        failures,
    })
}

fn write_json(path: &Path, value: &Value) -> Result<(), CommandError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            CommandError::Infrastructure(format!("could not create {}: {error}", parent.display()))
        })?;
    }
    let body = serde_json::to_vec_pretty(value)
        .map_err(|error| CommandError::Infrastructure(format!("could not encode JSON: {error}")))?;
    fs::write(path, body).map_err(|error| {
        CommandError::Infrastructure(format!("could not write {}: {error}", path.display()))
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::find_again_bundle;

    #[test]
    fn selects_only_the_exact_again_fixture() {
        let bundles = [
            PathBuf::from("VST3/again-simple.vst3"),
            PathBuf::from("VST3/again-sample-accurate.vst3"),
            PathBuf::from("VST3/again.vst3"),
        ];

        assert_eq!(
            find_again_bundle(&bundles),
            Some(PathBuf::from("VST3/again.vst3"))
        );
    }
}
