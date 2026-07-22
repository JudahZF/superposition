//! CoreAudio-attached Phase 1 IPC feasibility harness.
//!
//! Unlike the synthetic preflight, this command starts a real AUHAL output callback and performs
//! the shared-memory observe/dispatch cycle on that callback. The callback records only
//! fixed-size atomic outcomes and preallocated histograms; report construction, worker polling,
//! process accounting, energy-evidence parsing, and file publication stay on the control thread.
//!
//! A successful invocation is an active-device preflight. It never claims the Phase 1 hard gate:
//! certification additionally requires the full device matrix, calibrated-load evidence, and
//! fault-isolation evidence described in `docs/plan.md`.

use std::{
    collections::BTreeSet,
    fmt::Write as _,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sp_audio_io_macos::{
    ActiveOutput, CallbackTelemetry, DeviceFormatReport, InterleavedStereoF32, PhaseOneConfig,
    PhaseOneFrames, PhaseOneRenderer, RenderDisposition,
};
use sp_shared_memory::{BlockRequest, MAX_RACKS};
use sp_shared_memory_macos::{
    MonotonicClock, ProcessEnergySample, ProcessResourceUsage, child_process_resource_usage,
    current_process_resource_usage, sample_process_energy,
};
use sp_test_support::{
    COMPUTE_LOAD_MICROS_OPTION, COMPUTE_LOAD_MODE_OPTION, ComputeLoadMode,
    FAULT_DELAY_MICROS_OPTION, FAULT_MODE_OPTION, FAULT_TRIGGER_SEQUENCE_OPTION, FaultMode,
    SELF_CRASH_AFTER_CLAIM_MODE, WORK_DURATION_MICROS_OPTION, parse_compute_load_configuration,
    parse_fault_configuration,
};

use crate::phase1::{
    self, DeviceBlockCounters, DeviceHarness, DeviceWorkerConfiguration, DeviceWorkerIdentity,
    Phase1Error, TimingHistograms, WorkerHeartbeatSnapshot, build_worker, ensure_phase1_platform,
    fixed_request, process_device_callback_block, start_device_harness,
};

const REPORT_VERSION: u32 = 1;
const SAMPLE_RATE_HZ: u32 = 48_000;
const CERTIFICATION_DURATION_SECONDS: u64 = 1_800;
const DISPATCH_P9999_LIMIT_MICROS: u64 = 150;
const DISPATCH_MAX_LIMIT_MICROS: u64 = 400;
const P9999_MINIMUM_SAMPLE_COUNT: u64 = 10_000;
const HISTOGRAM_MAX_MICROS: usize = 10_000;

/// Runs the complete active-device 1/2/4/8-rack by 128/256-frame matrix sequentially.
///
/// This is deliberately a control-plane loop: it starts exactly one AUHAL run at a time and
/// preserves each cell's raw report beneath the selected directory. The consolidated Phase 1
/// report remains the only command that can assert certification.
pub(crate) fn run_device_matrix(
    workspace_root: &Path,
    arguments: &[String],
) -> Result<phase1::CommandOutcome, Phase1Error> {
    ensure_phase1_platform()?;
    let options = parse_matrix_options(arguments).map_err(Phase1Error::InvalidConfiguration)?;
    let mut cells = Vec::new();
    let mut all_passed = true;
    for rack_count in [1, 2, 4, 8] {
        for frame_count in [128, 256] {
            let cell = format!("{rack_count}r-{frame_count}f");
            let mut cell_arguments = vec![
                "--racks".to_owned(),
                rack_count.to_string(),
                "--frames".to_owned(),
                frame_count.to_string(),
                "--duration-seconds".to_owned(),
                options.duration.as_secs().to_string(),
                "--output-dir".to_owned(),
                options.output_directory.join(&cell).display().to_string(),
            ];
            if let Some(energy_directory) = &options.energy_evidence_directory {
                cell_arguments.push("--energy-evidence".to_owned());
                cell_arguments.push(
                    energy_directory
                        .join(format!("{cell}.json"))
                        .display()
                        .to_string(),
                );
            }
            if options.require_energy_evidence {
                cell_arguments.push("--require-energy-evidence".to_owned());
            }
            let result = run_device_feasibility(workspace_root, &cell_arguments)?;
            let passed = result.exit_code == 0;
            all_passed &= passed;
            cells.push(serde_json::json!({
                "cell": cell,
                "exit_code": result.exit_code,
                "report": options.output_directory.join(format!("{rack_count}r-{frame_count}f/device-feasibility.json")),
                "passed": passed,
            }));
        }
    }
    let mut report = serde_json::json!({
        "report_version": REPORT_VERSION,
        "report_id": "",
        "artifact_kind": "device_matrix",
        "status": if all_passed { "passed" } else { "failed" },
        "duration_seconds": options.duration.as_secs(),
        "require_energy_evidence": options.require_energy_evidence,
        "cells": cells,
        "acceptance_passed": all_passed,
        "evidence_complete": all_passed,
        "active_device_callback": all_passed,
    });
    let model = serde_json::to_vec(&report).map_err(|error| {
        Phase1Error::Infrastructure(format!(
            "could not fingerprint device matrix report: {error}"
        ))
    })?;
    let report_id = format!("{:016x}", fnv1a64(&model));
    report["report_id"] = Value::String(report_id.clone());
    let json = serde_json::to_vec_pretty(&report).map_err(|error| {
        Phase1Error::Infrastructure(format!("could not serialize device matrix report: {error}"))
    })?;
    let markdown = format!(
        "# Phase 1 attached-device matrix\nreport_id: {report_id}\nstatus: {}\nduration seconds per cell: {}\n",
        if all_passed { "passed" } else { "failed" },
        options.duration.as_secs(),
    )
    .into_bytes();
    let manifest = serde_json::to_vec_pretty(&artifact_manifest(
        REPORT_VERSION,
        &report_id,
        "device-matrix.json",
        "device-matrix.md",
        &json,
        &markdown,
    ))
    .map_err(|error| {
        Phase1Error::Infrastructure(format!(
            "could not serialize device matrix manifest: {error}"
        ))
    })?;
    publish_artifact_set(
        &options.output_directory,
        "device-matrix",
        &report_id,
        &json,
        &markdown,
        &manifest,
    )?;
    println!(
        "DEVICE_IPC_MATRIX: passed={all_passed}, artifacts={}",
        options.output_directory.display()
    );
    if all_passed {
        Ok(phase1::CommandOutcome::passed())
    } else {
        Ok(phase1::CommandOutcome::acceptance_failure())
    }
}

/// Runs CoreAudio-attached IPC feasibility and writes device evidence.
#[allow(clippy::too_many_lines)]
pub(crate) fn run_device_feasibility(
    workspace_root: &Path,
    arguments: &[String],
) -> Result<phase1::CommandOutcome, Phase1Error> {
    ensure_phase1_platform()?;
    let options = parse_options(arguments).map_err(Phase1Error::InvalidConfiguration)?;
    let environment = collect_environment(workspace_root);
    let clock = MonotonicClock::new().map_err(|error| {
        Phase1Error::Infrastructure(format!("could not initialize continuous clock: {error}"))
    })?;
    let worker = build_worker(workspace_root, options.worker.needs_fault_injection())
        .map_err(Phase1Error::Infrastructure)?;
    let artifact_kind = device_artifact_kind(&options.worker);
    let workload = device_workload(&options.worker);
    // External power-meter imports remain useful for cross-checking, but Phase 1's certifying
    // energy evidence is collected internally from the host and every live worker PID below.
    let external_energy_evidence = load_external_energy_evidence(
        options.energy_evidence_path.as_deref(),
        options.rack_count,
        options.frame_count,
        &environment.hardware_model,
        workload,
    )?;
    let period = phase1::block_period(options.frame_count);
    let host_before = current_process_resource_usage().ok();
    let children_before = child_process_resource_usage().ok();
    let harness = start_device_harness(options.rack_count, 1, &worker, &options.worker)
        .map_err(Phase1Error::Infrastructure)?;
    let worker_provenance = WorkerProvenance::collect(&worker, harness.device_worker_identities());
    let energy_before =
        ProcessEnergyBaseline::capture(std::process::id(), &worker_provenance.worker_process_ids);
    let (harness, mut worker_monitor) = harness
        .into_device_parts()
        .map_err(Phase1Error::Infrastructure)?;
    let request = fixed_request(options.frame_count);
    let frames = phase_one_frames(options.frame_count)?;
    let shared = Arc::new(SharedDeviceStats::new(options.worker.target_rack));
    let stop = Arc::new(AtomicBool::new(false));
    let timing_capture = Arc::new(TimingCapture::default());
    let renderer = DeviceIpcRenderer {
        harness,
        clock,
        request,
        completion_budget_ticks: clock.duration_to_ticks(period.mul_f64(0.75)),
        callback_period_ticks: clock.duration_to_ticks(period),
        block_index: 0,
        timing: TimingHistograms::default(),
        callback_duration: RealtimeHistogram::new(),
        timing_capture: Arc::clone(&timing_capture),
        stats: Arc::clone(&shared),
        stop: Arc::clone(&stop),
    };

    let config = PhaseOneConfig::new(frames).allow_device_reconfiguration();
    let mut output = ActiveOutput::start(config, renderer).map_err(|error| {
        Phase1Error::Infrastructure(format!(
            "could not start CoreAudio output: {error}. \
             device-feasibility needs a default output that can run fixed 48 kHz stereo callbacks \
             at {} frames (BlackHole or a reconfigurable hardware device). \
             This failure is infrastructure, not active-device acceptance evidence.",
            options.frame_count
        ))
    })?;
    let device_format = output.device_format();

    let started = Instant::now();
    let mut monitor_error = None;
    while started.elapsed() < options.duration {
        // Poll before observing the fatal latch so an intentional self-crash is retained as
        // control-plane exit evidence even when the callback already selected next-block
        // fallback from its pending request.
        if let Err(error) = worker_monitor.poll() {
            shared.record_control_error(CallbackErrorCode::WorkerMonitorFailure);
            monitor_error = Some(error);
            break;
        }
        if shared.fatal_error.load(Ordering::Acquire) {
            break;
        }
        // A control-plane exit sample must arrive before the next 128-frame callback (2.67 ms)
        // when an injected worker crashes. The callback only reads this atomic flag; it never
        // polls a process itself.
        thread::sleep(Duration::from_millis(1));
    }
    let observed_duration = started.elapsed();
    stop.store(true, Ordering::Release);
    let stop_error = output.stop().err().map(|error| error.to_string());
    // Retain one last non-real-time liveness/heartbeat sample and capture PID-scoped energy
    // while the worker handles still exist. Reaping before the energy read loses the target
    // process on deliberately crashing fault runs.
    if monitor_error.is_none() {
        monitor_error = worker_monitor.poll().err();
    }
    let heartbeat_snapshots = worker_monitor.heartbeat_snapshots();
    let control_plane_worker_exits = worker_monitor.observed_exit_flags();
    let energy_after =
        ProcessEnergyBaseline::capture(std::process::id(), &worker_provenance.worker_process_ids);
    let cleanup_error = worker_monitor.stop_and_reap().err();
    let telemetry = output.telemetry();
    let callback_stats = shared.snapshot();
    let timing = timing_capture.take();
    let timing_capture_error = timing_capture.error_code.load(Ordering::Acquire);
    let timing_evidence = timing.map(TimingEvidence::from_snapshot);
    let timing_thresholds =
        timing_evidence
            .as_ref()
            .map_or_else(TimingThresholds::unavailable, |evidence| {
                TimingThresholds::evaluate(
                    evidence,
                    period,
                    options.duration >= Duration::from_secs(CERTIFICATION_DURATION_SECONDS),
                )
            });
    let cpu_evidence = CpuEvidence::new(
        host_before,
        current_process_resource_usage().ok(),
        children_before,
        child_process_resource_usage().ok(),
    );

    let attached = callback_stats.callbacks > 0 && telemetry.callbacks > 0;
    let calibrated_load = CalibratedLoadEvidence::from_snapshots(
        options.worker.compute_load.mode,
        options.frame_count,
        clock,
        &heartbeat_snapshots,
    );
    let heartbeat = HeartbeatEvidence::from_snapshots(heartbeat_snapshots);
    let expected_departed_worker_process_id = options.worker.self_crash_after_claim.then(|| {
        options
            .worker
            .target_rack
            .and_then(|rack| worker_provenance.worker_process_ids.get(rack).copied())
            .expect("self-crash configuration has a validated target worker PID")
    });
    let energy_evidence = EnergyEvidence::from_process_samples(
        energy_before,
        energy_after,
        observed_duration,
        expected_departed_worker_process_id,
        options.require_energy_evidence,
        external_energy_evidence,
    );
    let mut acceptance_failures = acceptance_failures(
        attached,
        device_format,
        frames,
        telemetry,
        &callback_stats,
        &timing_thresholds,
        timing_capture_error,
    );
    if attached && !heartbeat.all_workers_progressed {
        acceptance_failures.push(
            "one or more mapped workers did not publish a healthy periodic heartbeat progression"
                .to_owned(),
        );
    }
    if !energy_evidence.certification_complete {
        acceptance_failures.push(
            "internal host/worker proc_pid_rusage energy accounting is incomplete".to_owned(),
        );
    }
    if options.worker.compute_load.mode == ComputeLoadMode::CalibratedCpu
        && !calibrated_load.complete
    {
        acceptance_failures
            .push("calibrated worker busy-duration evidence is incomplete".to_owned());
    }
    if let Some(error) = &monitor_error {
        acceptance_failures.push(format!("worker monitor failed: {error}"));
    }
    if let Some(error) = &stop_error {
        acceptance_failures.push(format!("AUHAL stop failed: {error}"));
    }
    if let Some(error) = &cleanup_error {
        acceptance_failures.push(format!("worker cleanup failed: {error}"));
    }
    let infrastructure_error = monitor_error.or(stop_error).or(cleanup_error);
    let fault_isolation = FaultIsolationEvidence::evaluate(
        &options.worker,
        attached,
        &callback_stats,
        &control_plane_worker_exits,
        options.rack_count,
    );
    if fault_isolation
        .as_ref()
        .is_some_and(|evidence| !evidence.passed)
    {
        acceptance_failures
            .push("target fault was not contained to its rack with next-block fallback".to_owned());
    }
    let acceptance_passed = acceptance_failures.is_empty();
    // `--require-energy-evidence` is retained as a backwards-compatible spelling. It now means
    // the internally sampled process-energy contract must be complete; an imported manual file
    // is never a prerequisite for a qualifying artifact.
    let evidence_complete = energy_evidence.certification_complete;
    let mut report = DeviceFeasibilityReport {
        report_version: REPORT_VERSION,
        report_id: String::new(),
        artifact_kind,
        generated_unix_seconds: unix_seconds(),
        labels: DeviceReportLabels::for_duration(options.duration, attached, acceptance_passed),
        environment,
        device: DeviceProvenance::from_format(device_format),
        configuration: DeviceConfiguration {
            rack_count: options.rack_count,
            frame_count: options.frame_count,
            sample_rate_hz: SAMPLE_RATE_HZ,
            requested_duration_seconds: options.duration.as_secs(),
            observed_duration_micros: duration_to_micros_ceil(observed_duration),
            block_period_micros: duration_to_micros_ceil(period),
            output_disposition: "silence",
            workload,
            compute_load_mode: options.worker.compute_load.mode.as_str(),
            compute_load_micros: u64::try_from(options.worker.compute_load.duration.as_micros())
                .unwrap_or(u64::MAX),
            fault_mode: if options.worker.self_crash_after_claim {
                SELF_CRASH_AFTER_CLAIM_MODE
            } else {
                options.worker.fault.mode.as_str()
            },
            fault_target_rack: options.worker.target_rack,
            fault_trigger_sequence: options.worker.fault.trigger_request_sequence,
        },
        workers: worker_provenance,
        callback_telemetry: DeviceCallbackTelemetry::from(telemetry),
        callback_stats,
        fault_isolation,
        timing: timing_evidence,
        timing_thresholds,
        heartbeat,
        calibrated_load,
        cpu_evidence,
        energy_evidence,
        acceptance_passed,
        evidence_complete,
        acceptance_failures,
        infrastructure_error: infrastructure_error.clone(),
        limitations: device_limitations(&options.worker),
    };
    assign_report_id(&mut report)?;
    write_report(&options.output_directory, &report)?;
    print_summary(&report, &options.output_directory);

    if let Some(error) = infrastructure_error {
        return Err(Phase1Error::Infrastructure(error));
    }
    if !acceptance_passed {
        return Ok(phase1::CommandOutcome::acceptance_failure());
    }
    if !evidence_complete {
        return Ok(phase1::CommandOutcome::evidence_incomplete());
    }
    Ok(phase1::CommandOutcome::passed())
}

fn device_artifact_kind(configuration: &DeviceWorkerConfiguration) -> &'static str {
    if configuration.self_crash_after_claim || configuration.fault.mode != FaultMode::None {
        "fault_isolation"
    } else if configuration.compute_load.mode == ComputeLoadMode::CalibratedCpu {
        "calibrated_load"
    } else if configuration.bundle.is_some() {
        "active_device_vst3_smoke"
    } else {
        "active_device_cell"
    }
}

fn device_workload(configuration: &DeviceWorkerConfiguration) -> &'static str {
    match device_artifact_kind(configuration) {
        "fault_isolation" => "device_fault_isolation",
        "calibrated_load" => "calibrated_cpu",
        "active_device_vst3_smoke" => "device_vst3_smoke",
        "active_device_cell" => "device_feasibility",
        _ => unreachable!("device artifact kind is exhaustive"),
    }
}

fn device_limitations(configuration: &DeviceWorkerConfiguration) -> Vec<String> {
    let mut limitations = vec![
        "The device command measures one requested rack/frame cell; it does not certify the required 1/2/4/8 by 128/256 device matrix.".to_owned(),
        "Internal energy is sampled before and after the attached window with proc_pid_rusage for the host and every worker PID; optional external imports are cross-checks only.".to_owned(),
        "A control-thread monitor reads a separate mapping for every worker and requires monotonic heartbeat progression without borrowing callback-owned mapping state.".to_owned(),
        "The AUHAL client stream is fixed stereo and maps to physical default-output channels 1–2; channel_count records the device's full physical output count.".to_owned(),
    ];
    if configuration.bundle.is_none() {
        limitations.push(
            "This attached harness does not load a VST3 bundle; real-VST3 smoke evidence is collected by host-checker through scanner and worker helpers.".to_owned(),
        );
    } else {
        limitations.push(
            "The attached VST3 mode is a non-certifying device smoke; Phase 1 certification continues to use the required no-op matrix and separate scanner-first VST3 artifact.".to_owned(),
        );
    }
    limitations
}

pub(crate) fn device_feasibility_usage() -> &'static str {
    "usage: cargo xtask device-feasibility -r|--racks <1|2|4|8> -f|--frames <128|256> -d|--duration-seconds <seconds> [-o|--output-dir <directory>] [-e|--energy-evidence <structured-json>] [--require-energy-evidence] [--bundle <vst3-bundle>] [--compute-load-mode <none|calibrated-cpu> --compute-load-micros <1..=60000000>] [--fault-mode <none|self-crash|hang-after-claim|delay-before-claim|late-completion|malformed-completion|stale-completion> --fault-target-rack <1..=racks> --fault-trigger-sequence <n> --fault-delay-micros <n> --work-duration-micros <n>]"
}

pub(crate) fn device_matrix_usage() -> &'static str {
    "usage: cargo xtask device-matrix --duration-seconds <seconds> --output-dir <directory> [--energy-evidence-dir <directory> --require-energy-evidence]"
}

#[derive(Debug)]
struct DeviceOptions {
    rack_count: usize,
    frame_count: u32,
    duration: Duration,
    output_directory: PathBuf,
    energy_evidence_path: Option<PathBuf>,
    require_energy_evidence: bool,
    worker: DeviceWorkerConfiguration,
}

struct DeviceMatrixOptions {
    duration: Duration,
    output_directory: PathBuf,
    energy_evidence_directory: Option<PathBuf>,
    require_energy_evidence: bool,
}

fn parse_matrix_options(arguments: &[String]) -> Result<DeviceMatrixOptions, String> {
    let mut duration_seconds = None;
    let mut output_directory = None;
    let mut energy_evidence_directory = None;
    let mut require_energy_evidence = false;
    let mut seen = BTreeSet::new();
    let mut index = 0;
    while index < arguments.len() {
        let option = arguments[index].as_str();
        if option == "--require-energy-evidence" {
            if !seen.insert("require-energy-evidence") {
                return Err(duplicate_option(option));
            }
            require_energy_evidence = true;
            index += 1;
            continue;
        }
        let value = argument_value(arguments, index, option)?;
        let name = match option {
            "--duration-seconds" => "duration-seconds",
            "--output-dir" => "output-dir",
            "--energy-evidence-dir" => "energy-evidence-dir",
            _ => return Err(device_matrix_usage().to_owned()),
        };
        if !seen.insert(name) {
            return Err(duplicate_option(option));
        }
        match name {
            "duration-seconds" => duration_seconds = Some(parse_duration_seconds(value)?),
            "output-dir" => output_directory = Some(PathBuf::from(value)),
            "energy-evidence-dir" => energy_evidence_directory = Some(PathBuf::from(value)),
            _ => unreachable!("matrix option names are fixed"),
        }
        index += 2;
    }
    if require_energy_evidence && energy_evidence_directory.is_none() {
        return Err("--require-energy-evidence requires --energy-evidence-dir".to_owned());
    }
    Ok(DeviceMatrixOptions {
        duration: Duration::from_secs(
            duration_seconds.ok_or_else(|| missing_option("--duration-seconds"))?,
        ),
        output_directory: output_directory.ok_or_else(|| missing_option("--output-dir"))?,
        energy_evidence_directory,
        require_energy_evidence,
    })
}

#[allow(clippy::too_many_lines)]
fn parse_options(arguments: &[String]) -> Result<DeviceOptions, String> {
    let mut rack_count = None;
    let mut frame_count = None;
    let mut duration_seconds = None;
    let mut output_directory = None;
    let mut energy_evidence_path = None;
    let mut require_energy_evidence = false;
    let mut bundle = None;
    let mut fault_target_rack = None;
    let mut self_crash_after_claim = false;
    let mut fault_arguments = Vec::new();
    let mut compute_load_arguments = Vec::new();
    let mut seen = BTreeSet::new();
    let mut index = 0;

    while index < arguments.len() {
        let option = arguments[index].as_str();
        if option == "--require-energy-evidence" {
            if !seen.insert("require-energy-evidence") {
                return Err(duplicate_option(option));
            }
            require_energy_evidence = true;
            index += 1;
            continue;
        }
        let value = argument_value(arguments, index, option)?;
        let name = match option {
            "-r" | "--racks" => "racks",
            "-f" | "--frames" => "frames",
            "-d" | "--duration-seconds" => "duration-seconds",
            "-o" | "--output-dir" => "output-dir",
            "-e" | "--energy-evidence" => "energy-evidence",
            "--bundle" => "bundle",
            "--fault-target-rack" => "fault-target-rack",
            FAULT_MODE_OPTION => "fault-mode",
            FAULT_TRIGGER_SEQUENCE_OPTION => "fault-trigger-sequence",
            FAULT_DELAY_MICROS_OPTION => "fault-delay-micros",
            WORK_DURATION_MICROS_OPTION => "work-duration-micros",
            COMPUTE_LOAD_MODE_OPTION => "compute-load-mode",
            COMPUTE_LOAD_MICROS_OPTION => "compute-load-micros",
            _ => {
                return Err(format!(
                    "unknown option `{option}`\n\n{}",
                    device_feasibility_usage()
                ));
            }
        };
        if !seen.insert(name) {
            return Err(duplicate_option(option));
        }
        match name {
            "racks" => rack_count = Some(parse_racks(value)?),
            "frames" => frame_count = Some(parse_frames(value)?),
            "duration-seconds" => duration_seconds = Some(parse_duration_seconds(value)?),
            "output-dir" => output_directory = Some(PathBuf::from(value)),
            "energy-evidence" => energy_evidence_path = Some(PathBuf::from(value)),
            "bundle" => bundle = Some(PathBuf::from(value)),
            "fault-target-rack" => fault_target_rack = Some(parse_fault_target_rack(value)?),
            "fault-mode" if value == SELF_CRASH_AFTER_CLAIM_MODE => {
                self_crash_after_claim = true;
            }
            "fault-mode"
            | "fault-trigger-sequence"
            | "fault-delay-micros"
            | "work-duration-micros" => {
                fault_arguments.push(option.to_owned());
                fault_arguments.push(value.to_owned());
            }
            "compute-load-mode" | "compute-load-micros" => {
                compute_load_arguments.push(option.to_owned());
                compute_load_arguments.push(value.to_owned());
            }
            _ => unreachable!("option names are fixed above"),
        }
        index += 2;
    }

    let output_directory = output_directory.unwrap_or_else(|| {
        PathBuf::from("target")
            .join("phase1")
            .join("device-feasibility")
    });
    if output_directory.as_os_str().is_empty() {
        return Err("--output-dir must not be empty".to_owned());
    }
    let rack_count = rack_count.ok_or_else(|| missing_option("--racks"))?;
    let fault = parse_fault_configuration(fault_arguments).map_err(|error| error.to_string())?;
    let compute_load = parse_compute_load_configuration(compute_load_arguments)
        .map_err(|error| error.to_string())?;
    if self_crash_after_claim && fault.mode != FaultMode::None {
        return Err("self-crash cannot be combined with another --fault-mode".to_owned());
    }
    let fault_enabled = self_crash_after_claim || fault.mode != FaultMode::None;
    if fault_enabled && compute_load.mode != ComputeLoadMode::None {
        return Err(
            "calibrated load and fault injection must be qualified in separate runs".to_owned(),
        );
    }
    if bundle.is_some() && fault_enabled {
        return Err("--bundle cannot be combined with fault injection".to_owned());
    }
    if bundle.is_some() && compute_load.mode != ComputeLoadMode::None {
        return Err("--bundle cannot be combined with calibrated synthetic load".to_owned());
    }
    if let Some(path) = &bundle
        && !path.is_dir()
    {
        return Err(format!(
            "--bundle must name an existing VST3 bundle directory: {}",
            path.display()
        ));
    }
    if fault_enabled && fault.trigger_request_sequence < 2 {
        return Err("fault isolation requires --fault-trigger-sequence >= 2 so every rack has a baseline completion".to_owned());
    }
    if fault_enabled && fault_target_rack.is_none() {
        return Err("a non-default fault requires --fault-target-rack <1..=racks>".to_owned());
    }
    let target_rack = fault_target_rack
        .map(|one_based| {
            if one_based > rack_count {
                Err(format!(
                    "--fault-target-rack must be within 1..={rack_count}; got {one_based}"
                ))
            } else {
                Ok(one_based - 1)
            }
        })
        .transpose()?;
    if !fault_enabled && target_rack.is_some() {
        return Err("--fault-target-rack requires a non-default fault mode".to_owned());
    }
    let frame_count = frame_count.ok_or_else(|| missing_option("--frames"))?;
    if compute_load.mode == ComputeLoadMode::CalibratedCpu {
        let target = calibrated_target_micros(frame_count)
            .expect("validated Phase 1 frame count has a calibrated target");
        let observed = u64::try_from(compute_load.duration.as_micros()).unwrap_or(u64::MAX);
        if observed != target {
            return Err(format!(
                "calibrated-cpu uses the deterministic {target} us target at {frame_count} frames; got {observed} us"
            ));
        }
    }
    Ok(DeviceOptions {
        rack_count,
        frame_count,
        duration: Duration::from_secs(
            duration_seconds.ok_or_else(|| missing_option("--duration-seconds"))?,
        ),
        output_directory,
        energy_evidence_path,
        require_energy_evidence,
        worker: DeviceWorkerConfiguration {
            fault,
            self_crash_after_claim,
            compute_load,
            bundle,
            target_rack,
        },
    })
}

fn parse_fault_target_rack(value: &str) -> Result<usize, String> {
    let value = value
        .parse::<usize>()
        .map_err(|_| format!("invalid --fault-target-rack `{value}`"))?;
    (value > 0)
        .then_some(value)
        .ok_or_else(|| "--fault-target-rack must be positive".to_owned())
}

fn argument_value<'a>(
    arguments: &'a [String],
    index: usize,
    option: &str,
) -> Result<&'a str, String> {
    arguments.get(index + 1).map(String::as_str).ok_or_else(|| {
        format!(
            "missing value for `{option}`\n\n{}",
            device_feasibility_usage()
        )
    })
}

fn duplicate_option(option: &str) -> String {
    format!(
        "duplicate option `{option}`\n\n{}",
        device_feasibility_usage()
    )
}

fn missing_option(option: &str) -> String {
    format!(
        "missing required `{option}`\n\n{}",
        device_feasibility_usage()
    )
}

fn parse_racks(value: &str) -> Result<usize, String> {
    match value {
        "1" | "2" | "4" | "8" => value.parse().map_err(|_| unreachable!()),
        _ => Err(format!("--racks must be 1, 2, 4, or 8; got `{value}`")),
    }
}

fn parse_frames(value: &str) -> Result<u32, String> {
    match value {
        "128" | "256" => value.parse().map_err(|_| unreachable!()),
        _ => Err(format!("--frames must be 128 or 256; got `{value}`")),
    }
}

/// Fixed calibrated worker target: 10% of the fixed 48 kHz block period, rounded upward to one
/// microsecond. This produces a reproducible 267 us target at 128 frames and 534 us at 256.
const fn calibrated_target_micros(frame_count: u32) -> Option<u64> {
    match frame_count {
        128 => Some(267),
        256 => Some(534),
        _ => None,
    }
}

fn parse_duration_seconds(value: &str) -> Result<u64, String> {
    let seconds = value
        .parse::<u64>()
        .map_err(|_| format!("invalid --duration-seconds `{value}`"))?;
    (seconds > 0)
        .then_some(seconds)
        .ok_or_else(|| "--duration-seconds must be positive".to_owned())
}

fn phase_one_frames(frame_count: u32) -> Result<PhaseOneFrames, Phase1Error> {
    match frame_count {
        128 => Ok(PhaseOneFrames::Frames128),
        256 => Ok(PhaseOneFrames::Frames256),
        other => Err(Phase1Error::InvalidConfiguration(format!(
            "unsupported frame count {other}"
        ))),
    }
}

/// Fixed numeric callback outcome codes; no diagnostic text is constructed on the audio thread.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CallbackErrorCode {
    None = 0,
    DeadlineMiss = 1,
    CallbackOverrun = 2,
    ProtocolFault = 3,
    WorkerExited = 4,
    WorkerMonitorFailure = 5,
    TimingCaptureFailure = 6,
}

impl CallbackErrorCode {
    const fn label(code: u32) -> &'static str {
        match code {
            0 => "none",
            1 => "deadline_miss",
            2 => "callback_overrun",
            3 => "protocol_fault",
            4 => "worker_exited",
            5 => "worker_monitor_failure",
            6 => "timing_capture_failure",
            _ => "unknown_callback_error",
        }
    }
}

/// Control-thread shared state. The callback only mutates its atomics.
struct SharedDeviceStats {
    callbacks: AtomicU64,
    accepted_completions: AtomicU64,
    accepted_completions_by_rack: [AtomicU64; MAX_RACKS],
    deadline_misses: AtomicU64,
    deadline_misses_by_rack: [AtomicU64; MAX_RACKS],
    fallback_events: AtomicU64,
    callback_overruns: AtomicU64,
    protocol_faults: AtomicU64,
    protocol_faults_by_rack: [AtomicU64; MAX_RACKS],
    worker_exits: AtomicU64,
    worker_exits_by_rack: [AtomicU64; MAX_RACKS],
    first_fallback_block_plus_one: [AtomicU64; MAX_RACKS],
    fatal_error_events: AtomicU64,
    first_error_code: AtomicU32,
    last_error_code: AtomicU32,
    fatal_error: AtomicBool,
    // The callback never parses fault configuration. This precomputed index lets it retain the
    // target rack's expected containment event while still latching any unrelated rack failure.
    expected_fault_target: Option<usize>,
}

impl Default for SharedDeviceStats {
    fn default() -> Self {
        Self::new(None)
    }
}

impl SharedDeviceStats {
    fn new(expected_fault_target: Option<usize>) -> Self {
        Self {
            callbacks: AtomicU64::new(0),
            accepted_completions: AtomicU64::new(0),
            accepted_completions_by_rack: std::array::from_fn(|_| AtomicU64::new(0)),
            deadline_misses: AtomicU64::new(0),
            deadline_misses_by_rack: std::array::from_fn(|_| AtomicU64::new(0)),
            fallback_events: AtomicU64::new(0),
            callback_overruns: AtomicU64::new(0),
            protocol_faults: AtomicU64::new(0),
            protocol_faults_by_rack: std::array::from_fn(|_| AtomicU64::new(0)),
            worker_exits: AtomicU64::new(0),
            worker_exits_by_rack: std::array::from_fn(|_| AtomicU64::new(0)),
            first_fallback_block_plus_one: std::array::from_fn(|_| AtomicU64::new(0)),
            fatal_error_events: AtomicU64::new(0),
            first_error_code: AtomicU32::new(CallbackErrorCode::None as u32),
            last_error_code: AtomicU32::new(CallbackErrorCode::None as u32),
            fatal_error: AtomicBool::new(false),
            expected_fault_target,
        }
    }

    fn record_callback_outcome(&self, counters: &DeviceBlockCounters) {
        self.accepted_completions
            .fetch_add(counters.accepted_racks as u64, Ordering::Release);
        for (total, accepted) in self
            .accepted_completions_by_rack
            .iter()
            .zip(counters.accepted_by_rack.iter())
        {
            total.fetch_add(*accepted, Ordering::Release);
        }
        self.deadline_misses
            .fetch_add(counters.deadline_misses, Ordering::Release);
        for (total, observed) in self
            .deadline_misses_by_rack
            .iter()
            .zip(counters.deadline_misses_by_rack.iter())
        {
            total.fetch_add(*observed, Ordering::Release);
        }
        self.fallback_events
            .fetch_add(counters.fallback_events, Ordering::Release);
        for (recorded, observed) in self
            .first_fallback_block_plus_one
            .iter()
            .zip(counters.first_fallback_block_plus_one.iter())
        {
            if *observed != 0 {
                let _ =
                    recorded.compare_exchange(0, *observed, Ordering::AcqRel, Ordering::Acquire);
            }
        }
        self.protocol_faults
            .fetch_add(counters.protocol_faults, Ordering::Release);
        for (total, observed) in self
            .protocol_faults_by_rack
            .iter()
            .zip(counters.protocol_faults_by_rack.iter())
        {
            total.fetch_add(*observed, Ordering::Release);
        }
        self.worker_exits
            .fetch_add(counters.worker_exits, Ordering::Release);
        for (total, observed) in self
            .worker_exits_by_rack
            .iter()
            .zip(counters.worker_exits_by_rack.iter())
        {
            total.fetch_add(*observed, Ordering::Release);
        }
        if self.unexpected_rack_event(&counters.deadline_misses_by_rack) {
            self.record_callback_error(CallbackErrorCode::DeadlineMiss);
        }
        if self.unexpected_rack_event(&counters.protocol_faults_by_rack) {
            self.record_callback_error(CallbackErrorCode::ProtocolFault);
        }
        if self.unexpected_rack_event(&counters.worker_exits_by_rack) {
            self.record_callback_error(CallbackErrorCode::WorkerExited);
        }
    }

    fn unexpected_rack_event(&self, events: &[u64; MAX_RACKS]) -> bool {
        events
            .iter()
            .copied()
            .enumerate()
            .any(|(rack, count)| count != 0 && self.expected_fault_target != Some(rack))
    }

    fn record_callback_overrun(&self) {
        self.callback_overruns.fetch_add(1, Ordering::Release);
        self.record_callback_error(CallbackErrorCode::CallbackOverrun);
    }

    fn record_callback_error(&self, code: CallbackErrorCode) {
        self.fatal_error_events.fetch_add(1, Ordering::Release);
        let _ = self.first_error_code.compare_exchange(
            CallbackErrorCode::None as u32,
            code as u32,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        self.last_error_code.store(code as u32, Ordering::Release);
        self.fatal_error.store(true, Ordering::Release);
    }

    fn record_control_error(&self, code: CallbackErrorCode) {
        self.record_callback_error(code);
    }

    fn snapshot(&self) -> CallbackStats {
        CallbackStats {
            callbacks: self.callbacks.load(Ordering::Acquire),
            accepted_completions: self.accepted_completions.load(Ordering::Acquire),
            accepted_completions_by_rack: self
                .accepted_completions_by_rack
                .each_ref()
                .map(|counter| counter.load(Ordering::Acquire)),
            deadline_misses: self.deadline_misses.load(Ordering::Acquire),
            deadline_misses_by_rack: self
                .deadline_misses_by_rack
                .each_ref()
                .map(|counter| counter.load(Ordering::Acquire)),
            fallback_events: self.fallback_events.load(Ordering::Acquire),
            callback_overruns: self.callback_overruns.load(Ordering::Acquire),
            protocol_faults: self.protocol_faults.load(Ordering::Acquire),
            protocol_faults_by_rack: self
                .protocol_faults_by_rack
                .each_ref()
                .map(|counter| counter.load(Ordering::Acquire)),
            worker_exits: self.worker_exits.load(Ordering::Acquire),
            worker_exits_by_rack: self
                .worker_exits_by_rack
                .each_ref()
                .map(|counter| counter.load(Ordering::Acquire)),
            first_fallback_block_by_rack: self
                .first_fallback_block_plus_one
                .each_ref()
                .map(|counter| counter.load(Ordering::Acquire).checked_sub(1)),
            fatal_error_events: self.fatal_error_events.load(Ordering::Acquire),
            first_error_code: self.first_error_code.load(Ordering::Acquire),
            first_error_label: CallbackErrorCode::label(
                self.first_error_code.load(Ordering::Acquire),
            ),
            last_error_code: self.last_error_code.load(Ordering::Acquire),
            last_error_label: CallbackErrorCode::label(
                self.last_error_code.load(Ordering::Acquire),
            ),
            fatal_error: self.fatal_error.load(Ordering::Acquire),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct CallbackStats {
    callbacks: u64,
    accepted_completions: u64,
    accepted_completions_by_rack: [u64; MAX_RACKS],
    deadline_misses: u64,
    deadline_misses_by_rack: [u64; MAX_RACKS],
    fallback_events: u64,
    callback_overruns: u64,
    protocol_faults: u64,
    protocol_faults_by_rack: [u64; MAX_RACKS],
    worker_exits: u64,
    worker_exits_by_rack: [u64; MAX_RACKS],
    first_fallback_block_by_rack: [Option<u64>; MAX_RACKS],
    fatal_error_events: u64,
    first_error_code: u32,
    first_error_label: &'static str,
    last_error_code: u32,
    last_error_label: &'static str,
    fatal_error: bool,
}

/// Evidence for one intentionally faulted worker while the other device-attached workers advance.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Serialize)]
struct FaultIsolationEvidence {
    target_rack: usize,
    fault_mode: &'static str,
    active_device_callback: bool,
    target_fault_observed: bool,
    control_plane_worker_exit_observed: bool,
    unaffected_racks_continued: bool,
    unaffected_racks_fault_free: bool,
    fault_trigger_sequence: u64,
    first_target_fallback_block: Option<u64>,
    fallback_by_current_or_next_block: bool,
    fallback_events: u64,
    deadline_misses: u64,
    protocol_faults: u64,
    worker_exits: u64,
    passed: bool,
}

impl FaultIsolationEvidence {
    fn evaluate(
        configuration: &DeviceWorkerConfiguration,
        active_device_callback: bool,
        stats: &CallbackStats,
        control_plane_worker_exits: &[bool],
        rack_count: usize,
    ) -> Option<Self> {
        let target_rack = configuration.target_rack?;
        let fault_mode = if configuration.self_crash_after_claim {
            SELF_CRASH_AFTER_CLAIM_MODE
        } else {
            configuration.fault.mode.as_str()
        };
        let control_plane_worker_exit_observed = control_plane_worker_exits
            .get(target_rack)
            .copied()
            .unwrap_or(false);
        let target_fault_observed = match fault_mode {
            SELF_CRASH_AFTER_CLAIM_MODE => control_plane_worker_exit_observed,
            "hang-after-claim" | "late-completion" | "delay-before-claim" => stats
                .deadline_misses_by_rack
                .get(target_rack)
                .is_some_and(|count| *count > 0),
            "malformed-completion" | "stale-completion" => stats
                .protocol_faults_by_rack
                .get(target_rack)
                .is_some_and(|count| *count > 0),
            _ => false,
        };
        let unaffected_racks_continued = (0..rack_count)
            .filter(|rack_index| *rack_index != target_rack)
            .all(|rack_index| {
                stats
                    .accepted_completions_by_rack
                    .get(rack_index)
                    .is_some_and(|accepted| *accepted > 0)
            });
        let unaffected_racks_fault_free = (0..rack_count)
            .filter(|rack_index| *rack_index != target_rack)
            .all(|rack_index| {
                stats.deadline_misses_by_rack[rack_index] == 0
                    && stats.protocol_faults_by_rack[rack_index] == 0
                    && stats.worker_exits_by_rack[rack_index] == 0
            });
        // Sequence one is dispatched on callback block zero. The fault trigger sequence is
        // therefore dispatched on `sequence - 1`, and the gate must select fallback no later
        // than the following callback block (`sequence`). This is captured from callback-owned
        // counters after AUHAL retirement, not inferred from a control-thread timestamp.
        let fault_trigger_sequence = configuration.fault.trigger_request_sequence;
        let first_target_fallback_block = stats
            .first_fallback_block_by_rack
            .get(target_rack)
            .copied()
            .flatten();
        let fallback_by_current_or_next_block =
            first_target_fallback_block.is_some_and(|block| block <= fault_trigger_sequence);
        let passed = active_device_callback
            && target_fault_observed
            && unaffected_racks_continued
            && unaffected_racks_fault_free
            && fallback_by_current_or_next_block
            && stats.fallback_events > 0
            && stats.callback_overruns == 0;
        Some(Self {
            target_rack,
            fault_mode,
            active_device_callback,
            target_fault_observed,
            control_plane_worker_exit_observed,
            unaffected_racks_continued,
            unaffected_racks_fault_free,
            fault_trigger_sequence,
            first_target_fallback_block,
            fallback_by_current_or_next_block,
            fallback_events: stats.fallback_events,
            deadline_misses: stats.deadline_misses,
            protocol_faults: stats.protocol_faults,
            worker_exits: stats.worker_exits,
            passed,
        })
    }
}

/// `TimingHistograms` stays callback-owned until AUHAL proves callback retirement. Its Drop
/// implementation runs when `ActiveOutput::stop` drops the renderer on the control thread.
#[derive(Default)]
struct TimingCapture {
    snapshot: Mutex<Option<CapturedTiming>>,
    error_code: AtomicU32,
}

impl TimingCapture {
    fn take(&self) -> Option<CapturedTiming> {
        self.snapshot.lock().ok()?.take()
    }

    fn publish(&self, snapshot: CapturedTiming) {
        match self.snapshot.lock() {
            Ok(mut capture) => *capture = Some(snapshot),
            Err(_) => self.error_code.store(
                CallbackErrorCode::TimingCaptureFailure as u32,
                Ordering::Release,
            ),
        }
    }
}

struct DeviceIpcRenderer {
    harness: DeviceHarness,
    clock: MonotonicClock,
    request: BlockRequest,
    completion_budget_ticks: u64,
    callback_period_ticks: u64,
    block_index: u64,
    timing: TimingHistograms,
    callback_duration: RealtimeHistogram,
    timing_capture: Arc<TimingCapture>,
    stats: Arc<SharedDeviceStats>,
    stop: Arc<AtomicBool>,
}

impl PhaseOneRenderer for DeviceIpcRenderer {
    fn render(&mut self, _output: InterleavedStereoF32<'_>) -> RenderDisposition {
        if self.stop.load(Ordering::Acquire) || self.stats.fatal_error.load(Ordering::Acquire) {
            // This harness intentionally exercises the scheduling/protocol path only. Returning
            // Silence tells the C boundary to retain its zeroed output buffer.
            return RenderDisposition::Silence;
        }

        let callback_started = self.clock.now_ticks();
        self.stats.callbacks.fetch_add(1, Ordering::Release);
        let counters = process_device_callback_block(
            &mut self.harness,
            self.block_index,
            self.request,
            self.clock,
            self.completion_budget_ticks,
            &mut self.timing,
        );
        self.stats.record_callback_outcome(&counters);
        self.block_index = self.block_index.saturating_add(1);
        let callback_elapsed = self.clock.now_ticks().saturating_sub(callback_started);
        self.callback_duration
            .observe(self.clock.ticks_to_duration(callback_elapsed));
        if callback_elapsed >= self.callback_period_ticks {
            self.stats.record_callback_overrun();
        }

        // No process polling, allocation, locking, file I/O, control IPC, formatting, or String
        // construction occurs on this real-time path.
        RenderDisposition::Silence
    }
}

impl Drop for DeviceIpcRenderer {
    fn drop(&mut self) {
        // `ActiveOutput` drops the renderer only after the C shim retired the callback. This
        // control-thread handoff intentionally performs serialization and locking off RT.
        let snapshot = serde_json::to_value(&self.timing)
            .ok()
            .and_then(|value| serde_json::from_value::<TimingSnapshot>(value).ok())
            .map(|phase| CapturedTiming {
                phase,
                callback_duration: self.callback_duration.snapshot(),
            });
        if let Some(snapshot) = snapshot {
            self.timing_capture.publish(snapshot);
        } else {
            self.timing_capture.error_code.store(
                CallbackErrorCode::TimingCaptureFailure as u32,
                Ordering::Release,
            );
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct HistogramSnapshot {
    bucket_width_micros: u64,
    buckets: Vec<u64>,
    sample_count: u64,
    overflow_count: u64,
    max_micros: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct TimingSnapshot {
    request_to_claim: HistogramSnapshot,
    claim_to_completion: HistogramSnapshot,
    request_to_completion: HistogramSnapshot,
    completion_to_observation: HistogramSnapshot,
    request_to_observation: HistogramSnapshot,
    synthetic_callback_work: HistogramSnapshot,
}

struct CapturedTiming {
    phase: TimingSnapshot,
    callback_duration: HistogramSnapshot,
}

/// Callback-owned fixed histogram allocated before AUHAL starts.
struct RealtimeHistogram {
    buckets: Box<[u64]>,
    sample_count: u64,
    overflow_count: u64,
    max_micros: u64,
}

impl RealtimeHistogram {
    fn new() -> Self {
        Self {
            buckets: vec![0; HISTOGRAM_MAX_MICROS + 1].into_boxed_slice(),
            sample_count: 0,
            overflow_count: 0,
            max_micros: 0,
        }
    }

    fn observe(&mut self, duration: Duration) {
        let micros = duration_to_micros_ceil(duration);
        self.sample_count = self.sample_count.saturating_add(1);
        self.max_micros = self.max_micros.max(micros);
        match usize::try_from(micros) {
            Ok(index) if index <= HISTOGRAM_MAX_MICROS => {
                self.buckets[index] = self.buckets[index].saturating_add(1);
            }
            _ => self.overflow_count = self.overflow_count.saturating_add(1),
        }
    }

    fn snapshot(&self) -> HistogramSnapshot {
        HistogramSnapshot {
            bucket_width_micros: 1,
            buckets: self.buckets.to_vec(),
            sample_count: self.sample_count,
            overflow_count: self.overflow_count,
            max_micros: self.max_micros,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum P9999Status {
    Available,
    StatisticallyUnderpowered,
    Unavailable,
}

impl P9999Status {
    const fn is_available(self) -> bool {
        matches!(self, Self::Available)
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::StatisticallyUnderpowered => "statistically_underpowered",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct HistogramEvidence {
    raw: HistogramSnapshot,
    p999_micros: u64,
    /// `None` until the empirical histogram contains enough observations to resolve p99.99
    /// without reducing the percentile to the observed maximum.
    p9999_micros: Option<u64>,
    p9999_status: P9999Status,
    integrity_valid: bool,
}

impl HistogramEvidence {
    fn from_snapshot(raw: HistogramSnapshot) -> Self {
        let lower_quantile = percentile_upper_bound(&raw, 999, 1_000);
        let p9999_status = if raw.sample_count >= P9999_MINIMUM_SAMPLE_COUNT {
            P9999Status::Available
        } else {
            P9999Status::StatisticallyUnderpowered
        };
        let accounted = raw.buckets.iter().copied().fold(0_u64, u64::saturating_add);
        let integrity_valid = raw.bucket_width_micros == 1
            && accounted.saturating_add(raw.overflow_count) == raw.sample_count;
        Self {
            p999_micros: lower_quantile,
            p9999_micros: p9999_status
                .is_available()
                .then(|| percentile_upper_bound(&raw, 9_999, 10_000)),
            p9999_status,
            integrity_valid,
            raw,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct TimingEvidence {
    /// Worker request publication through worker claim.
    request_to_claim: HistogramEvidence,
    /// Worker claim through completion publication.
    processing: HistogramEvidence,
    /// Request publication through worker completion publication.
    request_to_completion: HistogramEvidence,
    /// Worker completion publication through host acquire/consume observation.
    completion_publication_to_observation: HistogramEvidence,
    /// Request publication through completion metadata consumed by the host.
    completion_observation: HistogramEvidence,
    /// Observe/dispatch work measured inside `process_device_callback_block`.
    observe_dispatch_work: HistogramEvidence,
    /// Entire Rust renderer callback, including its outcome accounting.
    callback_duration: HistogramEvidence,
}

impl TimingEvidence {
    fn from_snapshot(snapshot: CapturedTiming) -> Self {
        Self {
            request_to_claim: HistogramEvidence::from_snapshot(snapshot.phase.request_to_claim),
            processing: HistogramEvidence::from_snapshot(snapshot.phase.claim_to_completion),
            request_to_completion: HistogramEvidence::from_snapshot(
                snapshot.phase.request_to_completion,
            ),
            completion_publication_to_observation: HistogramEvidence::from_snapshot(
                snapshot.phase.completion_to_observation,
            ),
            completion_observation: HistogramEvidence::from_snapshot(
                snapshot.phase.request_to_observation,
            ),
            observe_dispatch_work: HistogramEvidence::from_snapshot(
                snapshot.phase.synthetic_callback_work,
            ),
            callback_duration: HistogramEvidence::from_snapshot(snapshot.callback_duration),
        }
    }
}

fn percentile_upper_bound(histogram: &HistogramSnapshot, numerator: u64, denominator: u64) -> u64 {
    if histogram.sample_count == 0 || numerator == 0 || denominator == 0 {
        return 0;
    }
    let rank = histogram
        .sample_count
        .saturating_mul(numerator)
        .div_ceil(denominator)
        .max(1);
    let mut cumulative = 0_u64;
    for (index, count) in histogram.buckets.iter().enumerate() {
        cumulative = cumulative.saturating_add(*count);
        if cumulative >= rank {
            return u64::try_from(index)
                .unwrap_or(u64::MAX)
                .saturating_mul(histogram.bucket_width_micros);
        }
    }
    histogram.max_micros
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Serialize)]
struct TimingThresholds {
    dispatch_p9999_limit_micros: u64,
    dispatch_maximum_limit_micros_exclusive: u64,
    callback_p999_limit_period_fraction_exclusive: f64,
    callback_p9999_limit_period_fraction_exclusive: f64,
    callback_maximum_limit_period_exclusive: bool,
    p9999_minimum_sample_count: u64,
    /// Long matrix and calibrated-load artifacts enforce p99.99. Short preflights do not turn an
    /// unobservable p99.99 into a maximum-backed failure, but still enforce maximums and faults.
    p9999_enforced_for_acceptance: bool,
    request_to_claim_samples: u64,
    processing_samples: u64,
    completion_observation_samples: u64,
    callback_duration_samples: u64,
    dispatch_p9999_micros: Option<u64>,
    dispatch_p9999_status: P9999Status,
    dispatch_maximum_micros: u64,
    callback_p999_micros: u64,
    callback_p9999_micros: Option<u64>,
    callback_p9999_status: P9999Status,
    callback_maximum_micros: u64,
    histograms_valid: bool,
    available: bool,
    passed: bool,
}

impl TimingThresholds {
    fn unavailable() -> Self {
        Self {
            dispatch_p9999_limit_micros: DISPATCH_P9999_LIMIT_MICROS,
            dispatch_maximum_limit_micros_exclusive: DISPATCH_MAX_LIMIT_MICROS,
            callback_p999_limit_period_fraction_exclusive: 0.7,
            callback_p9999_limit_period_fraction_exclusive: 0.8,
            callback_maximum_limit_period_exclusive: true,
            p9999_minimum_sample_count: P9999_MINIMUM_SAMPLE_COUNT,
            p9999_enforced_for_acceptance: false,
            request_to_claim_samples: 0,
            processing_samples: 0,
            completion_observation_samples: 0,
            callback_duration_samples: 0,
            dispatch_p9999_micros: None,
            dispatch_p9999_status: P9999Status::Unavailable,
            dispatch_maximum_micros: 0,
            callback_p999_micros: 0,
            callback_p9999_micros: None,
            callback_p9999_status: P9999Status::Unavailable,
            callback_maximum_micros: 0,
            histograms_valid: false,
            available: false,
            passed: false,
        }
    }

    fn evaluate(
        timing: &TimingEvidence,
        period: Duration,
        p9999_enforced_for_acceptance: bool,
    ) -> Self {
        let request = &timing.request_to_claim.raw;
        let processing = &timing.processing.raw;
        let completion = &timing.completion_observation.raw;
        let callback = &timing.callback_duration.raw;
        let dispatch_p9999_micros = timing.completion_observation.p9999_micros;
        let dispatch_p9999_status = timing.completion_observation.p9999_status;
        let dispatch_maximum_micros = completion.max_micros;
        let callback_lower_percentile = timing.callback_duration.p999_micros;
        let callback_upper_percentile = timing.callback_duration.p9999_micros;
        let callback_p9999_status = timing.callback_duration.p9999_status;
        let callback_maximum_micros = callback.max_micros;
        let period_micros = duration_to_micros_ceil(period);
        let short_callback_budget = period.mul_f64(0.7);
        let long_callback_budget = period.mul_f64(0.8);
        let histograms_valid = timing.request_to_claim.integrity_valid
            && timing.processing.integrity_valid
            && timing.request_to_completion.integrity_valid
            && timing.completion_publication_to_observation.integrity_valid
            && timing.completion_observation.integrity_valid
            && timing.observe_dispatch_work.integrity_valid
            && timing.callback_duration.integrity_valid;
        let p9999_observable = [
            timing.request_to_claim.p9999_status,
            timing.processing.p9999_status,
            timing.request_to_completion.p9999_status,
            timing.completion_publication_to_observation.p9999_status,
            timing.completion_observation.p9999_status,
            timing.observe_dispatch_work.p9999_status,
            timing.callback_duration.p9999_status,
        ]
        .into_iter()
        .all(P9999Status::is_available);
        let p9999_within_limits = dispatch_p9999_micros
            .is_some_and(|micros| micros <= DISPATCH_P9999_LIMIT_MICROS)
            && callback_upper_percentile
                .is_some_and(|micros| Duration::from_micros(micros) < long_callback_budget);
        let passed = request.sample_count > 0
            && processing.sample_count > 0
            && completion.sample_count > 0
            && callback.sample_count > 0
            && histograms_valid
            && dispatch_maximum_micros < DISPATCH_MAX_LIMIT_MICROS
            && Duration::from_micros(callback_lower_percentile) < short_callback_budget
            && callback_maximum_micros < period_micros
            && (!p9999_enforced_for_acceptance || (p9999_observable && p9999_within_limits));
        Self {
            dispatch_p9999_limit_micros: DISPATCH_P9999_LIMIT_MICROS,
            dispatch_maximum_limit_micros_exclusive: DISPATCH_MAX_LIMIT_MICROS,
            callback_p999_limit_period_fraction_exclusive: 0.7,
            callback_p9999_limit_period_fraction_exclusive: 0.8,
            callback_maximum_limit_period_exclusive: true,
            p9999_minimum_sample_count: P9999_MINIMUM_SAMPLE_COUNT,
            p9999_enforced_for_acceptance,
            request_to_claim_samples: request.sample_count,
            processing_samples: processing.sample_count,
            completion_observation_samples: completion.sample_count,
            callback_duration_samples: callback.sample_count,
            dispatch_p9999_micros,
            dispatch_p9999_status,
            dispatch_maximum_micros,
            callback_p999_micros: callback_lower_percentile,
            callback_p9999_micros: callback_upper_percentile,
            callback_p9999_status,
            callback_maximum_micros,
            histograms_valid,
            available: true,
            passed,
        }
    }
}

fn acceptance_failures(
    attached: bool,
    device_format: DeviceFormatReport,
    frames: PhaseOneFrames,
    telemetry: CallbackTelemetry,
    callback_stats: &CallbackStats,
    timing_thresholds: &TimingThresholds,
    timing_capture_error: u32,
) -> Vec<String> {
    let mut failures = Vec::new();
    if !attached {
        failures.push("CoreAudio callback never fired; device attachment failed".to_owned());
    }
    if !device_format.matches_phase_one(frames) {
        failures.push(
            "final CoreAudio device format no longer matches the requested Phase 1 format"
                .to_owned(),
        );
    }
    if !telemetry.is_coherent() {
        failures.push("CoreAudio callback telemetry is incoherent".to_owned());
    }
    if telemetry.rendered != 0 {
        failures.push("the feasibility renderer unexpectedly reported rendered output".to_owned());
    }
    if telemetry.silenced < callback_stats.callbacks {
        failures.push(
            "CoreAudio telemetry observed fewer silent render callbacks than the renderer"
                .to_owned(),
        );
    }
    if callback_stats.accepted_completions == 0 {
        failures.push("no worker completion was accepted by the attached callback".to_owned());
    }
    if callback_stats.last_error_code != CallbackErrorCode::None as u32 {
        failures.push(format!(
            "callback last-error code {} ({})",
            callback_stats.last_error_code, callback_stats.last_error_label
        ));
    }
    if timing_capture_error != CallbackErrorCode::None as u32 {
        failures.push(format!(
            "timing capture error code {} ({})",
            timing_capture_error,
            CallbackErrorCode::label(timing_capture_error)
        ));
    }
    if !timing_thresholds.passed {
        failures.push("device timing histograms did not satisfy Phase 1 thresholds".to_owned());
    }
    failures
}

#[derive(Clone, Debug, Serialize)]
struct EnvironmentEvidence {
    operating_system: String,
    architecture: String,
    os_version: String,
    hardware_model: String,
    hardware_memory_bytes: String,
    rust_version: String,
    source_revision: String,
    source_state: String,
}

fn collect_environment(workspace_root: &Path) -> EnvironmentEvidence {
    EnvironmentEvidence {
        operating_system: std::env::consts::OS.to_owned(),
        architecture: std::env::consts::ARCH.to_owned(),
        os_version: command_value("sw_vers", &["-productVersion"], workspace_root),
        hardware_model: command_value("sysctl", &["-n", "hw.model"], workspace_root),
        hardware_memory_bytes: command_value("sysctl", &["-n", "hw.memsize"], workspace_root),
        rust_version: command_value("rustc", &["--version"], workspace_root),
        source_revision: command_value("git", &["rev-parse", "HEAD"], workspace_root),
        source_state: source_state(workspace_root),
    }
}

fn command_value(command: &str, arguments: &[&str], workspace_root: &Path) -> String {
    std::process::Command::new(command)
        .args(arguments)
        .current_dir(workspace_root)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unavailable".to_owned())
}

fn source_state(workspace_root: &Path) -> String {
    std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(workspace_root)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map_or_else(
            || "unavailable".to_owned(),
            |output| {
                if output.stdout.is_empty() {
                    "clean".to_owned()
                } else {
                    "dirty".to_owned()
                }
            },
        )
}

#[derive(Clone, Debug, Serialize)]
struct DeviceProvenance {
    default_output_device: bool,
    sample_rate_hz: f64,
    /// Total physical output channels exposed by the selected device.
    channel_count: u32,
    /// Fixed AUHAL client channel count, independently verified by callback telemetry.
    client_channel_count: u32,
    /// One-based physical destinations for the fixed stereo AUHAL client stream.
    client_output_channel_map: [u32; 2],
    current_frames_per_slice: u32,
    supported_minimum_frames_per_slice: u32,
    supported_maximum_frames_per_slice: u32,
    maximum_callback_frames_per_slice: u32,
    audio_unit_maximum_frames_per_slice: u32,
    uses_variable_buffer_frame_sizes: bool,
}

impl DeviceProvenance {
    fn from_format(format: DeviceFormatReport) -> Self {
        Self {
            default_output_device: true,
            sample_rate_hz: format.sample_rate_hz,
            channel_count: format.channel_count,
            client_channel_count: 2,
            client_output_channel_map: [1, 2],
            current_frames_per_slice: format.current_frames_per_slice,
            supported_minimum_frames_per_slice: format.supported_minimum_frames_per_slice,
            supported_maximum_frames_per_slice: format.supported_maximum_frames_per_slice,
            maximum_callback_frames_per_slice: format.maximum_callback_frames_per_slice,
            audio_unit_maximum_frames_per_slice: format.audio_unit_maximum_frames_per_slice,
            uses_variable_buffer_frame_sizes: format.uses_variable_buffer_frame_sizes,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct WorkerProvenance {
    executable: PathBuf,
    executable_sha256: Option<String>,
    requested_workers: usize,
    worker_ids: Vec<u32>,
    worker_generations: Vec<u64>,
    worker_process_ids: Vec<u32>,
    bank_identities: Vec<String>,
    startup_heartbeat_validation: &'static str,
}

impl WorkerProvenance {
    fn collect(executable: &Path, identities: Vec<DeviceWorkerIdentity>) -> Self {
        Self {
            executable: executable.to_path_buf(),
            executable_sha256: file_sha256(executable),
            requested_workers: identities.len(),
            worker_ids: identities
                .iter()
                .map(|identity| identity.worker_id)
                .collect(),
            worker_generations: identities
                .iter()
                .map(|identity| identity.generation)
                .collect(),
            worker_process_ids: identities
                .iter()
                .map(|identity| identity.process_id)
                .collect(),
            bank_identities: identities
                .into_iter()
                .map(|identity| identity.bank_identity)
                .collect(),
            startup_heartbeat_validation: "start_device_harness rejects a worker whose initial shared-memory heartbeat is zero",
        }
    }
}

fn file_sha256(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    Some(encoded)
}

#[derive(Clone, Debug, Serialize)]
struct DeviceCallbackTelemetry {
    callbacks: u64,
    rendered: u64,
    silenced: u64,
    invalid_frames: u64,
    invalid_buffers: u64,
    invalid_channels: u64,
    invalid_bytes: u64,
    frame_histogram_128: u64,
    frame_histogram_256: u64,
    coherent: bool,
}

impl From<CallbackTelemetry> for DeviceCallbackTelemetry {
    fn from(telemetry: CallbackTelemetry) -> Self {
        Self {
            callbacks: telemetry.callbacks,
            rendered: telemetry.rendered,
            silenced: telemetry.silenced,
            invalid_frames: telemetry.invalid_frames,
            invalid_buffers: telemetry.invalid_buffers,
            invalid_channels: telemetry.invalid_channels,
            invalid_bytes: telemetry.invalid_bytes,
            frame_histogram_128: telemetry.frame_histogram_128,
            frame_histogram_256: telemetry.frame_histogram_256,
            coherent: telemetry.is_coherent(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct CpuSnapshot {
    user_cpu_micros: u64,
    system_cpu_micros: u64,
    max_resident_bytes: u64,
}

impl From<ProcessResourceUsage> for CpuSnapshot {
    fn from(snapshot: ProcessResourceUsage) -> Self {
        Self {
            user_cpu_micros: snapshot.user_cpu_micros,
            system_cpu_micros: snapshot.system_cpu_micros,
            max_resident_bytes: snapshot.max_resident_bytes,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct CpuEvidence {
    status: &'static str,
    scope: &'static str,
    host_before: Option<CpuSnapshot>,
    host_after: Option<CpuSnapshot>,
    reaped_children_before: Option<CpuSnapshot>,
    reaped_children_after: Option<CpuSnapshot>,
}

impl CpuEvidence {
    fn new(
        host_before: Option<ProcessResourceUsage>,
        host_after: Option<ProcessResourceUsage>,
        children_before: Option<ProcessResourceUsage>,
        children_after: Option<ProcessResourceUsage>,
    ) -> Self {
        let collected = host_before.is_some()
            && host_after.is_some()
            && children_before.is_some()
            && children_after.is_some();
        Self {
            status: if collected {
                "collected"
            } else {
                "unavailable"
            },
            scope: "xtask_host_and_reaped_workers",
            host_before: host_before.map(Into::into),
            host_after: host_after.map(Into::into),
            reaped_children_before: children_before.map(Into::into),
            reaped_children_after: children_after.map(Into::into),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct ProcessEnergyReading {
    availability: &'static str,
    raw_nanojoules: Option<u64>,
    joules: Option<f64>,
    error: Option<String>,
}

impl ProcessEnergyReading {
    fn capture(process_id: u32) -> Self {
        match sample_process_energy(process_id) {
            Ok(ProcessEnergySample::Available(energy)) => Self {
                availability: "available",
                raw_nanojoules: Some(energy.raw_nanojoules),
                joules: Some(energy.joules),
                error: None,
            },
            Ok(ProcessEnergySample::Unavailable) => Self {
                availability: "unavailable",
                raw_nanojoules: None,
                joules: None,
                error: Some(
                    "macOS did not expose the requested process-energy rusage flavor".to_owned(),
                ),
            },
            Err(error) => Self {
                availability: "error",
                raw_nanojoules: None,
                joules: None,
                error: Some(error.to_string()),
            },
        }
    }

    const fn is_available(&self) -> bool {
        self.raw_nanojoules.is_some() && self.joules.is_some() && self.error.is_none()
    }
}

#[derive(Clone, Debug)]
struct ProcessEnergyBaseline {
    host: (u32, ProcessEnergyReading),
    workers: Vec<(u32, ProcessEnergyReading)>,
}

impl ProcessEnergyBaseline {
    fn capture(host_pid: u32, worker_pids: &[u32]) -> Self {
        Self {
            host: (host_pid, ProcessEnergyReading::capture(host_pid)),
            workers: worker_pids
                .iter()
                .copied()
                .map(|pid| (pid, ProcessEnergyReading::capture(pid)))
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct ProcessEnergyDelta {
    role: &'static str,
    process_id: u32,
    before: ProcessEnergyReading,
    after: ProcessEnergyReading,
    delta_raw_nanojoules: Option<u64>,
    delta_joules: Option<f64>,
    /// The planned self-crash worker is not live at the end of a containment run.
    expected_to_exit: bool,
    complete: bool,
}

impl ProcessEnergyDelta {
    fn new(
        role: &'static str,
        process_id: u32,
        before: ProcessEnergyReading,
        after: ProcessEnergyReading,
        expected_to_exit: bool,
    ) -> Self {
        let delta_raw_nanojoules = before
            .raw_nanojoules
            .zip(after.raw_nanojoules)
            .and_then(|(before, after)| after.checked_sub(before));
        let delta_joules = before.joules.zip(after.joules).and_then(|(before, after)| {
            let delta = after - before;
            (delta.is_finite() && delta >= 0.0).then_some(delta)
        });
        let complete = before.is_available()
            && after.is_available()
            && delta_raw_nanojoules.is_some()
            && delta_joules.is_some();
        Self {
            role,
            process_id,
            before,
            after,
            delta_raw_nanojoules,
            delta_joules,
            expected_to_exit,
            complete,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct InternalEnergyEvidence {
    status: &'static str,
    collector: &'static str,
    api: &'static str,
    rusage_flavor: &'static str,
    measurement_duration_micros: u64,
    measurement_duration_seconds: f64,
    host: ProcessEnergyDelta,
    workers: Vec<ProcessEnergyDelta>,
    expected_departed_worker_process_ids: Vec<u32>,
    total_delta_raw_nanojoules: Option<u64>,
    total_delta_joules: Option<f64>,
    certification_complete: bool,
    validation_errors: Vec<String>,
}

impl InternalEnergyEvidence {
    fn from_baselines(
        before: ProcessEnergyBaseline,
        after: ProcessEnergyBaseline,
        observed_duration: Duration,
        expected_departed_worker_process_id: Option<u32>,
    ) -> Self {
        let host =
            ProcessEnergyDelta::new("host", before.host.0, before.host.1, after.host.1, false);
        let workers = before
            .workers
            .into_iter()
            .zip(after.workers)
            .map(|((before_pid, before), (after_pid, after))| {
                let process_id = if before_pid == after_pid {
                    before_pid
                } else {
                    after_pid
                };
                ProcessEnergyDelta::new(
                    "worker",
                    process_id,
                    before,
                    after,
                    expected_departed_worker_process_id == Some(process_id),
                )
            })
            .collect::<Vec<_>>();
        let total_delta_raw_nanojoules = host
            .delta_raw_nanojoules
            .into_iter()
            .chain(
                workers
                    .iter()
                    .filter_map(|worker| worker.delta_raw_nanojoules),
            )
            .try_fold(0_u64, u64::checked_add);
        let total_delta_joules = host
            .delta_joules
            .into_iter()
            .chain(workers.iter().filter_map(|worker| worker.delta_joules))
            .try_fold(0.0_f64, |total, value| {
                let next = total + value;
                (next.is_finite()).then_some(next)
            });
        let mut validation_errors = Vec::new();
        if observed_duration.is_zero() {
            validation_errors.push("observed measurement duration must be positive".to_owned());
        }
        if !host.complete {
            validation_errors.push("host process energy sampling was incomplete".to_owned());
        }
        if workers.is_empty() {
            validation_errors.push("no worker process energy samples were retained".to_owned());
        }
        for worker in &workers {
            if !worker.complete && !worker.expected_to_exit {
                validation_errors.push(format!(
                    "worker PID {} energy sampling was incomplete",
                    worker.process_id
                ));
            }
            if worker.expected_to_exit && !worker.before.is_available() {
                validation_errors.push(format!(
                    "planned self-crash worker PID {} lacks its pre-exit energy sample",
                    worker.process_id
                ));
            }
        }
        if total_delta_raw_nanojoules.is_none() || total_delta_joules.is_none() {
            validation_errors.push("could not calculate total process-energy delta".to_owned());
        }
        if total_delta_raw_nanojoules == Some(0) {
            validation_errors.push("total process-energy delta must be positive".to_owned());
        }
        let certification_complete = validation_errors.is_empty();
        Self {
            status: if certification_complete {
                "collected"
            } else {
                "incomplete"
            },
            collector: "superposition-xtask",
            api: "proc_pid_rusage",
            // `ri_energy_nj` is present in the current Darwin V6 layout on the reference Xcode
            // SDK. V4 has billed/serviced energy but no nanjoule field, so requesting V6 avoids
            // reading an uninitialized extension while preserving the requested proc_pid_rusage
            // measurement mechanism.
            rusage_flavor: "RUSAGE_INFO_V6 (ri_energy_nj)",
            measurement_duration_micros: duration_to_micros_ceil(observed_duration),
            measurement_duration_seconds: observed_duration.as_secs_f64(),
            host,
            expected_departed_worker_process_ids: expected_departed_worker_process_id
                .into_iter()
                .collect(),
            workers,
            total_delta_raw_nanojoules,
            total_delta_joules,
            certification_complete,
            validation_errors,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct ExternalEnergyEvidence {
    status: &'static str,
    imported: bool,
    path: Option<PathBuf>,
    schema_version: Option<u64>,
    collector: Option<String>,
    source: Option<String>,
    measurement_duration_seconds: Option<f64>,
    hardware_identity: Option<String>,
    workload: Option<String>,
    rack_count: Option<u64>,
    frame_count: Option<u64>,
    energy_joules: Option<f64>,
    average_power_watts: Option<f64>,
    validation_errors: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
struct EnergyEvidence {
    status: &'static str,
    required: bool,
    /// Accepted for command-line compatibility; internal collection is required regardless.
    compatibility_require_energy_evidence_flag: bool,
    certification_complete: bool,
    internal: InternalEnergyEvidence,
    external_cross_check: ExternalEnergyEvidence,
}

impl EnergyEvidence {
    fn from_process_samples(
        before: ProcessEnergyBaseline,
        after: ProcessEnergyBaseline,
        observed_duration: Duration,
        expected_departed_worker_process_id: Option<u32>,
        compatibility_require_energy_evidence_flag: bool,
        external_cross_check: ExternalEnergyEvidence,
    ) -> Self {
        let internal = InternalEnergyEvidence::from_baselines(
            before,
            after,
            observed_duration,
            expected_departed_worker_process_id,
        );
        let certification_complete = internal.certification_complete;
        Self {
            status: if certification_complete {
                "collected"
            } else {
                "incomplete"
            },
            required: true,
            compatibility_require_energy_evidence_flag,
            certification_complete,
            internal,
            external_cross_check,
        }
    }
}

fn load_external_energy_evidence(
    path: Option<&Path>,
    expected_racks: usize,
    expected_frames: u32,
    expected_hardware: &str,
    expected_workload: &str,
) -> Result<ExternalEnergyEvidence, Phase1Error> {
    let Some(path) = path else {
        return Ok(ExternalEnergyEvidence {
            status: "not_supplied",
            imported: false,
            path: None,
            schema_version: None,
            collector: None,
            source: None,
            measurement_duration_seconds: None,
            hardware_identity: None,
            workload: None,
            rack_count: None,
            frame_count: None,
            energy_joules: None,
            average_power_watts: None,
            validation_errors: Vec::new(),
        });
    };
    let contents = fs::read(path).map_err(|error| {
        Phase1Error::InvalidConfiguration(format!(
            "could not read external structured energy evidence {}: {error}",
            path.display()
        ))
    })?;
    let value: Value = serde_json::from_slice(&contents).map_err(|error| {
        Phase1Error::InvalidConfiguration(format!(
            "external structured energy evidence {} is not JSON: {error}",
            path.display()
        ))
    })?;
    let object = value.as_object().ok_or_else(|| {
        Phase1Error::InvalidConfiguration(
            "external structured energy evidence must be a JSON object".to_owned(),
        )
    })?;
    let schema_version = object.get("schema_version").and_then(Value::as_u64);
    let collector = nonempty_json_string(object.get("collector"));
    let source = nonempty_json_string(object.get("source"));
    let measurement_duration_seconds =
        positive_json_number(object.get("measurement_duration_seconds"));
    let hardware_identity = nonempty_json_string(object.get("hardware_identity"));
    let workload = nonempty_json_string(object.get("workload"));
    let rack_count = object.get("rack_count").and_then(Value::as_u64);
    let frame_count = object.get("frame_count").and_then(Value::as_u64);
    let energy_joules = positive_json_number(object.get("energy_joules"));
    let average_power_watts = positive_json_number(object.get("average_power_watts"));
    let mut validation_errors = Vec::new();
    if schema_version != Some(1) {
        validation_errors.push("schema_version must equal 1".to_owned());
    }
    if collector.is_none() {
        validation_errors.push("collector must be a nonempty string".to_owned());
    }
    if source.is_none() {
        validation_errors.push("source must be a nonempty string".to_owned());
    }
    if measurement_duration_seconds.is_none() {
        validation_errors.push("measurement_duration_seconds must be positive".to_owned());
    }
    if hardware_identity.as_deref() != Some(expected_hardware) {
        validation_errors.push(format!(
            "hardware_identity must match current hardware `{expected_hardware}`"
        ));
    }
    if workload.as_deref() != Some(expected_workload) {
        validation_errors.push(format!("workload must equal `{expected_workload}`"));
    }
    if rack_count != u64::try_from(expected_racks).ok() {
        validation_errors.push(format!("rack_count must equal {expected_racks}"));
    }
    if frame_count != Some(u64::from(expected_frames)) {
        validation_errors.push(format!("frame_count must equal {expected_frames}"));
    }
    if energy_joules.is_none() && average_power_watts.is_none() {
        validation_errors.push(
            "at least one of energy_joules or average_power_watts must be positive".to_owned(),
        );
    }
    let imported = validation_errors.is_empty();
    Ok(ExternalEnergyEvidence {
        status: if imported { "imported" } else { "invalid" },
        imported,
        path: Some(path.to_path_buf()),
        schema_version,
        collector,
        source,
        measurement_duration_seconds,
        hardware_identity,
        workload,
        rack_count,
        frame_count,
        energy_joules,
        average_power_watts,
        validation_errors,
    })
}

fn nonempty_json_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn positive_json_number(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(Value::as_f64)
        .filter(|number| number.is_finite() && *number > 0.0)
}

#[derive(Clone, Debug, Serialize)]
struct WorkerBusyDurationEvidence {
    worker_id: u32,
    operations: u64,
    requested_busy_micros: u64,
    observed_busy_micros: u64,
}

#[derive(Clone, Debug, Serialize)]
struct CalibratedLoadEvidence {
    mode: &'static str,
    deterministic_target_per_request_micros: Option<u64>,
    workers: Vec<WorkerBusyDurationEvidence>,
    requested_busy_micros_total: u64,
    observed_busy_micros_total: u64,
    operations_total: u64,
    complete: bool,
}

impl CalibratedLoadEvidence {
    fn from_snapshots(
        mode: ComputeLoadMode,
        frame_count: u32,
        clock: MonotonicClock,
        snapshots: &[WorkerHeartbeatSnapshot],
    ) -> Self {
        let workers = snapshots
            .iter()
            .map(|worker| WorkerBusyDurationEvidence {
                worker_id: worker.worker_id,
                operations: worker.busy_operations,
                requested_busy_micros: duration_to_micros_ceil(
                    clock.ticks_to_duration(worker.busy_requested_ticks),
                ),
                observed_busy_micros: duration_to_micros_ceil(
                    clock.ticks_to_duration(worker.busy_observed_ticks),
                ),
            })
            .collect::<Vec<_>>();
        let requested_busy_micros_total = workers
            .iter()
            .map(|worker| worker.requested_busy_micros)
            .fold(0_u64, u64::saturating_add);
        let observed_busy_micros_total = workers
            .iter()
            .map(|worker| worker.observed_busy_micros)
            .fold(0_u64, u64::saturating_add);
        let operations_total = workers
            .iter()
            .map(|worker| worker.operations)
            .fold(0_u64, u64::saturating_add);
        let deterministic_target_per_request_micros = calibrated_target_micros(frame_count);
        let complete = match mode {
            ComputeLoadMode::None => {
                operations_total == 0
                    && requested_busy_micros_total == 0
                    && observed_busy_micros_total == 0
            }
            ComputeLoadMode::CalibratedCpu => {
                deterministic_target_per_request_micros.is_some()
                    && workers.iter().all(|worker| {
                        worker.operations > 0
                            && worker.requested_busy_micros > 0
                            && worker.observed_busy_micros > 0
                    })
            }
        };
        Self {
            mode: mode.as_str(),
            deterministic_target_per_request_micros,
            workers,
            requested_busy_micros_total,
            observed_busy_micros_total,
            operations_total,
            complete,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct HeartbeatEvidence {
    startup_heartbeat_verified: bool,
    periodic_heartbeat_observation: &'static str,
    worker_exit_liveness_source: &'static str,
    mapped_workers: Vec<WorkerHeartbeatSnapshot>,
    all_workers_progressed: bool,
}

impl HeartbeatEvidence {
    fn from_snapshots(mapped_workers: Vec<WorkerHeartbeatSnapshot>) -> Self {
        let all_workers_progressed = !mapped_workers.is_empty()
            && mapped_workers
                .iter()
                .copied()
                .all(WorkerHeartbeatSnapshot::healthy);
        Self {
            startup_heartbeat_verified: mapped_workers
                .iter()
                .all(|worker| worker.initial_tick != 0),
            periodic_heartbeat_observation: "control thread polls a separate read-only mapping for every worker bank",
            worker_exit_liveness_source: "control-thread worker monitor publishes atomic exit flags",
            mapped_workers,
            all_workers_progressed,
        }
    }
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Serialize)]
struct DeviceReportLabels {
    scope: &'static str,
    synthetic_preflight: bool,
    coreaudio_callback_attached: bool,
    active_device_preflight: bool,
    phase1_hard_gate_certified: bool,
    status: &'static str,
    qualifying: bool,
    official_duration_seconds: u64,
    required_device_matrix_cells: Vec<String>,
    detail: &'static str,
}

impl DeviceReportLabels {
    fn for_duration(duration: Duration, callback_attached: bool, acceptance_passed: bool) -> Self {
        let duration_complete = duration >= Duration::from_secs(CERTIFICATION_DURATION_SECONDS);
        Self {
            scope: "active_device_callback",
            synthetic_preflight: false,
            coreaudio_callback_attached: callback_attached,
            active_device_preflight: callback_attached,
            phase1_hard_gate_certified: false,
            status: if acceptance_passed && duration_complete {
                "active_device_single_cell_duration_complete_matrix_incomplete"
            } else if acceptance_passed {
                "active_device_preflight_passed"
            } else {
                "active_device_preflight_failed"
            },
            qualifying: false,
            official_duration_seconds: CERTIFICATION_DURATION_SECONDS,
            required_device_matrix_cells: required_device_matrix_cells(),
            detail: "This command attaches to CoreAudio but one no-op worker cell cannot certify Phase 1. Certification requires all device matrix cells plus calibrated-load and live fault-isolation evidence.",
        }
    }
}

fn required_device_matrix_cells() -> Vec<String> {
    [1, 2, 4, 8]
        .into_iter()
        .flat_map(|racks| [128, 256].map(move |frames| format!("{racks}r-{frames}f")))
        .collect()
}

#[derive(Clone, Debug, Serialize)]
struct DeviceConfiguration {
    rack_count: usize,
    frame_count: u32,
    sample_rate_hz: u32,
    requested_duration_seconds: u64,
    observed_duration_micros: u64,
    block_period_micros: u64,
    output_disposition: &'static str,
    workload: &'static str,
    compute_load_mode: &'static str,
    compute_load_micros: u64,
    fault_mode: &'static str,
    fault_target_rack: Option<usize>,
    fault_trigger_sequence: u64,
}

#[derive(Clone, Debug, Serialize)]
struct DeviceFeasibilityReport {
    report_version: u32,
    report_id: String,
    artifact_kind: &'static str,
    generated_unix_seconds: u64,
    labels: DeviceReportLabels,
    environment: EnvironmentEvidence,
    device: DeviceProvenance,
    configuration: DeviceConfiguration,
    workers: WorkerProvenance,
    callback_telemetry: DeviceCallbackTelemetry,
    callback_stats: CallbackStats,
    fault_isolation: Option<FaultIsolationEvidence>,
    timing: Option<TimingEvidence>,
    timing_thresholds: TimingThresholds,
    heartbeat: HeartbeatEvidence,
    calibrated_load: CalibratedLoadEvidence,
    cpu_evidence: CpuEvidence,
    energy_evidence: EnergyEvidence,
    acceptance_passed: bool,
    evidence_complete: bool,
    acceptance_failures: Vec<String>,
    infrastructure_error: Option<String>,
    limitations: Vec<String>,
}

#[derive(Serialize)]
struct ArtifactManifest<'a> {
    report_version: u32,
    report_id: &'a str,
    json: &'a str,
    markdown: &'a str,
    json_sha256: String,
    markdown_sha256: String,
}

fn artifact_manifest<'a>(
    report_version: u32,
    report_id: &'a str,
    json_name: &'a str,
    markdown_name: &'a str,
    json: &[u8],
    markdown: &[u8],
) -> ArtifactManifest<'a> {
    ArtifactManifest {
        report_version,
        report_id,
        json: json_name,
        markdown: markdown_name,
        json_sha256: content_sha256(json),
        markdown_sha256: content_sha256(markdown),
    }
}

fn content_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

fn assign_report_id(report: &mut DeviceFeasibilityReport) -> Result<(), Phase1Error> {
    report.report_id.clear();
    let model = serde_json::to_vec(report).map_err(|error| {
        Phase1Error::Infrastructure(format!("could not fingerprint device report: {error}"))
    })?;
    report.report_id = format!("{:016x}", fnv1a64(&model));
    Ok(())
}

fn write_report(
    output_directory: &Path,
    report: &DeviceFeasibilityReport,
) -> Result<(), Phase1Error> {
    let json = serde_json::to_vec_pretty(report).map_err(|error| {
        Phase1Error::Infrastructure(format!(
            "could not serialize device-feasibility.json: {error}"
        ))
    })?;
    let markdown = report_markdown(report).into_bytes();
    let manifest = serde_json::to_vec_pretty(&artifact_manifest(
        report.report_version,
        &report.report_id,
        "device-feasibility.json",
        "device-feasibility.md",
        &json,
        &markdown,
    ))
    .map_err(|error| {
        Phase1Error::Infrastructure(format!(
            "could not serialize device report manifest: {error}"
        ))
    })?;
    publish_artifact_set(
        output_directory,
        "device-feasibility",
        &report.report_id,
        &json,
        &markdown,
        &manifest,
    )
}

fn publish_artifact_set(
    output_directory: &Path,
    stem: &str,
    report_id: &str,
    json: &[u8],
    markdown: &[u8],
    manifest: &[u8],
) -> Result<(), Phase1Error> {
    fs::create_dir_all(output_directory).map_err(|error| {
        Phase1Error::Infrastructure(format!(
            "could not create report directory {}: {error}",
            output_directory.display()
        ))
    })?;
    let nonce = format!("{}.{}.{}", std::process::id(), report_id, unix_seconds());
    let json_temp = output_directory.join(format!(".{stem}.{nonce}.json.tmp"));
    let markdown_temp = output_directory.join(format!(".{stem}.{nonce}.md.tmp"));
    let manifest_temp = output_directory.join(format!(".{stem}.{nonce}.manifest.tmp"));
    let final_manifest = output_directory.join(format!("{stem}.manifest.json"));
    match fs::remove_file(&final_manifest) {
        Ok(()) => fs::File::open(output_directory)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| {
                Phase1Error::Infrastructure(format!(
                    "could not invalidate stale {stem} manifest: {error}"
                ))
            })?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(Phase1Error::Infrastructure(format!(
                "could not invalidate stale {stem} manifest: {error}"
            )));
        }
    }
    let result = (|| {
        write_synced_file(&json_temp, json)?;
        write_synced_file(&markdown_temp, markdown)?;
        write_synced_file(&manifest_temp, manifest)?;
        fs::rename(&json_temp, output_directory.join(format!("{stem}.json")))?;
        fs::rename(&markdown_temp, output_directory.join(format!("{stem}.md")))?;
        fs::rename(&manifest_temp, &final_manifest)?;
        fs::File::open(output_directory)?.sync_all()?;
        Ok::<(), std::io::Error>(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(json_temp);
        let _ = fs::remove_file(markdown_temp);
        let _ = fs::remove_file(manifest_temp);
        return Err(Phase1Error::Infrastructure(format!(
            "could not atomically publish {stem} artifacts: {error}"
        )));
    }
    Ok(())
}

fn write_synced_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(contents)?;
    file.flush()?;
    file.sync_all()
}

#[allow(clippy::too_many_lines)]
fn report_markdown(report: &DeviceFeasibilityReport) -> String {
    let mut markdown = String::new();
    writeln!(markdown, "# Phase 1 active-device feasibility preflight")
        .expect("writing to String cannot fail");
    writeln!(markdown, "report_id: {}", report.report_id).expect("writing to String cannot fail");
    writeln!(markdown, "report_version: {}", report.report_version)
        .expect("writing to String cannot fail");
    writeln!(markdown, "artifact_kind: {}", report.artifact_kind)
        .expect("writing to String cannot fail");
    writeln!(
        markdown,
        "scope: {}; synthetic_preflight={}; coreaudio_callback_attached={}; active_device_preflight={}; phase1_hard_gate_certified={}",
        report.labels.scope,
        report.labels.synthetic_preflight,
        report.labels.coreaudio_callback_attached,
        report.labels.active_device_preflight,
        report.labels.phase1_hard_gate_certified,
    )
    .expect("writing to String cannot fail");
    writeln!(markdown, "## Qualification").expect("writing to String cannot fail");
    writeln!(
        markdown,
        "- status: {}; qualifying: {}; required duration: {} seconds; {}",
        report.labels.status,
        report.labels.qualifying,
        report.labels.official_duration_seconds,
        report.labels.detail,
    )
    .expect("writing to String cannot fail");
    writeln!(
        markdown,
        "- required device matrix: {}",
        report.labels.required_device_matrix_cells.join(", ")
    )
    .expect("writing to String cannot fail");
    writeln!(markdown, "## Environment and device").expect("writing to String cannot fail");
    writeln!(
        markdown,
        "- OS: {} {}; arch: {}; hardware: {}; memory bytes: {}; Rust: {}; revision: {}; source: {}",
        report.environment.operating_system,
        report.environment.os_version,
        report.environment.architecture,
        report.environment.hardware_model,
        report.environment.hardware_memory_bytes,
        report.environment.rust_version,
        report.environment.source_revision,
        report.environment.source_state,
    )
    .expect("writing to String cannot fail");
    writeln!(
        markdown,
        "- default output: {}; sample rate: {} Hz; physical output channels: {}; fixed AUHAL client output: channels 1–2; device/AU max frames: {}/{}; variable buffers: {}",
        report.device.default_output_device,
        report.device.sample_rate_hz,
        report.device.channel_count,
        report.device.maximum_callback_frames_per_slice,
        report.device.audio_unit_maximum_frames_per_slice,
        report.device.uses_variable_buffer_frame_sizes,
    )
    .expect("writing to String cannot fail");
    writeln!(markdown, "## Callback and worker provenance").expect("writing to String cannot fail");
    writeln!(
        markdown,
        "- racks: {}; frames: {}; requested duration: {} s; observed duration: {} us; period: {} us; output: {}; workload: {}",
        report.configuration.rack_count,
        report.configuration.frame_count,
        report.configuration.requested_duration_seconds,
        report.configuration.observed_duration_micros,
        report.configuration.block_period_micros,
        report.configuration.output_disposition,
        report.configuration.workload,
    )
    .expect("writing to String cannot fail");
    writeln!(
        markdown,
        "- worker executable: {}; sha256: {}; IDs: {:?}; PIDs: {:?}; generations: {:?}; banks: {:?}",
        report.workers.executable.display(),
        report
            .workers
            .executable_sha256
            .as_deref()
            .unwrap_or("unavailable"),
        report.workers.worker_ids,
        report.workers.worker_process_ids,
        report.workers.worker_generations,
        report.workers.bank_identities,
    )
    .expect("writing to String cannot fail");
    writeln!(
        markdown,
        "- CoreAudio callbacks/rendered/silenced: {}/{}/{}; valid 128/256 frames: {}/{}; coherent: {}",
        report.callback_telemetry.callbacks,
        report.callback_telemetry.rendered,
        report.callback_telemetry.silenced,
        report.callback_telemetry.frame_histogram_128,
        report.callback_telemetry.frame_histogram_256,
        report.callback_telemetry.coherent,
    )
    .expect("writing to String cannot fail");
    writeln!(
        markdown,
        "- callback accepted/deadline/fallback/overrun/protocol/worker-exit: {}/{}/{}/{}/{}/{}; fatal events: {}; first/last code: {} ({}) / {} ({})",
        report.callback_stats.accepted_completions,
        report.callback_stats.deadline_misses,
        report.callback_stats.fallback_events,
        report.callback_stats.callback_overruns,
        report.callback_stats.protocol_faults,
        report.callback_stats.worker_exits,
        report.callback_stats.fatal_error_events,
        report.callback_stats.first_error_code,
        report.callback_stats.first_error_label,
        report.callback_stats.last_error_code,
        report.callback_stats.last_error_label,
    )
    .expect("writing to String cannot fail");
    if let Some(fault) = &report.fault_isolation {
        writeln!(
            markdown,
            "- fault isolation: target rack {}; mode {}; trigger sequence {}; active callback={}; target observed={}; control-plane exit observed={}; unaffected continued={}; unaffected fault-free={}; first fallback block={:?}; current-or-next-block={}; fallback/deadline/protocol/exit={}/{}/{}/{}; passed={}",
            fault.target_rack,
            fault.fault_mode,
            fault.fault_trigger_sequence,
            fault.active_device_callback,
            fault.target_fault_observed,
            fault.control_plane_worker_exit_observed,
            fault.unaffected_racks_continued,
            fault.unaffected_racks_fault_free,
            fault.first_target_fallback_block,
            fault.fallback_by_current_or_next_block,
            fault.fallback_events,
            fault.deadline_misses,
            fault.protocol_faults,
            fault.worker_exits,
            fault.passed,
        )
        .expect("writing to String cannot fail");
    }
    writeln!(markdown, "## Timing thresholds").expect("writing to String cannot fail");
    writeln!(
        markdown,
        "- available: {}; histograms valid: {}; passed: {}; p99.99 enforced: {}; minimum p99.99 samples: {}; request/processing/completion/callback samples: {}/{}/{}/{}",
        report.timing_thresholds.available,
        report.timing_thresholds.histograms_valid,
        report.timing_thresholds.passed,
        report.timing_thresholds.p9999_enforced_for_acceptance,
        report.timing_thresholds.p9999_minimum_sample_count,
        report.timing_thresholds.request_to_claim_samples,
        report.timing_thresholds.processing_samples,
        report.timing_thresholds.completion_observation_samples,
        report.timing_thresholds.callback_duration_samples,
    )
    .expect("writing to String cannot fail");
    writeln!(
        markdown,
        "- completion p99.99={} (<= {} when enforced); max={} us (< {}); callback p99.9={} us (<70% period), p99.99={} (<80% period when enforced), max={} us (<period)",
        format_p9999_measurement(
            report.timing_thresholds.dispatch_p9999_micros,
            report.timing_thresholds.dispatch_p9999_status,
        ),
        report.timing_thresholds.dispatch_p9999_limit_micros,
        report.timing_thresholds.dispatch_maximum_micros,
        report.timing_thresholds.dispatch_maximum_limit_micros_exclusive,
        report.timing_thresholds.callback_p999_micros,
        format_p9999_measurement(
            report.timing_thresholds.callback_p9999_micros,
            report.timing_thresholds.callback_p9999_status,
        ),
        report.timing_thresholds.callback_maximum_micros,
    )
    .expect("writing to String cannot fail");
    if let Some(timing) = &report.timing {
        writeln!(
            markdown,
            "- request→claim p99.9/p99.99/max: {}/{}/{} us; processing: {}/{}/{} us; request→completion publication: {}/{}/{} us; publication→host observation: {}/{}/{} us; request→host observation: {}/{}/{} us; observe/dispatch: {}/{}/{} us; full callback: {}/{}/{} us",
            timing.request_to_claim.p999_micros,
            format_p9999_measurement(
                timing.request_to_claim.p9999_micros,
                timing.request_to_claim.p9999_status,
            ),
            timing.request_to_claim.raw.max_micros,
            timing.processing.p999_micros,
            format_p9999_measurement(timing.processing.p9999_micros, timing.processing.p9999_status),
            timing.processing.raw.max_micros,
            timing.request_to_completion.p999_micros,
            format_p9999_measurement(
                timing.request_to_completion.p9999_micros,
                timing.request_to_completion.p9999_status,
            ),
            timing.request_to_completion.raw.max_micros,
            timing.completion_publication_to_observation.p999_micros,
            format_p9999_measurement(
                timing.completion_publication_to_observation.p9999_micros,
                timing.completion_publication_to_observation.p9999_status,
            ),
            timing.completion_publication_to_observation.raw.max_micros,
            timing.completion_observation.p999_micros,
            format_p9999_measurement(
                timing.completion_observation.p9999_micros,
                timing.completion_observation.p9999_status,
            ),
            timing.completion_observation.raw.max_micros,
            timing.observe_dispatch_work.p999_micros,
            format_p9999_measurement(
                timing.observe_dispatch_work.p9999_micros,
                timing.observe_dispatch_work.p9999_status,
            ),
            timing.observe_dispatch_work.raw.max_micros,
            timing.callback_duration.p999_micros,
            format_p9999_measurement(
                timing.callback_duration.p9999_micros,
                timing.callback_duration.p9999_status,
            ),
            timing.callback_duration.raw.max_micros,
        )
        .expect("writing to String cannot fail");
    }
    writeln!(markdown, "## Heartbeat, CPU, and energy").expect("writing to String cannot fail");
    writeln!(
        markdown,
        "- startup heartbeat verified: {}; all mapped workers progressed: {}; periodic heartbeat: {}; exit liveness: {}",
        report.heartbeat.startup_heartbeat_verified,
        report.heartbeat.all_workers_progressed,
        report.heartbeat.periodic_heartbeat_observation,
        report.heartbeat.worker_exit_liveness_source,
    )
    .expect("writing to String cannot fail");
    for worker in &report.heartbeat.mapped_workers {
        writeln!(
            markdown,
            "  - worker {} heartbeat initial/last={} / {}; polls/advances/regressions={}/{}/{}; calibrated requested/observed/operations ticks={}/{}/{}",
            worker.worker_id,
            worker.initial_tick,
            worker.last_tick,
            worker.control_polls,
            worker.advances,
            worker.regressions,
            worker.busy_requested_ticks,
            worker.busy_observed_ticks,
            worker.busy_operations,
        )
        .expect("writing to String cannot fail");
    }
    writeln!(
        markdown,
        "- calibrated load mode={}; deterministic target/request={} us; requested/observed total={} / {} us; operations={}; complete={}",
        report.calibrated_load.mode,
        report
            .calibrated_load
            .deterministic_target_per_request_micros
            .map_or_else(|| "none".to_owned(), |target| target.to_string()),
        report.calibrated_load.requested_busy_micros_total,
        report.calibrated_load.observed_busy_micros_total,
        report.calibrated_load.operations_total,
        report.calibrated_load.complete,
    )
    .expect("writing to String cannot fail");
    for worker in &report.calibrated_load.workers {
        writeln!(
            markdown,
            "  - calibrated worker {} requested/observed={} / {} us across {} operations",
            worker.worker_id,
            worker.requested_busy_micros,
            worker.observed_busy_micros,
            worker.operations,
        )
        .expect("writing to String cannot fail");
    }
    writeln!(
        markdown,
        "- CPU: {} ({}); internal process energy: {}; required: {}; complete: {}; duration={} us; total={} nJ ({:?} J); optional external cross-check: {}",
        report.cpu_evidence.status,
        report.cpu_evidence.scope,
        report.energy_evidence.status,
        report.energy_evidence.required,
        report.energy_evidence.certification_complete,
        report.energy_evidence.internal.measurement_duration_micros,
        report
            .energy_evidence
            .internal
            .total_delta_raw_nanojoules
            .map_or_else(|| "unavailable".to_owned(), |value| value.to_string()),
        report.energy_evidence.internal.total_delta_joules,
        report.energy_evidence.external_cross_check.status,
    )
    .expect("writing to String cannot fail");
    for error in &report.energy_evidence.internal.validation_errors {
        writeln!(markdown, "  - internal energy validation: {error}")
            .expect("writing to String cannot fail");
    }
    for error in &report
        .energy_evidence
        .external_cross_check
        .validation_errors
    {
        writeln!(
            markdown,
            "  - external energy cross-check validation: {error}"
        )
        .expect("writing to String cannot fail");
    }
    writeln!(markdown, "## Result").expect("writing to String cannot fail");
    writeln!(
        markdown,
        "- acceptance passed: {}",
        report.acceptance_passed
    )
    .expect("writing to String cannot fail");
    writeln!(
        markdown,
        "- evidence complete: {}",
        report.evidence_complete
    )
    .expect("writing to String cannot fail");
    for failure in &report.acceptance_failures {
        writeln!(markdown, "- failure: {failure}").expect("writing to String cannot fail");
    }
    if let Some(error) = &report.infrastructure_error {
        writeln!(markdown, "- infrastructure error: {error}")
            .expect("writing to String cannot fail");
    }
    writeln!(markdown, "## Limitations").expect("writing to String cannot fail");
    for limitation in &report.limitations {
        writeln!(markdown, "- {limitation}").expect("writing to String cannot fail");
    }
    markdown
}

fn format_p9999_measurement(micros: Option<u64>, status: P9999Status) -> String {
    micros.map_or_else(
        || format!("unavailable ({})", status.label()),
        |micros| format!("{micros} us"),
    )
}

fn print_summary(report: &DeviceFeasibilityReport, output_directory: &Path) {
    println!("DEVICE_IPC_FEASIBILITY");
    println!(
        "  {} rack(s), {} frames, {} second(s), coreaudio_callback_attached={}",
        report.configuration.rack_count,
        report.configuration.frame_count,
        report.configuration.requested_duration_seconds,
        report.callback_stats.callbacks > 0 && report.callback_telemetry.callbacks > 0,
    );
    println!(
        "  callbacks={}, accepted_completions={}, deadline_misses={}, overruns={}, protocol_faults={}, worker_exits={}, last_error={} ({})",
        report.callback_stats.callbacks,
        report.callback_stats.accepted_completions,
        report.callback_stats.deadline_misses,
        report.callback_stats.callback_overruns,
        report.callback_stats.protocol_faults,
        report.callback_stats.worker_exits,
        report.callback_stats.last_error_code,
        report.callback_stats.last_error_label,
    );
    println!(
        "  timing_passed={}, active_device_preflight={}, phase1_hard_gate_certified=false; artifacts={}",
        report.timing_thresholds.passed,
        report.acceptance_passed,
        output_directory.display(),
    );
}

fn duration_to_micros_ceil(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos().div_ceil(1_000)).unwrap_or(u64::MAX)
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };

    use sp_audio_io_macos::{
        InterleavedStereoF32, PhaseOneFrames, PhaseOneRenderer, RenderDisposition,
    };
    use sp_shared_memory_macos::MonotonicClock;

    use super::{
        CallbackErrorCode, DeviceIpcRenderer, HistogramEvidence, HistogramSnapshot,
        InternalEnergyEvidence, P9999Status, ProcessEnergyBaseline, ProcessEnergyReading,
        SharedDeviceStats, TimingCapture, TimingEvidence, TimingSnapshot, TimingThresholds,
        parse_options,
    };
    use crate::phase1::{DeviceHarness, TimingHistograms, fixed_request};

    #[test]
    fn short_options_select_output_directory_and_duration() {
        let options = parse_options(
            &[
                "-r",
                "2",
                "-f",
                "256",
                "-d",
                "3",
                "-o",
                "evidence",
                "-e",
                "energy.json",
            ]
            .map(str::to_owned),
        )
        .expect("short device options parse");
        assert_eq!(options.rack_count, 2);
        assert_eq!(options.frame_count, 256);
        assert_eq!(options.duration, Duration::from_secs(3));
        assert_eq!(
            options.output_directory,
            std::path::PathBuf::from("evidence")
        );
        assert_eq!(
            options.energy_evidence_path,
            Some(std::path::PathBuf::from("energy.json"))
        );
        assert!(
            parse_options(&["-r", "2", "--racks", "4", "-f", "128", "-d", "1"].map(str::to_owned))
                .is_err()
        );
    }

    #[test]
    fn parses_calibrated_load_and_targeted_faults_without_mixing_them() {
        let calibrated = parse_options(
            &[
                "--racks",
                "8",
                "--frames",
                "128",
                "--duration-seconds",
                "1",
                "--compute-load-mode",
                "calibrated-cpu",
                "--compute-load-micros",
                "267",
            ]
            .map(str::to_owned),
        )
        .expect("calibrated options parse");
        assert_eq!(
            calibrated.worker.compute_load.mode,
            sp_test_support::ComputeLoadMode::CalibratedCpu
        );
        assert_eq!(
            calibrated.worker.compute_load.duration,
            Duration::from_micros(267)
        );
        assert!(calibrated.worker.target_rack.is_none());

        let fault = parse_options(
            &[
                "--racks",
                "2",
                "--frames",
                "128",
                "--duration-seconds",
                "1",
                "--fault-mode",
                "hang-after-claim",
                "--fault-target-rack",
                "1",
                "--fault-trigger-sequence",
                "2",
            ]
            .map(str::to_owned),
        )
        .expect("fault options parse");
        assert_eq!(fault.worker.target_rack, Some(0));
        assert_eq!(
            fault.worker.fault.mode,
            sp_test_support::FaultMode::HangAfterClaim
        );
        assert!(
            parse_options(
                &[
                    "--racks",
                    "2",
                    "--frames",
                    "128",
                    "--duration-seconds",
                    "1",
                    "--fault-mode",
                    "hang-after-claim",
                    "--fault-target-rack",
                    "1",
                    "--fault-trigger-sequence",
                    "2",
                    "--compute-load-mode",
                    "calibrated-cpu",
                    "--compute-load-micros",
                    "1",
                ]
                .map(str::to_owned),
            )
            .is_err()
        );
    }

    #[test]
    fn timing_thresholds_distinguish_underpowered_preflights_from_official_evidence() {
        fn histogram(sample_count: u64, value: u64) -> HistogramSnapshot {
            let mut buckets = vec![0; usize::try_from(value).unwrap() + 1];
            buckets[usize::try_from(value).unwrap()] = sample_count;
            HistogramSnapshot {
                bucket_width_micros: 1,
                buckets,
                sample_count,
                overflow_count: 0,
                max_micros: value,
            }
        }
        let timing = |sample_count, completion_micros| TimingEvidence {
            request_to_claim: HistogramEvidence::from_snapshot(histogram(sample_count, 1)),
            processing: HistogramEvidence::from_snapshot(histogram(sample_count, 1)),
            request_to_completion: HistogramEvidence::from_snapshot(histogram(sample_count, 1)),
            completion_publication_to_observation: HistogramEvidence::from_snapshot(histogram(
                sample_count,
                1,
            )),
            completion_observation: HistogramEvidence::from_snapshot(histogram(
                sample_count,
                completion_micros,
            )),
            observe_dispatch_work: HistogramEvidence::from_snapshot(histogram(sample_count, 1)),
            callback_duration: HistogramEvidence::from_snapshot(histogram(sample_count, 1)),
        };

        let underpowered = timing(9_999, 196);
        assert_eq!(
            underpowered.completion_observation.p9999_status,
            P9999Status::StatisticallyUnderpowered
        );
        assert_eq!(underpowered.completion_observation.p9999_micros, None);
        assert_eq!(underpowered.completion_observation.raw.max_micros, 196);
        let short_preflight =
            TimingThresholds::evaluate(&underpowered, Duration::from_millis(10), false);
        assert!(short_preflight.passed);
        assert_eq!(short_preflight.dispatch_p9999_micros, None);
        assert_eq!(short_preflight.dispatch_maximum_micros, 196);
        assert!(!TimingThresholds::evaluate(&underpowered, Duration::from_millis(10), true).passed);

        let observable = timing(10_000, 150);
        assert_eq!(
            observable.completion_observation.p9999_status,
            P9999Status::Available
        );
        assert_eq!(observable.completion_observation.p9999_micros, Some(150));
        assert!(TimingThresholds::evaluate(&observable, Duration::from_millis(10), true).passed);

        let excessive_maximum = timing(9_999, 400);
        assert!(
            !TimingThresholds::evaluate(&excessive_maximum, Duration::from_millis(10), false,)
                .passed
        );
    }

    #[test]
    fn callback_error_codes_are_fixed_and_fatal() {
        let stats = SharedDeviceStats::default();
        stats.record_callback_error(CallbackErrorCode::ProtocolFault);
        stats.record_callback_error(CallbackErrorCode::CallbackOverrun);
        let snapshot = stats.snapshot();
        assert!(snapshot.fatal_error);
        assert_eq!(
            snapshot.first_error_code,
            CallbackErrorCode::ProtocolFault as u32
        );
        assert_eq!(
            snapshot.last_error_code,
            CallbackErrorCode::CallbackOverrun as u32
        );
    }

    #[test]
    fn expected_self_crash_retains_live_worker_energy_certification() {
        let available = |raw_nanojoules| ProcessEnergyReading {
            availability: "available",
            raw_nanojoules: Some(raw_nanojoules),
            joules: Some(f64::from(u32::try_from(raw_nanojoules).unwrap()) / 1_000_000_000.0),
            error: None,
        };
        let departed = ProcessEnergyReading {
            availability: "error",
            raw_nanojoules: None,
            joules: None,
            error: Some("no such process after planned self-crash".to_owned()),
        };
        let evidence = InternalEnergyEvidence::from_baselines(
            ProcessEnergyBaseline {
                host: (10, available(100)),
                workers: vec![(11, available(100)), (12, available(100))],
            },
            ProcessEnergyBaseline {
                host: (10, available(200)),
                workers: vec![(11, departed), (12, available(200))],
            },
            Duration::from_secs(1),
            Some(11),
        );
        assert!(evidence.certification_complete);
        assert_eq!(evidence.expected_departed_worker_process_ids, vec![11]);
        assert!(evidence.workers[0].expected_to_exit);
        assert!(!evidence.workers[0].complete);
        assert!(evidence.workers[1].complete);
    }

    #[test]
    fn targeted_protocol_fault_keeps_an_unaffected_rack_running() {
        let stats = SharedDeviceStats::new(Some(0));
        let target_fault = crate::phase1::DeviceBlockCounters {
            protocol_faults: 1,
            protocol_faults_by_rack: std::array::from_fn(|rack| u64::from(rack == 0)),
            fallback_events: 1,
            ..Default::default()
        };
        stats.record_callback_outcome(&target_fault);
        assert!(
            !stats.snapshot().fatal_error,
            "the target rack must contain its malformed completion"
        );

        let unaffected_completion = crate::phase1::DeviceBlockCounters {
            accepted_racks: 1,
            accepted_by_rack: std::array::from_fn(|rack| u64::from(rack == 1)),
            ..Default::default()
        };
        stats.record_callback_outcome(&unaffected_completion);
        let snapshot = stats.snapshot();
        assert!(!snapshot.fatal_error);
        assert_eq!(snapshot.accepted_completions_by_rack[1], 1);
        assert_eq!(snapshot.protocol_faults_by_rack[0], 1);
    }

    #[test]
    fn renderer_executes_the_fixed_callback_path_without_a_worker_process_handle() {
        let stats = Arc::new(SharedDeviceStats::default());
        let stop = Arc::new(AtomicBool::new(false));
        let timing_capture = Arc::new(TimingCapture::default());
        let clock = MonotonicClock::new().unwrap();
        {
            let mut renderer = DeviceIpcRenderer {
                harness: DeviceHarness::empty_for_test(),
                clock,
                request: fixed_request(128),
                completion_budget_ticks: clock.duration_to_ticks(Duration::from_millis(7)),
                callback_period_ticks: clock.duration_to_ticks(Duration::from_millis(10)),
                block_index: 0,
                timing: TimingHistograms::default(),
                callback_duration: super::RealtimeHistogram::new(),
                timing_capture: Arc::clone(&timing_capture),
                stats: Arc::clone(&stats),
                stop,
            };
            let mut samples = [0.0_f32; 256];
            let output =
                InterleavedStereoF32::new(&mut samples, PhaseOneFrames::Frames128).unwrap();

            assert_eq!(renderer.render(output), RenderDisposition::Silence);
            assert_eq!(stats.callbacks.load(Ordering::Acquire), 1);
            assert_eq!(renderer.block_index, 1);
        }
        let timing = timing_capture
            .take()
            .expect("renderer drops timing on control thread");
        assert_eq!(timing.callback_duration.sample_count, 1);
    }

    #[test]
    fn timing_snapshot_uses_phase_one_histogram_field_names() {
        let json = r#"{
            "request_to_claim":{"bucket_width_micros":1,"buckets":[1],"sample_count":1,"overflow_count":0,"max_micros":0},
            "claim_to_completion":{"bucket_width_micros":1,"buckets":[1],"sample_count":1,"overflow_count":0,"max_micros":0},
            "request_to_completion":{"bucket_width_micros":1,"buckets":[1],"sample_count":1,"overflow_count":0,"max_micros":0},
            "completion_to_observation":{"bucket_width_micros":1,"buckets":[1],"sample_count":1,"overflow_count":0,"max_micros":0},
            "request_to_observation":{"bucket_width_micros":1,"buckets":[1],"sample_count":1,"overflow_count":0,"max_micros":0},
            "synthetic_callback_work":{"bucket_width_micros":1,"buckets":[1],"sample_count":1,"overflow_count":0,"max_micros":0}
        }"#;
        let snapshot: TimingSnapshot = serde_json::from_str(json).unwrap();
        assert_eq!(snapshot.request_to_claim.sample_count, 1);
        assert_eq!(snapshot.synthetic_callback_work.sample_count, 1);
    }
}
