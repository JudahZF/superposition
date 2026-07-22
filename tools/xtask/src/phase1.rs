//! Deterministic synthetic Phase 1 preflight and fault-matrix harness.
//!
//! This module deliberately models a synthetic callback. It is not attached to a
//! `CoreAudio` device callback and therefore cannot certify the Phase 1 hard gate.

use std::{
    collections::BTreeSet,
    fmt::Write as _,
    fs,
    io::{BufRead, BufReader, Write as _},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sp_engine::{FallbackReason, GateOutcome, RackGate, RackGateState, WorkerObservation};
use sp_shared_memory::{
    BLOCK_SLOT_COUNT, BlockRequest, BlockTicket, BlockTiming, MAX_RACKS, ProtocolError, SlotState,
};
use sp_shared_memory_macos::{
    MonotonicClock, ProcessResourceUsage, SharedMemoryRegion, child_process_resource_usage,
    current_process_resource_usage,
};
use sp_test_support::{
    ComputeLoadConfiguration, FaultConfiguration, FaultMode, SELF_CRASH_AFTER_CLAIM_MODE,
};

const SAMPLE_RATE_HZ: u32 = 48_000;
const WORKER_READY: &str = "ready";
const WORKER_READINESS_TIMEOUT: Duration = Duration::from_secs(5);
const WORKER_GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(250);
const WORKER_EXIT_POLL_INTERVAL: Duration = Duration::from_millis(5);
const HISTOGRAM_MAX_MICROS: usize = 10_000;
const HISTOGRAM_BUCKET_COUNT: usize = HISTOGRAM_MAX_MICROS + 1;
const REPORT_VERSION: u32 = 1;
const FAULT_TRIGGER_SEQUENCE: u64 = 2;
const PHASE1_CERTIFICATION_DURATION_SECONDS: u64 = 1_800;

/// Successful command completion with a process exit status.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CommandOutcome {
    pub(crate) exit_code: u8,
}

impl CommandOutcome {
    pub(crate) const fn passed() -> Self {
        Self { exit_code: 0 }
    }

    pub(crate) const fn acceptance_failure() -> Self {
        Self { exit_code: 1 }
    }

    pub(crate) const fn evidence_incomplete() -> Self {
        Self { exit_code: 3 }
    }
}

/// Distinguishes invalid input and unavailable infrastructure from an acceptance failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Phase1Error {
    InvalidConfiguration(String),
    Infrastructure(String),
}

impl Phase1Error {
    #[must_use]
    pub(crate) const fn exit_code() -> u8 {
        2
    }
}

impl std::fmt::Display for Phase1Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidConfiguration(message) | Self::Infrastructure(message) => {
                formatter.write_str(message)
            }
        }
    }
}

/// Fixed 1 microsecond histogram with an explicit overflow counter.
///
/// The bins are allocated once at construction and sample values are never retained.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct FixedHistogram {
    bucket_width_micros: u64,
    buckets: Box<[u64]>,
    sample_count: u64,
    overflow_count: u64,
    max_micros: u64,
}

impl FixedHistogram {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            bucket_width_micros: 1,
            buckets: vec![0; HISTOGRAM_BUCKET_COUNT].into_boxed_slice(),
            sample_count: 0,
            overflow_count: 0,
            max_micros: 0,
        }
    }

    pub(crate) fn observe(&mut self, duration: Duration) {
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

    fn merge(&mut self, other: &Self) {
        for (bucket, other_bucket) in self.buckets.iter_mut().zip(other.buckets.iter()) {
            *bucket = bucket.saturating_add(*other_bucket);
        }
        self.sample_count = self.sample_count.saturating_add(other.sample_count);
        self.overflow_count = self.overflow_count.saturating_add(other.overflow_count);
        self.max_micros = self.max_micros.max(other.max_micros);
    }

    #[must_use]
    pub(crate) const fn sample_count(&self) -> u64 {
        self.sample_count
    }

    #[must_use]
    pub(crate) const fn max_duration(&self) -> Duration {
        Duration::from_micros(self.max_micros)
    }

    /// Returns the conservative upper edge of the selected 1 microsecond bin.
    /// Counts every retained observation at or above a threshold, including overflow samples.
    #[must_use]
    fn samples_at_or_above(&self, threshold_micros: u64) -> u64 {
        let start = usize::try_from(threshold_micros).unwrap_or(usize::MAX);
        self.buckets
            .iter()
            .enumerate()
            .filter(|(index, _)| *index >= start)
            .map(|(_, count)| *count)
            .fold(self.overflow_count, u64::saturating_add)
    }

    #[must_use]
    pub(crate) fn percentile_upper_bound(&self, numerator: u64, denominator: u64) -> Duration {
        if self.sample_count == 0 || denominator == 0 || numerator == 0 {
            return Duration::ZERO;
        }
        let rank = self
            .sample_count
            .saturating_mul(numerator)
            .div_ceil(denominator)
            .max(1);
        let mut cumulative = 0_u64;
        for (index, count) in self.buckets.iter().enumerate() {
            cumulative = cumulative.saturating_add(*count);
            if cumulative >= rank {
                return Duration::from_micros(u64::try_from(index).unwrap_or(u64::MAX));
            }
        }
        // An overflow sample has no tighter fixed bin. The recorded maximum remains
        // conservative and makes an overflow visible in the serialized model.
        Duration::from_micros(self.max_micros)
    }
}

impl Default for FixedHistogram {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub(crate) struct TimingHistograms {
    request_to_claim: FixedHistogram,
    claim_to_completion: FixedHistogram,
    request_to_completion: FixedHistogram,
    completion_to_observation: FixedHistogram,
    request_to_observation: FixedHistogram,
    synthetic_callback_work: FixedHistogram,
}

impl TimingHistograms {
    fn observe_timing(
        &mut self,
        timing: BlockTiming,
        completion_observed_tick: u64,
        clock: MonotonicClock,
    ) {
        self.request_to_claim.observe(
            clock.ticks_to_duration(
                timing
                    .worker_claimed_tick
                    .saturating_sub(timing.request_published_tick),
            ),
        );
        self.claim_to_completion.observe(
            clock.ticks_to_duration(
                timing
                    .completion_published_tick
                    .saturating_sub(timing.worker_claimed_tick),
            ),
        );
        self.request_to_completion.observe(
            clock.ticks_to_duration(
                timing
                    .completion_published_tick
                    .saturating_sub(timing.request_published_tick),
            ),
        );
        self.completion_to_observation
            .observe(clock.ticks_to_duration(
                completion_observed_tick.saturating_sub(timing.completion_published_tick),
            ));
        self.request_to_observation.observe(clock.ticks_to_duration(
            completion_observed_tick.saturating_sub(timing.request_published_tick),
        ));
    }

    fn observe_callback_work(&mut self, duration: Duration) {
        self.synthetic_callback_work.observe(duration);
    }

    fn merge(&mut self, other: &Self) {
        self.request_to_claim.merge(&other.request_to_claim);
        self.claim_to_completion.merge(&other.claim_to_completion);
        self.request_to_completion
            .merge(&other.request_to_completion);
        self.completion_to_observation
            .merge(&other.completion_to_observation);
        self.request_to_observation
            .merge(&other.request_to_observation);
        self.synthetic_callback_work
            .merge(&other.synthetic_callback_work);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FeasibilityOptions {
    rack_count: usize,
    frame_count: u32,
    duration: Duration,
    output_directory: Option<PathBuf>,
    energy_evidence_path: Option<PathBuf>,
    require_energy_evidence: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct IpcMatrixOptions {
    duration: Duration,
    output_directory: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FaultMatrixOptions {
    rack_count: usize,
    frame_count: u32,
    output_directory: PathBuf,
    energy_evidence_path: Option<PathBuf>,
    require_energy_evidence: bool,
}

/// Control-plane-only launch settings for workers attached to an AUHAL feasibility run.
///
/// The configuration is resolved before callback startup. Only the selected target rack receives
/// an injected fault; every other rack stays on the normal worker path so the device report can
/// distinguish isolated fallback from a whole-harness failure.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct DeviceWorkerConfiguration {
    pub(crate) fault: FaultConfiguration,
    pub(crate) self_crash_after_claim: bool,
    pub(crate) compute_load: ComputeLoadConfiguration,
    pub(crate) bundle: Option<PathBuf>,
    pub(crate) target_rack: Option<usize>,
}

impl DeviceWorkerConfiguration {
    #[must_use]
    pub(crate) fn needs_fault_injection(&self) -> bool {
        matches!(
            self.fault.mode,
            FaultMode::MalformedCompletion | FaultMode::StaleCompletion
        )
    }

    #[must_use]
    fn for_rack(&self, rack_index: usize) -> Self {
        if self.target_rack.is_none_or(|target| target == rack_index) {
            self.clone()
        } else {
            Self::default()
        }
    }
}

pub(crate) fn ensure_phase1_platform() -> Result<(), Phase1Error> {
    if std::env::consts::OS == "macos" && std::env::consts::ARCH == "aarch64" {
        Ok(())
    } else {
        Err(Phase1Error::Infrastructure(format!(
            "Phase 1 synthetic process evidence requires macOS/aarch64; detected {}/{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )))
    }
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
    Command::new(command)
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
    Command::new("git")
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

fn required_ipc_matrix_cells() -> Vec<String> {
    [1, 2, 4, 8]
        .into_iter()
        .flat_map(|racks| [128, 256].map(move |frames| format!("{racks}r-{frames}f")))
        .collect()
}

/// Executes the existing synthetic IPC preflight through the timed protocol path.
pub(crate) fn run_ipc_feasibility(
    workspace_root: &Path,
    arguments: &[String],
) -> Result<CommandOutcome, Phase1Error> {
    ensure_phase1_platform()?;
    let options =
        parse_feasibility_options(arguments).map_err(Phase1Error::InvalidConfiguration)?;
    let environment = collect_environment(workspace_root);
    let mut energy = load_energy_evidence(
        options.energy_evidence_path.as_deref(),
        "ipc_feasibility",
        options.rack_count,
        options.frame_count,
        &environment.hardware_model,
    )?;
    energy.required = options.require_energy_evidence;
    let worker = build_worker(workspace_root, false).map_err(Phase1Error::Infrastructure)?;
    let clock = MonotonicClock::new().map_err(|error| {
        Phase1Error::Infrastructure(format!("could not initialize continuous clock: {error}"))
    })?;
    let output_directory = options.output_directory.clone().unwrap_or_else(|| {
        workspace_root
            .join("target")
            .join("phase1")
            .join("ipc-feasibility")
    });
    let mut report = execute_ipc_trial(&options, &worker, clock, environment, energy);
    assign_ipc_report_id(&mut report)?;
    write_ipc_report(&output_directory, &report)?;
    print_ipc_summary(&report, &output_directory);
    ipc_outcome(&report)
}

/// Runs the complete synthetic 1/2/4/8 rack by 128/256 frame development matrix.
pub(crate) fn run_ipc_matrix(
    workspace_root: &Path,
    arguments: &[String],
) -> Result<CommandOutcome, Phase1Error> {
    ensure_phase1_platform()?;
    let options = parse_ipc_matrix_options(arguments).map_err(Phase1Error::InvalidConfiguration)?;
    let environment = collect_environment(workspace_root);
    let worker = build_worker(workspace_root, false).map_err(Phase1Error::Infrastructure)?;
    let clock = MonotonicClock::new().map_err(|error| {
        Phase1Error::Infrastructure(format!("could not initialize continuous clock: {error}"))
    })?;
    let mut cells = Vec::with_capacity(8);
    for rack_count in [1, 2, 4, 8] {
        for frame_count in [128, 256] {
            let directory_name = format!("{rack_count}r-{frame_count}f");
            let cell_directory = options.output_directory.join(&directory_name);
            let cell_options = FeasibilityOptions {
                rack_count,
                frame_count,
                duration: options.duration,
                output_directory: Some(cell_directory.clone()),
                energy_evidence_path: None,
                require_energy_evidence: false,
            };
            let mut report = execute_ipc_trial(
                &cell_options,
                &worker,
                clock,
                environment.clone(),
                empty_energy_evidence(),
            );
            assign_ipc_report_id(&mut report)?;
            write_ipc_report(&cell_directory, &report)?;
            cells.push(IpcMatrixCell {
                rack_count,
                frame_count,
                directory: directory_name,
                report_id: report.report_id,
                acceptance_passed: report.acceptance_passed,
                evidence_complete: report.evidence_complete,
                infrastructure_error: report.infrastructure_error,
            });
        }
    }
    let acceptance_passed = cells.iter().all(|cell| cell.acceptance_passed);
    let evidence_complete = cells.iter().all(|cell| cell.evidence_complete);
    let infrastructure_errors = cells
        .iter()
        .filter_map(|cell| {
            cell.infrastructure_error
                .as_ref()
                .map(|error| format!("{}r-{}f: {error}", cell.rack_count, cell.frame_count))
        })
        .collect::<Vec<_>>();
    let duration_qualifies = options.duration >= Duration::from_mins(30);
    let mut matrix = IpcMatrixReport {
        report_version: REPORT_VERSION,
        report_id: String::new(),
        labels: preflight_labels(),
        environment,
        qualification: QualificationStatus {
            status: if duration_qualifies && acceptance_passed {
                "synthetic_matrix_criteria_complete"
            } else {
                "development_nonqualifying"
            },
            qualifying: duration_qualifies && acceptance_passed,
            official_duration_seconds: 1_800,
            required_matrix_cells: required_ipc_matrix_cells(),
            detail: if duration_qualifies {
                "All eight synthetic cells used the official minimum duration; active-device certification remains false."
            } else {
                "Short matrix duration is development evidence only; official synthetic evidence requires at least 1800 seconds per cell."
            }
            .to_owned(),
        },
        duration_seconds: options.duration.as_secs(),
        cells,
        acceptance_passed,
        evidence_complete,
        infrastructure_errors,
        limitations: phase1_limitations(),
    };
    assign_ipc_matrix_report_id(&mut matrix)?;
    write_ipc_matrix_report(&options.output_directory, &matrix)?;
    println!(
        "SYNTHETIC_IPC_MATRIX: acceptance={}, qualification={}, artifacts={}",
        matrix.acceptance_passed,
        matrix.qualification.status,
        options.output_directory.display()
    );
    command_outcome(
        matrix.infrastructure_errors.first().map(String::as_str),
        matrix.acceptance_passed,
        matrix.evidence_complete,
    )
}

#[allow(clippy::too_many_lines)]
fn execute_ipc_trial(
    options: &FeasibilityOptions,
    worker: &Path,
    clock: MonotonicClock,
    environment: EnvironmentEvidence,
    energy_evidence: EnergyEvidence,
) -> IpcReport {
    let period = block_period(options.frame_count);
    let host_before = current_process_resource_usage().ok();
    let children_before = child_process_resource_usage().ok();
    let mut timing = TimingHistograms::default();
    let mut counters = FaultCounters::default();
    let mut infrastructure_error = None;
    let mut acceptance_failure = None;
    match start_harness(options.rack_count, 1, worker, FaultCase::None, period) {
        Ok(mut harness) => {
            let block_count = duration_to_blocks(options.duration, period).unwrap_or(1);
            let request = fixed_request(options.frame_count);
            for block in 0..block_count {
                match run_synthetic_block(
                    &mut harness,
                    block,
                    request,
                    clock,
                    period,
                    true,
                    FaultCase::None,
                    &mut timing,
                ) {
                    Ok(result) => {
                        counters.merge(&result.counters);
                        if let Some(reason) = result.first_fault {
                            acceptance_failure =
                                Some(format!("rack gate closed at block {block}: {reason:?}"));
                            break;
                        }
                    }
                    Err(error) => {
                        infrastructure_error = Some(error);
                        break;
                    }
                }
            }
            if acceptance_failure.is_none() && infrastructure_error.is_none() {
                match run_synthetic_block(
                    &mut harness,
                    block_count,
                    request,
                    clock,
                    period,
                    false,
                    FaultCase::None,
                    &mut timing,
                ) {
                    Ok(result) => {
                        counters.merge(&result.counters);
                        if let Some(reason) = result.first_fault {
                            acceptance_failure = Some(format!(
                                "rack gate closed while draining block {block_count}: {reason:?}"
                            ));
                        }
                    }
                    Err(error) => infrastructure_error = Some(error),
                }
            }
            drop(harness);
        }
        Err(error) => infrastructure_error = Some(error),
    }
    let timing_thresholds = timing_thresholds(&timing, period);
    // Synthetic pacing intentionally characterizes scheduler wake behavior. The attached-device
    // artifacts alone enforce the Phase 1 timing limits, so a sleep-paced wake outlier is
    // retained in the histogram but does not turn a behavioral containment preflight into a
    // false device-timing failure.
    let acceptance_passed = infrastructure_error.is_none() && acceptance_failure.is_none();
    let mut cpu_evidence = CpuEvidence::new(host_before, children_before);
    cpu_evidence.finish(
        current_process_resource_usage().ok(),
        child_process_resource_usage().ok(),
    );
    let evidence_complete = !options.require_energy_evidence || energy_evidence.imported;
    let duration_qualifies = options.duration >= Duration::from_mins(30);
    IpcReport {
        report_version: REPORT_VERSION,
        report_id: String::new(),
        labels: preflight_labels(),
        environment,
        qualification: QualificationStatus {
            status: if duration_qualifies && options.rack_count == 8 {
                "single_cell_duration_complete_matrix_incomplete"
            } else {
                "development_nonqualifying"
            },
            qualifying: false,
            official_duration_seconds: 1_800,
            required_matrix_cells: required_ipc_matrix_cells(),
            detail: "A single ipc-feasibility cell never qualifies the full synthetic matrix; use ipc-matrix for all eight cells."
                .to_owned(),
        },
        configuration: IpcConfiguration {
            rack_count: options.rack_count,
            frame_count: options.frame_count,
            duration_seconds: options.duration.as_secs(),
            sample_rate_hz: SAMPLE_RATE_HZ,
            block_period_micros: duration_to_micros_ceil(period),
        },
        scheduler_characterization: SyntheticSchedulerCharacterization::from_timing(&timing),
        timing_histograms: timing,
        timing_thresholds,
        counters,
        cpu_evidence,
        energy_evidence,
        acceptance_passed,
        evidence_complete,
        acceptance_failure,
        infrastructure_error,
        limitations: phase1_limitations(),
    }
}

fn ipc_outcome(report: &IpcReport) -> Result<CommandOutcome, Phase1Error> {
    command_outcome(
        report.infrastructure_error.as_deref(),
        report.acceptance_passed,
        report.evidence_complete,
    )
}

fn preflight_labels() -> PreflightLabels {
    PreflightLabels {
        scope: "synthetic_preflight",
        future_scope: "active_device_callback",
        coreaudio_callback_attached: false,
        phase1_hard_gate_certified: false,
    }
}

fn phase1_limitations() -> Vec<&'static str> {
    vec![
        "Synthetic worker-process preflight only; no active CoreAudio callback is attached.",
        "Sleep-paced synthetic scheduler wake outliers are retained in fixed histograms and reported separately; they are never relabeled as attached-device timing evidence.",
        "Real-process timing results are manual evidence and are not CI-certifying.",
        "CPU evidence covers the xtask host and accumulated reaped children, not per-worker attribution or device energy consumption.",
        "Energy evidence is imported only from a validated structured record and is never inferred or fabricated.",
        "The synthetic worker has no descendants; process-group descendant cleanup remains a later real plug-in supervisor requirement.",
    ]
}

fn empty_energy_evidence() -> EnergyEvidence {
    EnergyEvidence {
        status: "not_supplied",
        required: false,
        imported: false,
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
    }
}

/// Executes and reports the deterministic Phase 1 synthetic worker fault matrix.
pub(crate) fn run_fault_matrix(
    workspace_root: &Path,
    arguments: &[String],
) -> Result<CommandOutcome, Phase1Error> {
    ensure_phase1_platform()?;
    let options =
        parse_fault_matrix_options(arguments).map_err(Phase1Error::InvalidConfiguration)?;
    let environment = collect_environment(workspace_root);
    let mut energy = load_energy_evidence(
        options.energy_evidence_path.as_deref(),
        "fault_matrix",
        options.rack_count,
        options.frame_count,
        &environment.hardware_model,
    )?;
    energy.required = options.require_energy_evidence;
    let clock = MonotonicClock::new().map_err(|error| {
        Phase1Error::Infrastructure(format!("could not initialize continuous clock: {error}"))
    })?;
    let period = block_period(options.frame_count);
    let cpu_before = current_process_resource_usage().ok();
    let children_before = child_process_resource_usage().ok();
    let mut report = FaultMatrixReport::new(
        &options,
        period,
        energy,
        environment,
        cpu_before,
        children_before,
    );

    let worker = match build_worker(workspace_root, true) {
        Ok(worker) => worker,
        Err(error) => {
            report.infrastructure_error = Some(error);
            return finish_fault_matrix(&options, report);
        }
    };

    for (case_index, case) in FaultCase::ALL.into_iter().enumerate() {
        match run_fault_case(case, case_index, &options, &worker, clock, period) {
            Ok(trial) => report.trials.push(trial),
            Err(error) => {
                report.infrastructure_error = Some(error);
                break;
            }
        }
    }

    finish_fault_matrix(&options, report)
}

fn finish_fault_matrix(
    options: &FaultMatrixOptions,
    mut report: FaultMatrixReport,
) -> Result<CommandOutcome, Phase1Error> {
    report.cpu_evidence.finish(
        current_process_resource_usage().ok(),
        child_process_resource_usage().ok(),
    );
    report.baseline.passed =
        !report.trials.is_empty() && report.trials.iter().all(|trial| trial.baseline_passed);
    report.baseline.timing_histograms = aggregate_baseline_histograms(
        report
            .trials
            .iter()
            .map(|trial| &trial.baseline_timing_histograms),
    );
    report.acceptance_passed = report.infrastructure_error.is_none()
        && report.trials.len() == FaultCase::ALL.len()
        && report.trials.iter().all(|trial| trial.passed);
    report.evidence_complete = !options.require_energy_evidence || report.energy_evidence.imported;
    assign_fault_report_id(&mut report)?;
    write_report(&options.output_directory, &report)?;
    println!(
        "SYNTHETIC_FAULT_MATRIX: acceptance={}, evidence_complete={}, qualification={}, artifacts={}",
        report.acceptance_passed,
        report.evidence_complete,
        report.qualification.status,
        options.output_directory.display(),
    );

    command_outcome(
        report.infrastructure_error.as_deref(),
        report.acceptance_passed,
        report.evidence_complete,
    )
}

fn aggregate_baseline_histograms<'a>(
    timings: impl IntoIterator<Item = &'a TimingHistograms>,
) -> Option<TimingHistograms> {
    let mut timings = timings.into_iter();
    let first = timings.next()?;
    let mut aggregate = first.clone();
    for timing in timings {
        aggregate.merge(timing);
    }
    Some(aggregate)
}

fn command_outcome(
    infrastructure_error: Option<&str>,
    acceptance_passed: bool,
    evidence_complete: bool,
) -> Result<CommandOutcome, Phase1Error> {
    if let Some(error) = infrastructure_error {
        return Err(Phase1Error::Infrastructure(error.to_owned()));
    }
    if !acceptance_passed {
        return Ok(CommandOutcome::acceptance_failure());
    }
    if !evidence_complete {
        return Ok(CommandOutcome::evidence_incomplete());
    }
    Ok(CommandOutcome::passed())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FaultCase {
    None,
    Kill,
    HangAfterClaim,
    DelayBeforeClaimTimely,
    LateCompletion,
    MalformedCompletion,
    StaleSequenceCompletion,
    StaleGenerationCompletion,
}

impl FaultCase {
    const ALL: [Self; 7] = [
        Self::Kill,
        Self::HangAfterClaim,
        Self::DelayBeforeClaimTimely,
        Self::LateCompletion,
        Self::MalformedCompletion,
        Self::StaleSequenceCompletion,
        Self::StaleGenerationCompletion,
    ];

    const fn id(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Kill => "kill",
            Self::HangAfterClaim => "hang-after-claim",
            Self::DelayBeforeClaimTimely => "delay-before-claim-timely",
            Self::LateCompletion => "late-completion",
            Self::MalformedCompletion => "malformed-completion",
            Self::StaleSequenceCompletion => "stale-completion-sequence",
            Self::StaleGenerationCompletion => "stale-completion-generation",
        }
    }

    const fn injection_layer(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Kill => "xtask_control_plane_process_signal",
            Self::HangAfterClaim | Self::DelayBeforeClaimTimely | Self::LateCompletion => {
                "worker_sp_test_support_fault_option"
            }
            Self::MalformedCompletion | Self::StaleSequenceCompletion => {
                "worker_sp_test_support_protocol_test_hook"
            }
            Self::StaleGenerationCompletion => "xtask_gate_adapter_boundary",
        }
    }

    const fn expected_result(self) -> &'static str {
        match self {
            Self::None | Self::DelayBeforeClaimTimely => "accepted",
            Self::Kill => "fallback:worker_exited",
            Self::HangAfterClaim | Self::LateCompletion => "fallback:deadline_miss",
            Self::MalformedCompletion => "fallback:malformed_completion",
            Self::StaleSequenceCompletion | Self::StaleGenerationCompletion => {
                "fallback:stale_completion"
            }
        }
    }

    const fn expected_fallback(self) -> Option<FallbackReason> {
        match self {
            Self::None | Self::DelayBeforeClaimTimely => None,
            Self::Kill => Some(FallbackReason::WorkerExited),
            Self::HangAfterClaim | Self::LateCompletion => Some(FallbackReason::DeadlineMiss),
            Self::MalformedCompletion => Some(FallbackReason::MalformedCompletion),
            Self::StaleSequenceCompletion | Self::StaleGenerationCompletion => {
                Some(FallbackReason::StaleCompletion)
            }
        }
    }

    const fn fatal(self) -> bool {
        self.expected_fallback().is_some()
    }

    fn worker_fault(self, period: Duration) -> FaultConfiguration {
        let mode = match self {
            Self::HangAfterClaim => FaultMode::HangAfterClaim,
            Self::DelayBeforeClaimTimely => FaultMode::DelayBeforeClaim,
            Self::LateCompletion => FaultMode::LateCompletion,
            Self::MalformedCompletion => FaultMode::MalformedCompletion,
            Self::StaleSequenceCompletion => FaultMode::StaleCompletion,
            Self::None | Self::Kill | Self::StaleGenerationCompletion => FaultMode::None,
        };
        let fault_delay = match self {
            Self::DelayBeforeClaimTimely => {
                Duration::from_micros(u64::try_from(period.as_micros().max(1) / 8).unwrap_or(1))
            }
            Self::LateCompletion => period.saturating_add(Duration::from_millis(1)),
            _ => Duration::ZERO,
        };
        FaultConfiguration {
            mode,
            trigger_request_sequence: FAULT_TRIGGER_SEQUENCE,
            fault_delay,
            work_duration: Duration::ZERO,
        }
        .validate()
        .expect("fixed Phase 1 fault configuration is valid")
    }
}

#[allow(clippy::too_many_lines)]
fn run_fault_case(
    case: FaultCase,
    case_index: usize,
    options: &FaultMatrixOptions,
    worker: &Path,
    clock: MonotonicClock,
    period: Duration,
) -> Result<FaultTrialReport, String> {
    let target_index = 0;
    let generation_seed = u64::try_from(case_index)
        .unwrap_or(u64::MAX)
        .saturating_mul(100)
        .saturating_add(1);
    let mut harness = start_harness(options.rack_count, generation_seed, worker, case, period)?;
    let request = fixed_request(options.frame_count);
    let mut timing = TimingHistograms::default();

    // Block zero dispatches the normal request. Block one proves a baseline completion and
    // dispatches sequence two, the exact worker fault trigger.
    let warmup_block = run_synthetic_block(
        &mut harness,
        0,
        request,
        clock,
        period,
        true,
        case,
        &mut timing,
    )?;
    let baseline_block = run_synthetic_block(
        &mut harness,
        1,
        request,
        clock,
        period,
        true,
        case,
        &mut timing,
    )?;
    let baseline_timing = timing.clone();
    let baseline_timing_thresholds = timing_thresholds(&baseline_timing, period);
    // The synthetic fault matrix proves rack-local behavior and recovery. It retains the
    // sleep-paced scheduler histogram below, but does not misclassify a worker wake outlier as
    // a failure of an otherwise independent rack or as attached-device timing evidence.
    let baseline_passed = baseline_block.accepted_racks == options.rack_count
        && harness
            .racks
            .iter()
            .all(|rack| matches!(rack.gate.state(), RackGateState::Awaiting { .. }));

    if baseline_block.kill_requested {
        // This is deliberately after the measured callback work and its pacing sleep.
        // Reaping remains deferred until the recovery control-plane step.
        harness.racks[target_index]
            .worker
            .as_mut()
            .expect("target rack has a worker")
            .stop_without_reaping()
            .map_err(|error| format!("could not stop target worker: {error}"))?;
    }

    let fault_block = run_synthetic_block(
        &mut harness,
        2,
        request,
        clock,
        period,
        true,
        case,
        &mut timing,
    )?;
    let post_fault_block = run_synthetic_block(
        &mut harness,
        3,
        request,
        clock,
        period,
        false,
        case,
        &mut timing,
    )?;

    let target_reason = closed_reason(harness.racks[target_index].gate.state());
    let stale_complete_slot_diagnosable = matches!(
        case,
        FaultCase::StaleSequenceCompletion | FaultCase::StaleGenerationCompletion
    )
    .then(|| stale_completion_still_diagnosable(&harness.racks[target_index], case));
    let hang_claim_observed = (case == FaultCase::HangAfterClaim)
        .then(|| wait_for_hang_claim(&harness.racks[target_index], period.saturating_mul(4)));
    let timely_accepted = !case.fatal()
        && fault_block.target_accepted
        && matches!(
            harness.racks[target_index].gate.state(),
            RackGateState::Open
        );
    let mut late_result_rejected = None;
    if case == FaultCase::LateCompletion && target_reason == Some(FallbackReason::DeadlineMiss) {
        let delay = case.worker_fault(period).fault_delay;
        // Control-plane observation only: no waiting or retrying occurs on a synthetic block.
        std::thread::sleep(delay.saturating_add(period));
        late_result_rejected = Some(observe_late_result_rejected(
            &mut harness.racks[target_index],
            4,
            clock,
            &mut timing,
        ));
    }

    let isolation_before_recovery = collect_isolation(&harness, target_index);
    let isolation_before_passed = isolation_before_recovery.iter().all(|entry| entry.passed);
    let normal_progress_passed = post_fault_block.unaffected_accepted == options.rack_count - 1;

    let mut counters = FaultCounters::default();
    counters.merge(&warmup_block.counters);
    counters.merge(&baseline_block.counters);
    counters.merge(&fault_block.counters);
    counters.merge(&post_fault_block.counters);

    let recovery = if case.fatal() {
        Some(recover_target(
            &mut harness,
            target_index,
            worker,
            request,
            clock,
            period,
            &mut timing,
            &mut counters,
        )?)
    } else {
        None
    };
    let isolation_after_recovery = collect_isolation(&harness, target_index);
    let isolation_after_passed = isolation_after_recovery.iter().all(|entry| entry.passed);

    let observed_result = match target_reason {
        Some(reason) => format!("fallback:{}", fallback_label(reason)),
        None if timely_accepted => "accepted".to_owned(),
        None => "no_expected_result".to_owned(),
    };
    let fault_passed = match case.expected_fallback() {
        Some(expected) => {
            target_reason == Some(expected)
                && stale_complete_slot_diagnosable.is_none_or(|diagnosable| diagnosable)
                && hang_claim_observed.is_none_or(|claimed| claimed)
        }
        None => timely_accepted,
    };
    let late_passed = late_result_rejected.is_none_or(|rejected| rejected);
    let recovery_passed = recovery.as_ref().is_none_or(|evidence| {
        evidence.bank_name_changed
            && evidence.ready_heartbeat_observed
            && evidence.recovery_blocks_driven == 3
            && evidence.unaffected_progress_continuous
            && evidence.accepted_completion
    });

    Ok(FaultTrialReport {
        case: case.id().to_owned(),
        injection_layer: case.injection_layer().to_owned(),
        expected_result: case.expected_result().to_owned(),
        observed_result,
        target_rack: target_index,
        baseline_passed,
        baseline_timing_thresholds,
        baseline_scheduler_characterization: SyntheticSchedulerCharacterization::from_timing(
            &baseline_timing,
        ),
        fault_passed,
        unaffected_rack_isolation_before_recovery: isolation_before_recovery,
        unaffected_rack_isolation_after_recovery: isolation_after_recovery,
        unaffected_progress_passed: normal_progress_passed,
        late_result_rejected,
        stale_complete_slot_diagnosable,
        hang_claim_observed,
        recovery,
        timing_histograms: timing,
        baseline_timing_histograms: baseline_timing,
        counters,
        passed: baseline_passed
            && fault_passed
            && isolation_before_passed
            && isolation_after_passed
            && normal_progress_passed
            && late_passed
            && recovery_passed,
    })
}

/// Starts a preconfigured worker set for an active-device run.
///
/// This function is control-plane-only. It creates every mapped bank and verifies the initial
/// heartbeat before the renderer takes ownership of the callback-side parts.
pub(crate) fn start_device_harness(
    rack_count: usize,
    generation_seed: u64,
    worker: &Path,
    configuration: &DeviceWorkerConfiguration,
) -> Result<Harness, String> {
    let mut racks = Vec::with_capacity(rack_count);
    for index in 0..rack_count {
        let generation = generation_seed.saturating_add(u64::try_from(index).unwrap_or(u64::MAX));
        let rack_configuration = configuration.for_rack(index);
        racks.push(start_rack_with_configuration(
            index,
            generation,
            worker,
            &rack_configuration,
        )?);
    }
    Ok(Harness { racks })
}

fn start_harness(
    rack_count: usize,
    generation_seed: u64,
    worker: &Path,
    case: FaultCase,
    period: Duration,
) -> Result<Harness, String> {
    let mut racks = Vec::with_capacity(rack_count);
    for index in 0..rack_count {
        let generation = generation_seed.saturating_add(u64::try_from(index).unwrap_or(u64::MAX));
        let fault = if index == 0 {
            case.worker_fault(period)
        } else {
            FaultCase::None.worker_fault(period)
        };
        racks.push(start_rack(index, generation, worker, fault)?);
    }
    Ok(Harness { racks })
}

fn start_rack(
    index: usize,
    generation: u64,
    worker: &Path,
    fault: FaultConfiguration,
) -> Result<RackHarness, String> {
    let configuration = DeviceWorkerConfiguration {
        fault,
        target_rack: Some(index),
        ..DeviceWorkerConfiguration::default()
    };
    start_rack_with_configuration(index, generation, worker, &configuration)
}

fn start_rack_with_configuration(
    index: usize,
    generation: u64,
    worker: &Path,
    configuration: &DeviceWorkerConfiguration,
) -> Result<RackHarness, String> {
    let region = SharedMemoryRegion::create(generation)
        .map_err(|error| format!("could not create bank for rack {index}: {error}"))?;
    let worker_id = u32::try_from(index + 1).map_err(|_| "rack worker ID overflows u32")?;
    let mut process =
        start_worker_with_configuration(worker, region.name(), worker_id, configuration)?;
    if region.bank().header.worker_heartbeat() == 0 {
        let _ = process.stop_and_reap();
        return Err(format!(
            "worker {worker_id} readiness arrived without a nonzero heartbeat"
        ));
    }
    let identity = RackIdentity {
        rack_index: index,
        process_id: process.id(),
        generation,
    };
    Ok(RackHarness {
        index,
        generation,
        region,
        worker: Some(process),
        gate: RackGate::new(),
        live: None,
        original_identity: identity,
    })
}

pub(crate) struct Harness {
    racks: Vec<RackHarness>,
}

/// Stable identities captured before a device callback takes ownership of the mapped-bank path.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct DeviceWorkerIdentity {
    pub(crate) worker_id: u32,
    pub(crate) process_id: u32,
    pub(crate) generation: u64,
    pub(crate) bank_identity: String,
}

pub(crate) struct RackHarness {
    index: usize,
    generation: u64,
    region: SharedMemoryRegion,
    worker: Option<WorkerProcess>,
    gate: RackGate,
    live: Option<LiveRequest>,
    original_identity: RackIdentity,
}

/// Callback-owned shared-memory and gate state for a device feasibility run.
///
/// This deliberately contains no child process handles. A control-plane
/// [`DeviceWorkerMonitor`] owns the handles and publishes worker loss through the per-rack
/// atomics, so the AUHAL renderer performs no process lifecycle operation.
pub(crate) struct DeviceHarness {
    racks: Vec<DeviceRackHarness>,
}

#[cfg(test)]
impl DeviceHarness {
    pub(crate) fn empty_for_test() -> Self {
        Self { racks: Vec::new() }
    }
}

struct DeviceRackHarness {
    region: SharedMemoryRegion,
    gate: RackGate,
    live: Option<LiveRequest>,
    worker_exited: Arc<AtomicBool>,
}

/// Control-plane owner for workers attached to a [`DeviceHarness`].
pub(crate) struct DeviceWorkerMonitor {
    workers: Vec<MonitoredWorkerProcess>,
}

struct MonitoredWorkerProcess {
    process: WorkerProcess,
    exited: Arc<AtomicBool>,
    heartbeat_reader: SharedMemoryRegion,
    heartbeat: WorkerHeartbeatProgress,
}

/// Control-thread heartbeat evidence from a dedicated read mapping for one worker bank.
///
/// The callback retains its own mapping and never synchronizes with this reader. The two views
/// communicate solely through the protocol's release/acquire heartbeat atomic.
#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) struct WorkerHeartbeatSnapshot {
    pub(crate) worker_id: u32,
    pub(crate) initial_tick: u64,
    pub(crate) last_tick: u64,
    pub(crate) control_polls: u64,
    pub(crate) advances: u64,
    pub(crate) regressions: u64,
    /// Cumulative requested calibrated-spin time from this worker's mapped protocol header.
    pub(crate) busy_requested_ticks: u64,
    /// Cumulative observed calibrated-spin time from this worker's mapped protocol header.
    pub(crate) busy_observed_ticks: u64,
    /// Number of calibrated-spin operations included in the two duration totals.
    pub(crate) busy_operations: u64,
}

impl WorkerHeartbeatSnapshot {
    #[must_use]
    pub(crate) const fn healthy(self) -> bool {
        self.initial_tick != 0
            && self.last_tick > self.initial_tick
            && self.advances != 0
            && self.regressions == 0
    }
}

#[derive(Clone, Copy, Debug)]
struct WorkerHeartbeatProgress {
    worker_id: u32,
    initial_tick: u64,
    last_tick: u64,
    control_polls: u64,
    advances: u64,
    regressions: u64,
}

impl WorkerHeartbeatProgress {
    fn new(worker_id: u32, initial_tick: u64) -> Self {
        Self {
            worker_id,
            initial_tick,
            last_tick: initial_tick,
            control_polls: 0,
            advances: 0,
            regressions: 0,
        }
    }

    fn observe(&mut self, tick: u64) {
        self.control_polls = self.control_polls.saturating_add(1);
        if tick > self.last_tick {
            self.advances = self.advances.saturating_add(1);
        } else if tick < self.last_tick {
            self.regressions = self.regressions.saturating_add(1);
        }
        self.last_tick = self.last_tick.max(tick);
    }

    fn snapshot(self, header: &sp_shared_memory::ProtocolHeader) -> WorkerHeartbeatSnapshot {
        let (busy_requested_ticks, busy_observed_ticks, busy_operations) =
            header.worker_busy_ticks();
        WorkerHeartbeatSnapshot {
            worker_id: self.worker_id,
            initial_tick: self.initial_tick,
            last_tick: self.last_tick,
            control_polls: self.control_polls,
            advances: self.advances,
            regressions: self.regressions,
            busy_requested_ticks,
            busy_observed_ticks,
            busy_operations,
        }
    }
}

impl Harness {
    /// Captures the worker process, generation, and mapped-bank identity for a report.
    #[must_use]
    pub(crate) fn device_worker_identities(&self) -> Vec<DeviceWorkerIdentity> {
        self.racks
            .iter()
            .map(|rack| DeviceWorkerIdentity {
                worker_id: u32::try_from(rack.index + 1).unwrap_or(u32::MAX),
                process_id: rack
                    .worker
                    .as_ref()
                    .expect("device harness has a worker per rack")
                    .id(),
                generation: rack.generation,
                bank_identity: format!("{}@{}", rack.region.name(), rack.region.generation()),
            })
            .collect()
    }

    /// Separates callback data from process lifecycle handles before the device starts.
    ///
    /// The control monitor opens a second mapping per bank so it can safely observe heartbeats
    /// without borrowing the callback-owned mapping or its mutable payload storage.
    pub(crate) fn into_device_parts(self) -> Result<(DeviceHarness, DeviceWorkerMonitor), String> {
        let mut callback_racks = Vec::with_capacity(self.racks.len());
        let mut workers = Vec::with_capacity(self.racks.len());
        for rack in self.racks {
            let RackHarness {
                index,
                region,
                worker,
                gate,
                live,
                ..
            } = rack;
            let worker_id = u32::try_from(index + 1).map_err(|_| "rack worker ID overflows u32")?;
            let heartbeat_reader = SharedMemoryRegion::open(region.name()).map_err(|error| {
                format!("could not open control heartbeat mapping for worker {worker_id}: {error}")
            })?;
            let initial_heartbeat = heartbeat_reader.bank().header.worker_heartbeat();
            if initial_heartbeat == 0 {
                return Err(format!(
                    "worker {worker_id} control heartbeat mapping has no startup heartbeat"
                ));
            }
            let exited = Arc::new(AtomicBool::new(false));
            callback_racks.push(DeviceRackHarness {
                region,
                gate,
                live,
                worker_exited: Arc::clone(&exited),
            });
            workers.push(MonitoredWorkerProcess {
                process: worker.expect("new device harness must retain a worker per rack"),
                exited,
                heartbeat_reader,
                heartbeat: WorkerHeartbeatProgress::new(worker_id, initial_heartbeat),
            });
        }
        Ok((
            DeviceHarness {
                racks: callback_racks,
            },
            DeviceWorkerMonitor { workers },
        ))
    }
}

impl DeviceWorkerMonitor {
    /// Polls worker liveness on the control plane and publishes fixed-size exit flags.
    ///
    /// # Errors
    ///
    /// Returns an error if the operating system cannot report a worker status.
    pub(crate) fn poll(&mut self) -> Result<(), String> {
        for worker in &mut self.workers {
            worker
                .heartbeat
                .observe(worker.heartbeat_reader.bank().header.worker_heartbeat());
            if !worker.exited.load(Ordering::Acquire) && worker.process.has_exited()? {
                worker.exited.store(true, Ordering::Release);
            }
        }
        Ok(())
    }

    /// Returns one safe control-thread heartbeat snapshot for every mapped worker bank.
    #[must_use]
    pub(crate) fn heartbeat_snapshots(&self) -> Vec<WorkerHeartbeatSnapshot> {
        self.workers
            .iter()
            .map(|worker| {
                worker
                    .heartbeat
                    .snapshot(&worker.heartbeat_reader.bank().header)
            })
            .collect()
    }

    /// Snapshots control-plane exit observations before teardown reaps healthy workers.
    #[must_use]
    pub(crate) fn observed_exit_flags(&self) -> Vec<bool> {
        self.workers
            .iter()
            .map(|worker| worker.exited.load(Ordering::Acquire))
            .collect()
    }

    /// Stops and reaps all workers after the device callback has retired.
    ///
    /// # Errors
    ///
    /// Returns the first cleanup failure after every worker has been given a cleanup attempt.
    pub(crate) fn stop_and_reap(&mut self) -> Result<(), String> {
        let mut first_error = None;
        for worker in &mut self.workers {
            if !worker.exited.load(Ordering::Acquire) {
                match worker.process.stop_and_reap() {
                    Ok(_) => worker.exited.store(true, Ordering::Release),
                    Err(error) if first_error.is_none() => first_error = Some(error),
                    Err(_) => {}
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

#[derive(Clone, Copy)]
pub(crate) struct LiveRequest {
    ticket: BlockTicket,
    slot_index: usize,
}

struct WorkerProcess {
    child: Child,
    control: Option<ChildStdin>,
}

impl WorkerProcess {
    fn id(&self) -> u32 {
        self.child.id()
    }

    fn has_exited(&mut self) -> Result<bool, String> {
        self.child
            .try_wait()
            .map(|status| status.is_some())
            .map_err(|error| format!("could not poll worker process: {error}"))
    }

    fn stop_without_reaping(&mut self) -> Result<(), String> {
        match self.child.kill() {
            Ok(()) => Ok(()),
            Err(kill_error) => match self.child.try_wait() {
                Ok(Some(_)) => Ok(()),
                Ok(None) => Err(format!("could not signal worker process: {kill_error}")),
                Err(poll_error) => Err(format!(
                    "could not signal worker process: {kill_error}; status poll also failed: {poll_error}"
                )),
            },
        }
    }

    fn stop_and_reap(&mut self) -> Result<ExitStatus, String> {
        let shutdown_error = self.request_graceful_shutdown().err();
        if let Some(status) = wait_for_child_exit(
            &mut self.child,
            WORKER_GRACEFUL_SHUTDOWN_TIMEOUT,
            WORKER_EXIT_POLL_INTERVAL,
        )? {
            return Ok(status);
        }

        match self.child.kill() {
            Ok(()) => self.child.wait().map_err(|error| {
                format_cleanup_error(
                    "could not reap worker after kill",
                    &error,
                    shutdown_error.as_ref(),
                )
            }),
            Err(kill_error) => match self.child.try_wait() {
                Ok(Some(status)) => Ok(status),
                Ok(None) => Err(format_cleanup_error(
                    "worker remained alive after kill failed",
                    &kill_error,
                    shutdown_error.as_ref(),
                )),
                Err(poll_error) => Err(format!(
                    "could not kill worker: {kill_error}; status poll also failed: {poll_error}{}",
                    shutdown_error_suffix(shutdown_error.as_ref())
                )),
            },
        }
    }

    fn request_graceful_shutdown(&mut self) -> std::io::Result<()> {
        let Some(mut control) = self.control.take() else {
            return Ok(());
        };
        let result = control.write_all(b"shutdown\n");
        drop(control);
        result
    }
}

impl Drop for WorkerProcess {
    fn drop(&mut self) {
        let _ = self.stop_and_reap();
    }
}

fn wait_for_child_exit(
    child: &mut Child,
    timeout: Duration,
    poll_interval: Duration,
) -> Result<Option<ExitStatus>, String> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(Some(status)),
            Ok(None) if Instant::now() < deadline => thread::sleep(poll_interval),
            Ok(None) => return Ok(None),
            Err(error) => return Err(format!("could not poll worker process: {error}")),
        }
    }
}

fn format_cleanup_error(
    context: &str,
    error: &std::io::Error,
    shutdown_error: Option<&std::io::Error>,
) -> String {
    format!(
        "{context}: {error}{}",
        shutdown_error_suffix(shutdown_error)
    )
}

fn shutdown_error_suffix(error: Option<&std::io::Error>) -> String {
    error.map_or_else(String::new, |error| {
        format!("; graceful shutdown write also failed: {error}")
    })
}

fn start_worker_with_configuration(
    executable: &Path,
    bank_name: &str,
    worker_id: u32,
    configuration: &DeviceWorkerConfiguration,
) -> Result<WorkerProcess, String> {
    let mut arguments = vec![
        "--feasibility-bank".to_owned(),
        bank_name.to_owned(),
        "--worker-id".to_owned(),
        worker_id.to_string(),
        "--fault-mode".to_owned(),
        if configuration.self_crash_after_claim {
            SELF_CRASH_AFTER_CLAIM_MODE.to_owned()
        } else {
            configuration.fault.mode.as_str().to_owned()
        },
        "--fault-trigger-sequence".to_owned(),
        configuration.fault.trigger_request_sequence.to_string(),
        "--fault-delay-micros".to_owned(),
        configuration.fault.fault_delay.as_micros().to_string(),
        "--work-duration-micros".to_owned(),
        configuration.fault.work_duration.as_micros().to_string(),
        "--compute-load-mode".to_owned(),
        configuration.compute_load.mode.as_str().to_owned(),
        "--compute-load-micros".to_owned(),
        configuration.compute_load.duration.as_micros().to_string(),
    ];
    if let Some(bundle) = &configuration.bundle {
        arguments.push("--bundle".to_owned());
        arguments.push(bundle.display().to_string());
    }
    let mut child = Command::new(executable)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not launch worker {worker_id}: {error}"))?;
    let control = child.stdin.take();
    let Some(stdout) = child.stdout.take() else {
        return Err(failed_startup_message(
            child,
            control,
            worker_id,
            "worker did not provide a readiness pipe",
        ));
    };
    let reader = spawn_readiness_reader(stdout);
    if let Err(error) = wait_for_readiness(&reader, WORKER_READINESS_TIMEOUT) {
        return Err(failed_startup_message(
            child,
            control,
            worker_id,
            &format!("readiness failed: {error}"),
        ));
    }
    match child.try_wait() {
        Ok(Some(status)) => Err(format!(
            "worker {worker_id} exited immediately after readiness with {status}"
        )),
        Ok(None) => Ok(WorkerProcess { child, control }),
        Err(error) => Err(failed_startup_message(
            child,
            control,
            worker_id,
            &format!("could not poll process after readiness: {error}"),
        )),
    }
}

fn failed_startup_message(
    mut child: Child,
    control: Option<ChildStdin>,
    worker_id: u32,
    failure: &str,
) -> String {
    match child.try_wait() {
        Ok(Some(status)) => format!(
            "worker {worker_id} startup infrastructure failure: {failure}; process already exited with {status}"
        ),
        Ok(None) => {
            let mut process = WorkerProcess { child, control };
            match process.stop_and_reap() {
                Ok(status) => format!(
                    "worker {worker_id} startup infrastructure failure: {failure}; cleanup reaped process with {status}"
                ),
                Err(cleanup_error) => format!(
                    "worker {worker_id} startup infrastructure failure: {failure}; cleanup failed: {cleanup_error}"
                ),
            }
        }
        Err(poll_error) => {
            let mut process = WorkerProcess { child, control };
            match process.stop_and_reap() {
                Ok(status) => format!(
                    "worker {worker_id} startup infrastructure failure: {failure}; initial status poll failed: {poll_error}; cleanup reaped process with {status}"
                ),
                Err(cleanup_error) => format!(
                    "worker {worker_id} startup infrastructure failure: {failure}; initial status poll failed: {poll_error}; cleanup failed: {cleanup_error}"
                ),
            }
        }
    }
}

fn spawn_readiness_reader(
    stdout: impl std::io::Read + Send + 'static,
) -> Receiver<std::io::Result<String>> {
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut line = String::new();
        let result = BufReader::new(stdout).read_line(&mut line).map(|_| line);
        let _ = sender.send(result);
    });
    receiver
}

fn wait_for_readiness(
    receiver: &Receiver<std::io::Result<String>>,
    timeout: Duration,
) -> Result<(), String> {
    match receiver.recv_timeout(timeout) {
        Ok(Ok(line)) if line.trim() == WORKER_READY => Ok(()),
        Ok(Ok(line)) if line.is_empty() => {
            Err("worker readiness pipe closed before `ready`".to_owned())
        }
        Ok(Ok(line)) => Err(format!("unexpected readiness line `{}`", line.trim())),
        Ok(Err(error)) => Err(format!("could not read readiness line: {error}")),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(format!(
            "timed out after {} ms waiting for `ready`",
            timeout.as_millis()
        )),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err("worker readiness reader disconnected".to_owned())
        }
    }
}

struct BlockResult {
    accepted_racks: usize,
    unaffected_accepted: usize,
    target_accepted: bool,
    first_fault: Option<FallbackReason>,
    kill_requested: bool,
    counters: FaultCounters,
}

/// Runs one lock-free, fixed-layout dispatch/observe cycle for the device callback.
///
/// All open racks are published first. The callback then performs nonblocking completion sweeps
/// until every rack completes or the absolute sub-period deadline is reached. Process liveness
/// arrives only through atomics maintained by [`DeviceWorkerMonitor`]. This path performs no child
/// polling, formatting, sleeping, allocation, or lifecycle work.
#[allow(
    clippy::too_many_lines,
    reason = "the callback's dispatch/observe/expire sequence reads clearest in order"
)]
pub(crate) fn process_device_callback_block(
    harness: &mut DeviceHarness,
    block_index: u64,
    request: BlockRequest,
    clock: MonotonicClock,
    completion_budget_ticks: u64,
    timing: &mut TimingHistograms,
) -> DeviceBlockCounters {
    let callback_started = clock.now_ticks();
    let completion_deadline = callback_started.saturating_add(completion_budget_ticks);
    let mut counters = DeviceBlockCounters::default();

    for (rack_index, rack) in harness.racks.iter_mut().enumerate() {
        if !matches!(rack.gate.state(), RackGateState::Open) {
            continue;
        }
        let slot_index = usize::try_from(block_index).unwrap_or(usize::MAX) % BLOCK_SLOT_COUNT;
        match rack
            .region
            .bank_mut()
            .request_block_at(slot_index, request, clock.now_ticks())
        {
            Ok(ticket)
                if rack.gate.dispatch(ticket, block_index) == GateOutcome::DispatchAllowed =>
            {
                rack.live = Some(LiveRequest { ticket, slot_index });
            }
            Ok(_) | Err(_) => {
                counters.protocol_faults = counters.protocol_faults.saturating_add(1);
                if let Some(total) = counters.protocol_faults_by_rack.get_mut(rack_index) {
                    *total = total.saturating_add(1);
                }
                if let GateOutcome::UseFallback(reason) = rack.gate.observe(
                    WorkerObservation::ProtocolFault(ProtocolError::InvalidState),
                    block_index,
                ) {
                    abandon_device_request(rack);
                    record_device_fallback(&mut counters, reason, rack_index, block_index);
                }
            }
        }
    }

    loop {
        let mut awaiting = false;
        let mut made_progress = false;
        for (rack_index, rack) in harness.racks.iter_mut().enumerate() {
            if !matches!(rack.gate.state(), RackGateState::Awaiting { .. }) {
                continue;
            }
            awaiting = true;
            let observation = match rack.live {
                Some(live) => observe_device_once(rack, live, clock, timing),
                None => WorkerObservation::ProtocolFault(ProtocolError::InvalidState),
            };
            match observation {
                WorkerObservation::ProtocolFault(_) => {
                    counters.protocol_faults = counters.protocol_faults.saturating_add(1);
                    if let Some(total) = counters.protocol_faults_by_rack.get_mut(rack_index) {
                        *total = total.saturating_add(1);
                    }
                }
                WorkerObservation::WorkerExited => {
                    counters.worker_exits = counters.worker_exits.saturating_add(1);
                    if let Some(total) = counters.worker_exits_by_rack.get_mut(rack_index) {
                        *total = total.saturating_add(1);
                    }
                }
                WorkerObservation::Pending | WorkerObservation::Completed(_) => {}
            }
            match rack.gate.observe(observation, block_index) {
                GateOutcome::WorkerResultAccepted => {
                    rack.live = None;
                    made_progress = true;
                    counters.accepted_racks = counters.accepted_racks.saturating_add(1);
                    if let Some(accepted) = counters.accepted_by_rack.get_mut(rack_index) {
                        *accepted = accepted.saturating_add(1);
                    }
                }
                GateOutcome::Awaiting => {}
                GateOutcome::UseFallback(reason) => {
                    abandon_device_request(rack);
                    made_progress = true;
                    record_device_fallback(&mut counters, reason, rack_index, block_index);
                }
                GateOutcome::DispatchAllowed => {
                    counters.protocol_faults = counters.protocol_faults.saturating_add(1);
                }
            }
        }

        if !awaiting {
            break;
        }
        if clock.now_ticks() >= completion_deadline {
            for (rack_index, rack) in harness.racks.iter_mut().enumerate() {
                if matches!(rack.gate.state(), RackGateState::Awaiting { .. }) {
                    abandon_device_request(rack);
                    if let GateOutcome::UseFallback(reason) =
                        rack.gate.deadline_expired_hard(block_index)
                    {
                        record_device_fallback(&mut counters, reason, rack_index, block_index);
                    }
                }
            }
            break;
        }
        if !made_progress {
            std::hint::spin_loop();
        }
    }

    timing.observe_callback_work(
        clock.ticks_to_duration(clock.now_ticks().saturating_sub(callback_started)),
    );
    counters
}

fn abandon_device_request(rack: &mut DeviceRackHarness) {
    let Some(live) = rack.live.take() else {
        return;
    };
    let Some(slot) = rack.region.bank().slot(live.slot_index) else {
        return;
    };
    if slot.metadata.state() == Ok(sp_shared_memory::SlotState::Requested) {
        let _ = slot.abandon_request(live.ticket);
    }
}

fn observe_device_once(
    rack: &mut DeviceRackHarness,
    live: LiveRequest,
    clock: MonotonicClock,
    timing: &mut TimingHistograms,
) -> WorkerObservation {
    if rack.worker_exited.load(Ordering::Acquire) {
        return WorkerObservation::WorkerExited;
    }
    let Some(slot) = rack.region.bank().slot(live.slot_index) else {
        return WorkerObservation::ProtocolFault(ProtocolError::InvalidState);
    };
    match slot.consume_completion_timing(live.ticket) {
        Ok(block_timing) => {
            let completion_observed_tick = clock.now_ticks();
            timing.observe_timing(block_timing, completion_observed_tick, clock);
            WorkerObservation::Completed(live.ticket)
        }
        Err(ProtocolError::UnexpectedState | ProtocolError::Owned) => WorkerObservation::Pending,
        Err(error) => WorkerObservation::ProtocolFault(error),
    }
}

fn record_device_fallback(
    counters: &mut DeviceBlockCounters,
    reason: FallbackReason,
    rack_index: usize,
    block_index: u64,
) {
    counters.fallback_events = counters.fallback_events.saturating_add(1);
    if let Some(recorded) = counters.first_fallback_block_plus_one.get_mut(rack_index)
        && *recorded == 0
    {
        *recorded = block_index.saturating_add(1);
    }
    if reason == FallbackReason::DeadlineMiss {
        counters.deadline_misses = counters.deadline_misses.saturating_add(1);
        if let Some(total) = counters.deadline_misses_by_rack.get_mut(rack_index) {
            *total = total.saturating_add(1);
        }
    }
}

/// Aggregate counters from a device-callback block.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct DeviceBlockCounters {
    pub(crate) accepted_racks: usize,
    /// Per-rack accepted completions for device fault-isolation evidence.
    pub(crate) accepted_by_rack: [u64; MAX_RACKS],
    pub(crate) deadline_misses: u64,
    /// Per-rack deadline misses; only a deliberately faulted target may miss in an attached
    /// containment run.
    pub(crate) deadline_misses_by_rack: [u64; MAX_RACKS],
    pub(crate) fallback_events: u64,
    /// First callback block that selected fallback per rack, encoded as `block + 1` so zero
    /// remains the no-fallback sentinel.
    pub(crate) first_fallback_block_plus_one: [u64; MAX_RACKS],
    pub(crate) protocol_faults: u64,
    /// Per-rack malformed/stale protocol observations.
    pub(crate) protocol_faults_by_rack: [u64; MAX_RACKS],
    pub(crate) worker_exits: u64,
    /// Per-rack control-plane worker-loss observations.
    pub(crate) worker_exits_by_rack: [u64; MAX_RACKS],
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn run_synthetic_block(
    harness: &mut Harness,
    block_index: u64,
    request: BlockRequest,
    clock: MonotonicClock,
    period: Duration,
    dispatch: bool,
    case: FaultCase,
    timing: &mut TimingHistograms,
) -> Result<BlockResult, String> {
    let callback_started = Instant::now();
    let mut grace_waited = Duration::ZERO;
    let mut result = BlockResult {
        accepted_racks: 0,
        unaffected_accepted: 0,
        target_accepted: false,
        first_fault: None,
        kill_requested: false,
        counters: FaultCounters::default(),
    };

    for rack in &mut harness.racks {
        if matches!(rack.gate.state(), RackGateState::Awaiting { .. }) {
            let live = rack.live.expect("awaiting gate has a live request");
            let mut observation = observe_once_for_case(rack, live, clock, timing, case)?;
            // Cooperative synthetic pacing: absorb short OS descheduling before treating Pending
            // as a deadline miss. Do not grace-wait intentional late/hang injections on the
            // target trigger sequence — those cases must observe an immediate miss. Grace time
            // is excluded from callback-work histograms so baseline thresholds stay honest.
            if matches!(observation, WorkerObservation::Pending)
                && !period.is_zero()
                && !suppresses_pending_grace(case, rack.index, live.ticket.sequence)
            {
                let grace = if case == FaultCase::DelayBeforeClaimTimely
                    && rack.index == 0
                    && live.ticket.sequence == FAULT_TRIGGER_SEQUENCE
                {
                    period.saturating_mul(4)
                } else {
                    period
                };
                let grace_started = Instant::now();
                observation = wait_for_pending_completion(rack, live, clock, timing, case, grace)?;
                grace_waited = grace_waited.saturating_add(grace_started.elapsed());
            }
            match observation {
                WorkerObservation::ProtocolFault(_) => {
                    result.counters.protocol_faults =
                        result.counters.protocol_faults.saturating_add(1);
                }
                WorkerObservation::WorkerExited => {
                    result.counters.worker_exits = result.counters.worker_exits.saturating_add(1);
                }
                WorkerObservation::Pending | WorkerObservation::Completed(_) => {}
            }
            let outcome = rack.gate.observe(observation, block_index);
            match outcome {
                GateOutcome::WorkerResultAccepted => {
                    rack.live = None;
                    result.accepted_racks += 1;
                    if rack.index == 0 {
                        result.target_accepted = true;
                    } else {
                        result.unaffected_accepted += 1;
                    }
                }
                GateOutcome::Awaiting => {
                    if let GateOutcome::UseFallback(reason) =
                        rack.gate.deadline_expired_hard(block_index)
                    {
                        record_fallback(&mut result.counters, reason);
                        result.first_fault.get_or_insert(reason);
                    }
                }
                GateOutcome::UseFallback(reason) => {
                    record_fallback(&mut result.counters, reason);
                    result.first_fault.get_or_insert(reason);
                }
                GateOutcome::DispatchAllowed => {
                    return Err(
                        "rack gate accepted dispatch while applying an observation".to_owned()
                    );
                }
            }
        }

        if dispatch && matches!(rack.gate.state(), RackGateState::Open) {
            let slot_index = usize::try_from(block_index).unwrap_or(usize::MAX) % BLOCK_SLOT_COUNT;
            let ticket = rack
                .region
                .bank_mut()
                .request_block_at(slot_index, request, clock.now_ticks())
                .map_err(|error| {
                    format!(
                        "block {block_index}, rack {}: could not publish timed request: {error}",
                        rack.index
                    )
                })?;
            if rack.gate.dispatch(ticket, block_index) != GateOutcome::DispatchAllowed {
                return Err(format!(
                    "block {block_index}, rack {}: rack gate rejected a fresh request",
                    rack.index
                ));
            }
            rack.live = Some(LiveRequest { ticket, slot_index });
            if case == FaultCase::Kill
                && rack.index == 0
                && ticket.sequence == FAULT_TRIGGER_SEQUENCE
            {
                result.kill_requested = true;
            }
        }
    }

    let callback_work = callback_started
        .elapsed()
        .checked_sub(grace_waited)
        .unwrap_or(Duration::ZERO);
    timing.observe_callback_work(callback_work);
    let paced = callback_work.saturating_add(grace_waited);
    if let Some(remaining) = period.checked_sub(paced) {
        std::thread::sleep(remaining);
    }
    Ok(result)
}

fn record_fallback(counters: &mut FaultCounters, reason: FallbackReason) {
    counters.fallback_events = counters.fallback_events.saturating_add(1);
    if reason == FallbackReason::DeadlineMiss {
        counters.deadline_misses = counters.deadline_misses.saturating_add(1);
    }
}

fn hang_claim_still_observable(rack: &RackHarness) -> bool {
    let Some(live) = rack.live else {
        return false;
    };
    if live.ticket.sequence != FAULT_TRIGGER_SEQUENCE {
        return false;
    }
    rack.region
        .bank()
        .slot(live.slot_index)
        .is_some_and(|slot| {
            matches!(slot.metadata.state(), Ok(SlotState::Processing))
                && slot
                    .metadata
                    .worker_claimed_tick
                    .load(std::sync::atomic::Ordering::Acquire)
                    != 0
        })
}

fn wait_for_hang_claim(rack: &RackHarness, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if hang_claim_still_observable(rack) {
            return true;
        }
        if Instant::now() >= deadline {
            return hang_claim_still_observable(rack);
        }
        thread::sleep(WORKER_EXIT_POLL_INTERVAL);
    }
}

fn suppresses_pending_grace(case: FaultCase, rack_index: usize, sequence: u64) -> bool {
    rack_index == 0
        && sequence == FAULT_TRIGGER_SEQUENCE
        && matches!(case, FaultCase::LateCompletion | FaultCase::HangAfterClaim)
}

fn wait_for_pending_completion(
    rack: &mut RackHarness,
    live: LiveRequest,
    clock: MonotonicClock,
    timing: &mut TimingHistograms,
    case: FaultCase,
    grace: Duration,
) -> Result<WorkerObservation, String> {
    let deadline = Instant::now() + grace;
    loop {
        let observation = observe_once_for_case(rack, live, clock, timing, case)?;
        if !matches!(observation, WorkerObservation::Pending) || Instant::now() >= deadline {
            return Ok(observation);
        }
        thread::sleep(WORKER_EXIT_POLL_INTERVAL);
    }
}

fn observe_once_for_case(
    rack: &mut RackHarness,
    live: LiveRequest,
    clock: MonotonicClock,
    timing: &mut TimingHistograms,
    case: FaultCase,
) -> Result<WorkerObservation, String> {
    if case != FaultCase::StaleGenerationCompletion
        || rack.index != 0
        || live.ticket.sequence != FAULT_TRIGGER_SEQUENCE
    {
        return observe_once(rack, live, clock, timing);
    }
    if rack
        .worker
        .as_mut()
        .expect("live rack has a worker")
        .has_exited()?
    {
        return Ok(WorkerObservation::WorkerExited);
    }
    let slot = rack
        .region
        .bank()
        .slot(live.slot_index)
        .expect("fixed slot index is in range");
    match slot.completion_snapshot() {
        Ok(Some(snapshot)) => {
            timing.observe_timing(snapshot.timing, clock.now_ticks(), clock);
            Ok(WorkerObservation::Completed(mismatched_generation_ticket(
                live.ticket,
            )))
        }
        Ok(None) | Err(ProtocolError::Owned) => Ok(WorkerObservation::Pending),
        Err(error) => Ok(WorkerObservation::ProtocolFault(error)),
    }
}

fn mismatched_generation_ticket(ticket: BlockTicket) -> BlockTicket {
    BlockTicket {
        generation: if ticket.generation == u64::MAX {
            1
        } else {
            ticket.generation.saturating_add(1)
        },
        sequence: ticket.sequence,
    }
}

fn observe_once(
    rack: &mut RackHarness,
    live: LiveRequest,
    clock: MonotonicClock,
    timing: &mut TimingHistograms,
) -> Result<WorkerObservation, String> {
    if rack
        .worker
        .as_mut()
        .expect("live rack has a worker")
        .has_exited()?
    {
        return Ok(WorkerObservation::WorkerExited);
    }
    let slot = rack
        .region
        .bank()
        .slot(live.slot_index)
        .expect("fixed slot index is in range");
    match slot.consume_completion_timing(live.ticket) {
        Ok(block_timing) => {
            timing.observe_timing(block_timing, clock.now_ticks(), clock);
            Ok(WorkerObservation::Completed(live.ticket))
        }
        Err(ProtocolError::UnexpectedState | ProtocolError::Owned) => {
            Ok(WorkerObservation::Pending)
        }
        Err(error) => Ok(WorkerObservation::ProtocolFault(error)),
    }
}

fn stale_completion_still_diagnosable(rack: &RackHarness, case: FaultCase) -> bool {
    let Some(live) = rack.live else {
        return false;
    };
    rack.region
        .bank()
        .slot(live.slot_index)
        .is_some_and(|slot| match slot.completion_snapshot() {
            Ok(Some(snapshot)) if case == FaultCase::StaleSequenceCompletion => {
                snapshot.ticket.generation == live.ticket.generation
                    && snapshot.ticket.sequence != live.ticket.sequence
            }
            Ok(Some(snapshot)) if case == FaultCase::StaleGenerationCompletion => {
                snapshot.ticket == live.ticket
            }
            _ => false,
        })
}

fn observe_late_result_rejected(
    rack: &mut RackHarness,
    block_index: u64,
    clock: MonotonicClock,
    timing: &mut TimingHistograms,
) -> bool {
    let Some(live) = rack.live else {
        return false;
    };
    let Some(slot) = rack.region.bank().slot(live.slot_index) else {
        return false;
    };
    match slot.consume_completion_timing(live.ticket) {
        Ok(block_timing) => {
            timing.observe_timing(block_timing, clock.now_ticks(), clock);
            matches!(
                rack.gate
                    .observe(WorkerObservation::Completed(live.ticket), block_index),
                GateOutcome::UseFallback(FallbackReason::DeadlineMiss)
            )
        }
        Err(_) => false,
    }
}

#[allow(clippy::too_many_arguments)]
fn recover_target(
    harness: &mut Harness,
    target_index: usize,
    worker: &Path,
    request: BlockRequest,
    clock: MonotonicClock,
    period: Duration,
    timing: &mut TimingHistograms,
    counters: &mut FaultCounters,
) -> Result<RecoveryEvidence, String> {
    let (before, old_name, next_generation, rack_index) = {
        let target = &mut harness.racks[target_index];
        let before = target.original_identity.clone();
        let old_name = target.region.name().to_owned();
        // This is control-plane work after the synthetic callback path: only the target is
        // stopped and reaped, then its bank and process are replaced.
        let mut old_worker = target
            .worker
            .take()
            .expect("target rack has a worker before replacement");
        old_worker.stop_and_reap()?;
        (
            before,
            old_name,
            target.generation.saturating_add(1_000),
            target.index,
        )
    };
    // Keep all unaffected racks on their normal bounded block cadence while the target has no
    // worker. The closed target gate prevents any access to its intentionally removed process.
    let during_replacement = run_synthetic_block(
        harness,
        10,
        request,
        clock,
        period,
        true,
        FaultCase::None,
        timing,
    )?;
    counters.merge(&during_replacement.counters);

    let replacement = start_rack(
        rack_index,
        next_generation,
        worker,
        FaultCase::None.worker_fault(period),
    )?;
    let new_name = replacement.region.name().to_owned();
    let after = RackIdentity {
        rack_index,
        process_id: replacement
            .worker
            .as_ref()
            .expect("fresh replacement rack has a worker")
            .id(),
        generation: next_generation,
    };
    harness.racks[target_index] = replacement;
    harness.racks[target_index].gate.reset_after_replacement();
    let ready_heartbeat_observed = harness.racks[target_index]
        .region
        .bank()
        .header
        .worker_heartbeat()
        != 0;

    let first_after_replacement = run_synthetic_block(
        harness,
        11,
        request,
        clock,
        period,
        true,
        FaultCase::None,
        timing,
    )?;
    counters.merge(&first_after_replacement.counters);
    let second_after_replacement = run_synthetic_block(
        harness,
        12,
        request,
        clock,
        period,
        false,
        FaultCase::None,
        timing,
    )?;
    counters.merge(&second_after_replacement.counters);
    let unaffected_rack_count =
        u64::try_from(harness.racks.len().saturating_sub(1)).unwrap_or(u64::MAX);
    let unaffected_accepted_during_replacement = u64::try_from(
        during_replacement
            .unaffected_accepted
            .saturating_add(first_after_replacement.unaffected_accepted)
            .saturating_add(second_after_replacement.unaffected_accepted),
    )
    .unwrap_or(u64::MAX);
    Ok(RecoveryEvidence {
        old_target: before,
        replacement: after,
        bank_name_changed: old_name != new_name,
        ready_heartbeat_observed,
        recovery_blocks_driven: 3,
        unaffected_accepted_during_replacement,
        unaffected_progress_continuous: during_replacement.unaffected_accepted == 0
            && first_after_replacement.unaffected_accepted == harness.racks.len() - 1
            && second_after_replacement.unaffected_accepted == harness.racks.len() - 1
            && unaffected_accepted_during_replacement == unaffected_rack_count.saturating_mul(2),
        accepted_completion: first_after_replacement.target_accepted
            || second_after_replacement.target_accepted,
    })
}

fn collect_isolation(harness: &Harness, target_index: usize) -> Vec<IsolationEvidence> {
    harness
        .racks
        .iter()
        .filter(|rack| rack.index != target_index)
        .map(|rack| {
            let final_identity = RackIdentity {
                rack_index: rack.index,
                process_id: rack
                    .worker
                    .as_ref()
                    .expect("unaffected rack has a worker")
                    .id(),
                generation: rack.generation,
            };
            let no_fault = matches!(rack.gate.state(), RackGateState::Open);
            let passed = no_fault && rack.original_identity == final_identity;
            IsolationEvidence {
                original: rack.original_identity.clone(),
                final_identity,
                no_fallback_protocol_or_deadline_fault: no_fault,
                passed,
            }
        })
        .collect()
}

fn closed_reason(state: RackGateState) -> Option<FallbackReason> {
    match state {
        RackGateState::Closed { reason, .. } => Some(reason),
        RackGateState::Open | RackGateState::Awaiting { .. } => None,
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct RackIdentity {
    rack_index: usize,
    process_id: u32,
    generation: u64,
}

#[derive(Clone, Debug, Serialize)]
struct IsolationEvidence {
    original: RackIdentity,
    final_identity: RackIdentity,
    no_fallback_protocol_or_deadline_fault: bool,
    passed: bool,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Serialize)]
struct RecoveryEvidence {
    old_target: RackIdentity,
    replacement: RackIdentity,
    bank_name_changed: bool,
    ready_heartbeat_observed: bool,
    recovery_blocks_driven: u64,
    unaffected_accepted_during_replacement: u64,
    unaffected_progress_continuous: bool,
    accepted_completion: bool,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Serialize)]
struct FaultTrialReport {
    case: String,
    injection_layer: String,
    expected_result: String,
    observed_result: String,
    target_rack: usize,
    baseline_passed: bool,
    /// Retained synthetic scheduler characterization; not an attached-device hard-gate verdict.
    baseline_timing_thresholds: TimingThresholds,
    baseline_scheduler_characterization: SyntheticSchedulerCharacterization,
    fault_passed: bool,
    unaffected_rack_isolation_before_recovery: Vec<IsolationEvidence>,
    unaffected_rack_isolation_after_recovery: Vec<IsolationEvidence>,
    unaffected_progress_passed: bool,
    late_result_rejected: Option<bool>,
    stale_complete_slot_diagnosable: Option<bool>,
    hang_claim_observed: Option<bool>,
    recovery: Option<RecoveryEvidence>,
    timing_histograms: TimingHistograms,
    baseline_timing_histograms: TimingHistograms,
    counters: FaultCounters,
    passed: bool,
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

#[derive(Clone, Debug, Serialize)]
struct QualificationStatus {
    status: &'static str,
    qualifying: bool,
    official_duration_seconds: u64,
    required_matrix_cells: Vec<String>,
    detail: String,
}

#[derive(Clone, Debug, Serialize)]
struct TimingThresholds {
    dispatch_p9999_below_micros: u64,
    dispatch_max_below_micros: u64,
    callback_p999_below_period_fraction: f64,
    callback_p9999_below_period_fraction: f64,
    dispatch_p9999_micros: u64,
    dispatch_max_micros: u64,
    callback_p999_micros: u64,
    callback_p9999_micros: u64,
    callback_max_micros: u64,
    passed: bool,
}

/// Synthetic scheduler evidence, deliberately separate from attached-device hard-gate results.
/// The full fixed histograms remain in the artifact so every outlier is retained by its exact
/// microsecond bucket and overflow count rather than hidden behind a percentile.
#[derive(Clone, Debug, Serialize)]
struct SyntheticSchedulerCharacterization {
    scope: &'static str,
    attached_device_thresholds_enforced: bool,
    wake_outlier_threshold_micros: u64,
    request_to_claim_outlier_samples: u64,
    request_to_completion_outlier_samples: u64,
    request_to_claim_max_micros: u64,
    request_to_completion_max_micros: u64,
}

impl SyntheticSchedulerCharacterization {
    fn from_timing(timing: &TimingHistograms) -> Self {
        const WAKE_OUTLIER_THRESHOLD_MICROS: u64 = 150;
        Self {
            scope: "sleep_paced_synthetic_scheduler_characterization",
            attached_device_thresholds_enforced: false,
            wake_outlier_threshold_micros: WAKE_OUTLIER_THRESHOLD_MICROS,
            request_to_claim_outlier_samples: timing
                .request_to_claim
                .samples_at_or_above(WAKE_OUTLIER_THRESHOLD_MICROS),
            request_to_completion_outlier_samples: timing
                .request_to_completion
                .samples_at_or_above(WAKE_OUTLIER_THRESHOLD_MICROS),
            request_to_claim_max_micros: duration_to_micros_ceil(
                timing.request_to_claim.max_duration(),
            ),
            request_to_completion_max_micros: duration_to_micros_ceil(
                timing.request_to_completion.max_duration(),
            ),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
struct FaultCounters {
    deadline_misses: u64,
    fallback_events: u64,
    protocol_faults: u64,
    worker_exits: u64,
}

impl FaultCounters {
    fn merge(&mut self, other: &Self) {
        self.deadline_misses = self.deadline_misses.saturating_add(other.deadline_misses);
        self.fallback_events = self.fallback_events.saturating_add(other.fallback_events);
        self.protocol_faults = self.protocol_faults.saturating_add(other.protocol_faults);
        self.worker_exits = self.worker_exits.saturating_add(other.worker_exits);
    }
}

#[derive(Clone, Debug, Serialize)]
struct PreflightLabels {
    scope: &'static str,
    future_scope: &'static str,
    coreaudio_callback_attached: bool,
    phase1_hard_gate_certified: bool,
}

#[derive(Clone, Debug, Serialize)]
struct MatrixConfiguration {
    rack_count: usize,
    frame_count: u32,
    sample_rate_hz: u32,
    block_period_micros: u64,
}

#[derive(Clone, Debug, Serialize)]
struct BaselineReport {
    expected_result: &'static str,
    timing_histograms: Option<TimingHistograms>,
    passed: bool,
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
        children_before: Option<ProcessResourceUsage>,
    ) -> Self {
        Self {
            status: "pending",
            scope: "xtask_host_and_reaped_children",
            host_before: host_before.map(Into::into),
            host_after: None,
            reaped_children_before: children_before.map(Into::into),
            reaped_children_after: None,
        }
    }

    fn finish(
        &mut self,
        host_after: Option<ProcessResourceUsage>,
        children_after: Option<ProcessResourceUsage>,
    ) {
        self.host_after = host_after.map(Into::into);
        self.reaped_children_after = children_after.map(Into::into);
        self.status = if self.host_before.is_some()
            && self.host_after.is_some()
            && self.reaped_children_before.is_some()
            && self.reaped_children_after.is_some()
        {
            "collected"
        } else {
            "unavailable"
        };
    }
}

#[derive(Clone, Debug, Serialize)]
struct EnergyEvidence {
    status: &'static str,
    required: bool,
    imported: bool,
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
struct IpcConfiguration {
    rack_count: usize,
    frame_count: u32,
    duration_seconds: u64,
    sample_rate_hz: u32,
    block_period_micros: u64,
}

#[derive(Clone, Debug, Serialize)]
struct IpcReport {
    report_version: u32,
    report_id: String,
    labels: PreflightLabels,
    environment: EnvironmentEvidence,
    qualification: QualificationStatus,
    configuration: IpcConfiguration,
    timing_histograms: TimingHistograms,
    timing_thresholds: TimingThresholds,
    scheduler_characterization: SyntheticSchedulerCharacterization,
    counters: FaultCounters,
    cpu_evidence: CpuEvidence,
    energy_evidence: EnergyEvidence,
    acceptance_passed: bool,
    evidence_complete: bool,
    acceptance_failure: Option<String>,
    infrastructure_error: Option<String>,
    limitations: Vec<&'static str>,
}

#[derive(Clone, Debug, Serialize)]
struct IpcMatrixCell {
    rack_count: usize,
    frame_count: u32,
    directory: String,
    report_id: String,
    acceptance_passed: bool,
    evidence_complete: bool,
    infrastructure_error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct IpcMatrixReport {
    report_version: u32,
    report_id: String,
    labels: PreflightLabels,
    environment: EnvironmentEvidence,
    qualification: QualificationStatus,
    duration_seconds: u64,
    cells: Vec<IpcMatrixCell>,
    acceptance_passed: bool,
    evidence_complete: bool,
    infrastructure_errors: Vec<String>,
    limitations: Vec<&'static str>,
}

#[derive(Clone, Debug, Serialize)]
struct FaultMatrixReport {
    report_version: u32,
    report_id: String,
    labels: PreflightLabels,
    environment: EnvironmentEvidence,
    qualification: QualificationStatus,
    configuration: MatrixConfiguration,
    baseline: BaselineReport,
    trials: Vec<FaultTrialReport>,
    cpu_evidence: CpuEvidence,
    energy_evidence: EnergyEvidence,
    acceptance_passed: bool,
    evidence_complete: bool,
    infrastructure_error: Option<String>,
    limitations: Vec<&'static str>,
}

impl FaultMatrixReport {
    fn new(
        options: &FaultMatrixOptions,
        period: Duration,
        energy_evidence: EnergyEvidence,
        environment: EnvironmentEvidence,
        cpu_before: Option<ProcessResourceUsage>,
        children_before: Option<ProcessResourceUsage>,
    ) -> Self {
        Self {
            report_version: REPORT_VERSION,
            report_id: String::new(),
            labels: PreflightLabels {
                scope: "synthetic_preflight",
                future_scope: "active_device_callback",
                coreaudio_callback_attached: false,
                phase1_hard_gate_certified: false,
            },
            environment,
            qualification: QualificationStatus {
                status: "synthetic_behavior_evidence_only",
                qualifying: false,
                official_duration_seconds: 1_800,
                required_matrix_cells: required_ipc_matrix_cells(),
                detail: "Fault injection validates synthetic rack isolation and recovery only; it does not qualify active-device timing.".to_owned(),
            },
            configuration: MatrixConfiguration {
                rack_count: options.rack_count,
                frame_count: options.frame_count,
                sample_rate_hz: SAMPLE_RATE_HZ,
                block_period_micros: duration_to_micros_ceil(period),
            },
            baseline: BaselineReport {
                expected_result: "all racks accept one normal timed completion before injection",
                timing_histograms: None,
                passed: false,
            },
            trials: Vec::with_capacity(FaultCase::ALL.len()),
            cpu_evidence: CpuEvidence::new(cpu_before, children_before),
            energy_evidence,
            acceptance_passed: false,
            evidence_complete: false,
            infrastructure_error: None,
            limitations: phase1_limitations(),
        }
    }
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
        json_sha256: sha256_hex(json),
        markdown_sha256: sha256_hex(markdown),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

fn assign_ipc_report_id(report: &mut IpcReport) -> Result<(), Phase1Error> {
    report.report_id.clear();
    let model = serde_json::to_vec(report).map_err(|error| {
        Phase1Error::Infrastructure(format!("could not fingerprint IPC report: {error}"))
    })?;
    report.report_id = format!("{:016x}", fnv1a64(&model));
    Ok(())
}

fn assign_ipc_matrix_report_id(report: &mut IpcMatrixReport) -> Result<(), Phase1Error> {
    report.report_id.clear();
    let model = serde_json::to_vec(report).map_err(|error| {
        Phase1Error::Infrastructure(format!("could not fingerprint IPC matrix report: {error}"))
    })?;
    report.report_id = format!("{:016x}", fnv1a64(&model));
    Ok(())
}

fn assign_fault_report_id(report: &mut FaultMatrixReport) -> Result<(), Phase1Error> {
    report.report_id.clear();
    let model = serde_json::to_vec(report).map_err(|error| {
        Phase1Error::Infrastructure(format!("could not fingerprint fault report: {error}"))
    })?;
    report.report_id = format!("{:016x}", fnv1a64(&model));
    Ok(())
}

fn write_ipc_report(output_directory: &Path, report: &IpcReport) -> Result<(), Phase1Error> {
    let json = serde_json::to_vec_pretty(report).map_err(|error| {
        Phase1Error::Infrastructure(format!("could not serialize ipc-feasibility.json: {error}"))
    })?;
    let markdown = ipc_report_markdown(report).into_bytes();
    let manifest = serde_json::to_vec_pretty(&artifact_manifest(
        report.report_version,
        &report.report_id,
        "ipc-feasibility.json",
        "ipc-feasibility.md",
        &json,
        &markdown,
    ))
    .map_err(|error| {
        Phase1Error::Infrastructure(format!("could not serialize IPC manifest: {error}"))
    })?;
    publish_artifact_set(
        output_directory,
        "ipc-feasibility",
        &report.report_id,
        &json,
        &markdown,
        &manifest,
    )
}

fn write_ipc_matrix_report(
    output_directory: &Path,
    report: &IpcMatrixReport,
) -> Result<(), Phase1Error> {
    let json = serde_json::to_vec_pretty(report).map_err(|error| {
        Phase1Error::Infrastructure(format!("could not serialize ipc-matrix.json: {error}"))
    })?;
    let markdown = ipc_matrix_markdown(report).into_bytes();
    let manifest = serde_json::to_vec_pretty(&artifact_manifest(
        report.report_version,
        &report.report_id,
        "ipc-matrix.json",
        "ipc-matrix.md",
        &json,
        &markdown,
    ))
    .map_err(|error| {
        Phase1Error::Infrastructure(format!("could not serialize IPC matrix manifest: {error}"))
    })?;
    publish_artifact_set(
        output_directory,
        "ipc-matrix",
        &report.report_id,
        &json,
        &markdown,
        &manifest,
    )
}

fn write_report(output_directory: &Path, report: &FaultMatrixReport) -> Result<(), Phase1Error> {
    let json = serde_json::to_vec_pretty(report).map_err(|error| {
        Phase1Error::Infrastructure(format!("could not serialize fault-matrix.json: {error}"))
    })?;
    let markdown = report_markdown(report).into_bytes();
    let manifest = serde_json::to_vec_pretty(&artifact_manifest(
        report.report_version,
        &report.report_id,
        "fault-matrix.json",
        "fault-matrix.md",
        &json,
        &markdown,
    ))
    .map_err(|error| {
        Phase1Error::Infrastructure(format!(
            "could not serialize fault-matrix manifest: {error}"
        ))
    })?;
    publish_artifact_set(
        output_directory,
        "fault-matrix",
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
    let publish_tick = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let nonce = format!("{}.{}.{publish_tick}", std::process::id(), report_id);
    let json_temp = output_directory.join(format!(".{stem}.{nonce}.json.tmp"));
    let markdown_temp = output_directory.join(format!(".{stem}.{nonce}.md.tmp"));
    let manifest_temp = output_directory.join(format!(".{stem}.{nonce}.manifest.tmp"));
    let final_manifest = output_directory.join(format!("{stem}.manifest.json"));
    match fs::remove_file(&final_manifest) {
        Ok(()) => std::fs::File::open(output_directory)
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
        // The manifest is the commit marker and is always published last. Readers can
        // reject any mixed pair whose embedded report IDs do not match this manifest.
        fs::rename(&manifest_temp, &final_manifest)?;
        std::fs::File::open(output_directory)?.sync_all()?;
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

fn publish_raw_phase1_file(
    output_directory: &Path,
    file_name: &str,
    contents: &[u8],
) -> Result<(), Phase1Error> {
    fs::create_dir_all(output_directory).map_err(|error| {
        Phase1Error::Infrastructure(format!(
            "could not create raw Phase 1 artifact directory {}: {error}",
            output_directory.display()
        ))
    })?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let temporary = output_directory.join(format!(".{file_name}.{nonce}.tmp"));
    let final_path = output_directory.join(file_name);
    write_synced_file(&temporary, contents).map_err(|error| {
        Phase1Error::Infrastructure(format!(
            "could not write raw Phase 1 artifact {}: {error}",
            temporary.display()
        ))
    })?;
    if let Err(error) = fs::rename(&temporary, &final_path) {
        let _ = fs::remove_file(&temporary);
        return Err(Phase1Error::Infrastructure(format!(
            "could not publish raw Phase 1 artifact {}: {error}",
            final_path.display()
        )));
    }
    fs::File::open(output_directory)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| {
            Phase1Error::Infrastructure(format!(
                "could not sync raw Phase 1 artifact directory {}: {error}",
                output_directory.display()
            ))
        })
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

fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

#[allow(clippy::too_many_lines)]
fn ipc_report_markdown(report: &IpcReport) -> String {
    let mut markdown = String::new();
    writeln!(markdown, "# SYNTHETIC_IPC_PREFLIGHT").expect("String writes cannot fail");
    writeln!(markdown, "report_id: {}", report.report_id).expect("String writes cannot fail");
    writeln!(markdown, "scope: {}", report.labels.scope).expect("String writes cannot fail");
    writeln!(markdown, "future scope: {}", report.labels.future_scope)
        .expect("String writes cannot fail");
    writeln!(markdown, "coreaudio_callback_attached=false").expect("String writes cannot fail");
    writeln!(markdown, "phase1_hard_gate_certified=false").expect("String writes cannot fail");
    writeln!(markdown, "## Qualification").expect("String writes cannot fail");
    writeln!(
        markdown,
        "- status: {}; qualifying: {}; official minimum: {} seconds per cell; {}",
        report.qualification.status,
        report.qualification.qualifying,
        report.qualification.official_duration_seconds,
        report.qualification.detail,
    )
    .expect("String writes cannot fail");
    writeln!(markdown, "## Environment").expect("String writes cannot fail");
    writeln!(
        markdown,
        "- OS: {} {}; arch: {}; hardware: {}; memory: {}; Rust: {}; revision: {}; source: {}",
        report.environment.operating_system,
        report.environment.os_version,
        report.environment.architecture,
        report.environment.hardware_model,
        report.environment.hardware_memory_bytes,
        report.environment.rust_version,
        report.environment.source_revision,
        report.environment.source_state,
    )
    .expect("String writes cannot fail");
    writeln!(
        markdown,
        "## Configuration and synthetic scheduler characterization"
    )
    .expect("String writes cannot fail");
    writeln!(
        markdown,
        "- racks: {}; frames: {}; duration: {} s; sample rate: {} Hz; period: {} us",
        report.configuration.rack_count,
        report.configuration.frame_count,
        report.configuration.duration_seconds,
        report.configuration.sample_rate_hz,
        report.configuration.block_period_micros,
    )
    .expect("String writes cannot fail");
    writeln!(
        markdown,
        "- attached-device thresholds applied: {}; synthetic comparison only={}; dispatch p99.99={} us, max={} us; callback p99.9={} us, p99.99={} us, max={} us",
        report
            .scheduler_characterization
            .attached_device_thresholds_enforced,
        report.timing_thresholds.passed,
        report.timing_thresholds.dispatch_p9999_micros,
        report.timing_thresholds.dispatch_max_micros,
        report.timing_thresholds.callback_p999_micros,
        report.timing_thresholds.callback_p9999_micros,
        report.timing_thresholds.callback_max_micros,
    )
    .expect("String writes cannot fail");
    markdown.push_str(&histogram_markdown(
        "timing histograms (every sample retained by its microsecond bucket)",
        &report.timing_histograms,
    ));
    writeln!(
        markdown,
        "- synthetic wake outliers at/above {} us: request→claim={}; request→completion={}; maxima={} / {} us; these are not attached-device evidence",
        report.scheduler_characterization.wake_outlier_threshold_micros,
        report
            .scheduler_characterization
            .request_to_claim_outlier_samples,
        report
            .scheduler_characterization
            .request_to_completion_outlier_samples,
        report.scheduler_characterization.request_to_claim_max_micros,
        report
            .scheduler_characterization
            .request_to_completion_max_micros,
    )
    .expect("String writes cannot fail");
    writeln!(
        markdown,
        "- counters deadline/fallback/protocol/worker-exit: {}/{}/{}/{}",
        report.counters.deadline_misses,
        report.counters.fallback_events,
        report.counters.protocol_faults,
        report.counters.worker_exits,
    )
    .expect("String writes cannot fail");
    writeln!(markdown, "## CPU and energy evidence").expect("String writes cannot fail");
    writeln!(
        markdown,
        "- CPU: {} ({})",
        report.cpu_evidence.status, report.cpu_evidence.scope
    )
    .expect("String writes cannot fail");
    writeln!(
        markdown,
        "- energy: {}; required: {}; imported: {}",
        report.energy_evidence.status,
        report.energy_evidence.required,
        report.energy_evidence.imported,
    )
    .expect("String writes cannot fail");
    for error in &report.energy_evidence.validation_errors {
        writeln!(markdown, "  - validation: {error}").expect("String writes cannot fail");
    }
    writeln!(markdown, "## Result").expect("String writes cannot fail");
    writeln!(
        markdown,
        "- acceptance passed: {}",
        report.acceptance_passed
    )
    .expect("String writes cannot fail");
    writeln!(
        markdown,
        "- evidence complete: {}",
        report.evidence_complete
    )
    .expect("String writes cannot fail");
    if let Some(error) = &report.acceptance_failure {
        writeln!(markdown, "- acceptance failure: {error}").expect("String writes cannot fail");
    }
    if let Some(error) = &report.infrastructure_error {
        writeln!(markdown, "- infrastructure error: {error}").expect("String writes cannot fail");
    }
    markdown
}

fn ipc_matrix_markdown(report: &IpcMatrixReport) -> String {
    let mut markdown = String::new();
    writeln!(markdown, "# Synthetic IPC matrix").expect("String writes cannot fail");
    writeln!(markdown, "report_id: {}", report.report_id).expect("String writes cannot fail");
    writeln!(markdown, "scope: synthetic_preflight").expect("String writes cannot fail");
    writeln!(markdown, "future scope: active_device_callback").expect("String writes cannot fail");
    writeln!(markdown, "coreaudio_callback_attached=false").expect("String writes cannot fail");
    writeln!(markdown, "phase1_hard_gate_certified=false").expect("String writes cannot fail");
    writeln!(
        markdown,
        "qualification: {} (qualifying={}); duration={} seconds per cell",
        report.qualification.status, report.qualification.qualifying, report.duration_seconds,
    )
    .expect("String writes cannot fail");
    writeln!(markdown, "| racks | frames | report | acceptance |")
        .expect("String writes cannot fail");
    writeln!(markdown, "| --- | --- | --- | --- |").expect("String writes cannot fail");
    for cell in &report.cells {
        writeln!(
            markdown,
            "| {} | {} | {}/ipc-feasibility.json ({}) | {} |",
            cell.rack_count,
            cell.frame_count,
            cell.directory,
            cell.report_id,
            cell.acceptance_passed,
        )
        .expect("String writes cannot fail");
    }
    markdown
}

fn print_ipc_summary(report: &IpcReport, output_directory: &Path) {
    println!("SYNTHETIC_IPC_PREFLIGHT");
    println!(
        "  {} rack(s), {} frames, {} second(s), {} Hz",
        report.configuration.rack_count,
        report.configuration.frame_count,
        report.configuration.duration_seconds,
        report.configuration.sample_rate_hz,
    );
    println!(
        "  wake p99.99 {}; processing p99.99 {}; total p99.99 {}, max {}; callback p99.9 {}, p99.99 {}, max {}; synthetic wake outliers={} (attached thresholds not applied)",
        format_duration(
            report
                .timing_histograms
                .request_to_claim
                .percentile_upper_bound(9_999, 10_000)
        ),
        format_duration(
            report
                .timing_histograms
                .claim_to_completion
                .percentile_upper_bound(9_999, 10_000)
        ),
        format_duration(
            report
                .timing_histograms
                .request_to_completion
                .percentile_upper_bound(9_999, 10_000)
        ),
        format_duration(
            report
                .timing_histograms
                .request_to_completion
                .max_duration()
        ),
        format_duration(
            report
                .timing_histograms
                .synthetic_callback_work
                .percentile_upper_bound(999, 1_000)
        ),
        format_duration(
            report
                .timing_histograms
                .synthetic_callback_work
                .percentile_upper_bound(9_999, 10_000)
        ),
        format_duration(
            report
                .timing_histograms
                .synthetic_callback_work
                .max_duration()
        ),
        report
            .scheduler_characterization
            .request_to_claim_outlier_samples
            .saturating_add(
                report
                    .scheduler_characterization
                    .request_to_completion_outlier_samples,
            ),
    );
    println!(
        "  qualification={} (full matrix and >=1800 seconds/cell required); artifacts={}",
        report.qualification.status,
        output_directory.display(),
    );
}

#[allow(clippy::too_many_lines)]
fn report_markdown(report: &FaultMatrixReport) -> String {
    let mut markdown = String::new();
    writeln!(markdown, "# Phase 1 synthetic fault matrix").expect("writing to String cannot fail");
    writeln!(markdown).expect("writing to String cannot fail");
    writeln!(markdown, "Report model version: {}", report.report_version)
        .expect("writing to String cannot fail");
    writeln!(markdown, "report_id: {}", report.report_id).expect("writing to String cannot fail");
    writeln!(markdown, "scope: {}", report.labels.scope).expect("writing to String cannot fail");
    writeln!(markdown, "future scope: {}", report.labels.future_scope)
        .expect("writing to String cannot fail");
    writeln!(
        markdown,
        "coreaudio_callback_attached={}",
        report.labels.coreaudio_callback_attached
    )
    .expect("writing to String cannot fail");
    writeln!(
        markdown,
        "phase1_hard_gate_certified={}",
        report.labels.phase1_hard_gate_certified
    )
    .expect("writing to String cannot fail");
    writeln!(markdown).expect("writing to String cannot fail");
    writeln!(markdown, "## Environment").expect("writing to String cannot fail");
    writeln!(
        markdown,
        "- OS: {} {}; architecture: {}; hardware: {}; memory bytes: {}; Rust: {}; revision: {}; source state: {}",
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
    writeln!(markdown, "## Qualification").expect("writing to String cannot fail");
    writeln!(
        markdown,
        "- status: {}; qualifying: {}; official duration: {} seconds; detail: {}",
        report.qualification.status,
        report.qualification.qualifying,
        report.qualification.official_duration_seconds,
        report.qualification.detail,
    )
    .expect("writing to String cannot fail");
    writeln!(markdown, "## Configuration").expect("writing to String cannot fail");
    writeln!(
        markdown,
        "- racks: {}; frames: {}; sample rate: {} Hz; period: {} us",
        report.configuration.rack_count,
        report.configuration.frame_count,
        report.configuration.sample_rate_hz,
        report.configuration.block_period_micros
    )
    .expect("writing to String cannot fail");
    writeln!(markdown, "## Baseline").expect("writing to String cannot fail");
    writeln!(markdown, "- expected: {}", report.baseline.expected_result)
        .expect("writing to String cannot fail");
    writeln!(markdown, "- passed: {}", report.baseline.passed)
        .expect("writing to String cannot fail");
    if let Some(histograms) = &report.baseline.timing_histograms {
        markdown.push_str(&histogram_markdown(
            "baseline timing histograms",
            histograms,
        ));
    }
    writeln!(markdown, "## Fault trials").expect("writing to String cannot fail");
    writeln!(
        markdown,
        "| case | injection layer | expected | observed | passed |"
    )
    .expect("writing to String cannot fail");
    writeln!(markdown, "| --- | --- | --- | --- | --- |").expect("writing to String cannot fail");
    for trial in &report.trials {
        writeln!(
            markdown,
            "| {} | {} | {} | {} | {} |",
            trial.case,
            trial.injection_layer,
            trial.expected_result,
            trial.observed_result,
            trial.passed
        )
        .expect("writing to String cannot fail");
        writeln!(
            markdown,
            "  - unaffected-rack isolation before recovery: {}; after recovery: {}; recovery: {}; late result rejected: {}; stale Complete retained for diagnosis: {}; hang claim observed: {}; baseline timing thresholds: {}; counters deadline/fallback/protocol/exit={}/{}/{}/{}",
            trial
                .unaffected_rack_isolation_before_recovery
                .iter()
                .all(|entry| entry.passed),
            trial
                .unaffected_rack_isolation_after_recovery
                .iter()
                .all(|entry| entry.passed),
            trial
                .recovery
                .as_ref()
                .is_none_or(|recovery| recovery.accepted_completion),
            trial
                .late_result_rejected
                .map_or("not applicable", |result| if result {
                    "true"
                } else {
                    "false"
                }),
            trial
                .stale_complete_slot_diagnosable
                .map_or("not applicable", |result| if result {
                    "true"
                } else {
                    "false"
                }),
            trial
                .hang_claim_observed
                .map_or("not applicable", |result| if result {
                    "true"
                } else {
                    "false"
                }),
            trial.baseline_timing_thresholds.passed,
            trial.counters.deadline_misses,
            trial.counters.fallback_events,
            trial.counters.protocol_faults,
            trial.counters.worker_exits,
        )
        .expect("writing to String cannot fail");
        writeln!(
            markdown,
            "  - baseline thresholds: dispatch p99.99={} us (<150), max={} us (<400); callback p99.9={} us (<70% period), p99.99={} us (<80% period), max={} us (<period)",
            trial.baseline_timing_thresholds.dispatch_p9999_micros,
            trial.baseline_timing_thresholds.dispatch_max_micros,
            trial.baseline_timing_thresholds.callback_p999_micros,
            trial.baseline_timing_thresholds.callback_p9999_micros,
            trial.baseline_timing_thresholds.callback_max_micros,
        )
        .expect("writing to String cannot fail");
        markdown.push_str(&histogram_markdown(
            "timing histograms",
            &trial.timing_histograms,
        ));
    }
    writeln!(markdown, "## CPU and energy evidence").expect("writing to String cannot fail");
    writeln!(
        markdown,
        "- CPU: {} ({})",
        report.cpu_evidence.status, report.cpu_evidence.scope
    )
    .expect("writing to String cannot fail");
    writeln!(
        markdown,
        "- energy: {}; required: {}; imported: {}",
        report.energy_evidence.status,
        report.energy_evidence.required,
        report.energy_evidence.imported
    )
    .expect("writing to String cannot fail");
    for error in &report.energy_evidence.validation_errors {
        writeln!(markdown, "  - energy validation: {error}")
            .expect("writing to String cannot fail");
    }
    writeln!(markdown, "## Limitations").expect("writing to String cannot fail");
    for limitation in &report.limitations {
        writeln!(markdown, "- {limitation}").expect("writing to String cannot fail");
    }
    writeln!(markdown, "## Overall result").expect("writing to String cannot fail");
    writeln!(
        markdown,
        "- acceptance passed: {}",
        report.acceptance_passed
    )
    .expect("writing to String cannot fail");
    writeln!(
        markdown,
        "- required evidence complete: {}",
        report.evidence_complete
    )
    .expect("writing to String cannot fail");
    if let Some(error) = &report.infrastructure_error {
        writeln!(markdown, "- infrastructure error: {error}")
            .expect("writing to String cannot fail");
    }
    markdown
}

fn histogram_markdown(name: &str, histograms: &TimingHistograms) -> String {
    let mut output = String::new();
    writeln!(output, "  - {name}:").expect("writing to String cannot fail");
    for (label, histogram) in [
        ("request_to_claim", &histograms.request_to_claim),
        ("claim_to_completion", &histograms.claim_to_completion),
        ("request_to_completion", &histograms.request_to_completion),
        (
            "completion_to_observation",
            &histograms.completion_to_observation,
        ),
        ("request_to_observation", &histograms.request_to_observation),
        (
            "synthetic_callback_work",
            &histograms.synthetic_callback_work,
        ),
    ] {
        writeln!(
            output,
            "    - {label}: samples={}, p99.9<={} us, max={} us, overflow={}",
            histogram.sample_count,
            duration_to_micros_ceil(histogram.percentile_upper_bound(999, 1_000)),
            histogram.max_micros,
            histogram.overflow_count
        )
        .expect("writing to String cannot fail");
    }
    output
}

fn load_energy_evidence(
    path: Option<&Path>,
    expected_workload: &str,
    expected_rack_count: usize,
    expected_frame_count: u32,
    expected_hardware: &str,
) -> Result<EnergyEvidence, Phase1Error> {
    let Some(path) = path else {
        return Ok(EnergyEvidence {
            status: "not_supplied",
            required: false,
            imported: false,
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
            "could not read structured energy evidence {}: {error}",
            path.display()
        ))
    })?;
    let value: Value = serde_json::from_slice(&contents).map_err(|error| {
        Phase1Error::InvalidConfiguration(format!(
            "structured energy evidence {} is not JSON: {error}",
            path.display()
        ))
    })?;
    let object = value.as_object().ok_or_else(|| {
        Phase1Error::InvalidConfiguration(
            "structured energy evidence must be a JSON object".to_owned(),
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
    if rack_count != u64::try_from(expected_rack_count).ok() {
        validation_errors.push(format!("rack_count must equal {expected_rack_count}"));
    }
    if frame_count != Some(u64::from(expected_frame_count)) {
        validation_errors.push(format!("frame_count must equal {expected_frame_count}"));
    }
    if energy_joules.is_none() && average_power_watts.is_none() {
        validation_errors.push(
            "at least one of energy_joules or average_power_watts must be positive".to_owned(),
        );
    }
    let imported = validation_errors.is_empty();
    Ok(EnergyEvidence {
        status: if imported { "imported" } else { "invalid" },
        required: false,
        imported,
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

fn parse_feasibility_options(arguments: &[String]) -> Result<FeasibilityOptions, String> {
    let mut racks = None;
    let mut frames = None;
    let mut duration_seconds = None;
    let mut output_directory = None;
    let mut energy_evidence_path = None;
    let mut require_energy_evidence = false;
    let mut seen = BTreeSet::new();
    let mut index = 0;
    while index < arguments.len() {
        let option = arguments[index].as_str();
        if !seen.insert(option) {
            return Err(format!(
                "duplicate option `{option}`\n\n{}",
                feasibility_usage()
            ));
        }
        if option == "--require-energy-evidence" {
            require_energy_evidence = true;
            index += 1;
            continue;
        }
        let value = arguments
            .get(index + 1)
            .ok_or_else(|| format!("missing value after `{option}`\n\n{}", feasibility_usage()))?;
        match option {
            "--racks" => racks = Some(parse_racks(value, true)?),
            "--frames" => frames = Some(parse_frames(value)?),
            "--duration-seconds" => {
                let parsed = value
                    .parse::<u64>()
                    .map_err(|_| "--duration-seconds must be a positive integer".to_owned())?;
                if parsed == 0 {
                    return Err("--duration-seconds must be a positive integer".to_owned());
                }
                duration_seconds = Some(parsed);
            }
            "--output-dir" => output_directory = Some(PathBuf::from(value)),
            "--energy-evidence" => energy_evidence_path = Some(PathBuf::from(value)),
            _ => {
                return Err(format!(
                    "unknown option `{option}`\n\n{}",
                    feasibility_usage()
                ));
            }
        }
        index += 2;
    }
    Ok(FeasibilityOptions {
        rack_count: racks.ok_or_else(|| "missing required --racks".to_owned())?,
        frame_count: frames.ok_or_else(|| "missing required --frames".to_owned())?,
        duration: Duration::from_secs(
            duration_seconds.ok_or_else(|| "missing required --duration-seconds".to_owned())?,
        ),
        output_directory,
        energy_evidence_path,
        require_energy_evidence,
    })
}

fn parse_ipc_matrix_options(arguments: &[String]) -> Result<IpcMatrixOptions, String> {
    let mut duration_seconds = None;
    let mut output_directory = None;
    let mut seen = BTreeSet::new();
    let mut index = 0;
    while index < arguments.len() {
        let option = arguments[index].as_str();
        if !seen.insert(option) {
            return Err(format!(
                "duplicate option `{option}`\n\n{}",
                ipc_matrix_usage()
            ));
        }
        let value = arguments
            .get(index + 1)
            .ok_or_else(|| format!("missing value after `{option}`\n\n{}", ipc_matrix_usage()))?;
        match option {
            "--duration-seconds" => {
                let parsed = value
                    .parse::<u64>()
                    .map_err(|_| "--duration-seconds must be a positive integer".to_owned())?;
                if parsed == 0 {
                    return Err("--duration-seconds must be a positive integer".to_owned());
                }
                duration_seconds = Some(parsed);
            }
            "--output-dir" => output_directory = Some(PathBuf::from(value)),
            _ => {
                return Err(format!(
                    "unknown option `{option}`\n\n{}",
                    ipc_matrix_usage()
                ));
            }
        }
        index += 2;
    }
    Ok(IpcMatrixOptions {
        duration: Duration::from_secs(
            duration_seconds.ok_or_else(|| "missing required --duration-seconds".to_owned())?,
        ),
        output_directory: output_directory
            .ok_or_else(|| "missing required --output-dir".to_owned())?,
    })
}

fn parse_fault_matrix_options(arguments: &[String]) -> Result<FaultMatrixOptions, String> {
    let mut racks = None;
    let mut frames = None;
    let mut output_directory = None;
    let mut energy_evidence_path = None;
    let mut require_energy_evidence = false;
    let mut seen = BTreeSet::new();
    let mut index = 0;
    while index < arguments.len() {
        let option = arguments[index].as_str();
        if !seen.insert(option) {
            return Err(format!(
                "duplicate option `{option}`\n\n{}",
                fault_matrix_usage()
            ));
        }
        if option == "--require-energy-evidence" {
            require_energy_evidence = true;
            index += 1;
            continue;
        }
        let value = arguments
            .get(index + 1)
            .ok_or_else(|| format!("missing value after `{option}`\n\n{}", fault_matrix_usage()))?;
        match option {
            "--racks" => racks = Some(parse_racks(value, false)?),
            "--frames" => frames = Some(parse_frames(value)?),
            "--output-dir" => output_directory = Some(PathBuf::from(value)),
            "--energy-evidence" => energy_evidence_path = Some(PathBuf::from(value)),
            _ => {
                return Err(format!(
                    "unknown option `{option}`\n\n{}",
                    fault_matrix_usage()
                ));
            }
        }
        index += 2;
    }
    let output_directory = output_directory.ok_or_else(|| fault_matrix_usage().to_owned())?;
    if output_directory.as_os_str().is_empty() {
        return Err("--output-dir must not be empty".to_owned());
    }
    Ok(FaultMatrixOptions {
        rack_count: racks.ok_or_else(|| fault_matrix_usage().to_owned())?,
        frame_count: frames.ok_or_else(|| fault_matrix_usage().to_owned())?,
        output_directory,
        energy_evidence_path,
        require_energy_evidence,
    })
}

fn parse_racks(value: &str, allow_one: bool) -> Result<usize, String> {
    let rack_count = value
        .parse::<usize>()
        .map_err(|_| "--racks must be one of 1, 2, 4, or 8".to_owned())?;
    let allowed = if allow_one {
        matches!(rack_count, 1 | 2 | 4 | 8)
    } else {
        matches!(rack_count, 2 | 4 | 8)
    };
    if allowed {
        Ok(rack_count)
    } else if allow_one {
        Err("--racks must be one of 1, 2, 4, or 8".to_owned())
    } else {
        Err("--racks must be one of 2, 4, or 8 for isolation evidence".to_owned())
    }
}

fn parse_frames(value: &str) -> Result<u32, String> {
    let frame_count = value
        .parse::<u32>()
        .map_err(|_| "--frames must be either 128 or 256".to_owned())?;
    if matches!(frame_count, 128 | 256) {
        Ok(frame_count)
    } else {
        Err("--frames must be either 128 or 256".to_owned())
    }
}

pub(crate) fn feasibility_usage() -> &'static str {
    "usage: cargo xtask ipc-feasibility --racks <1|2|4|8> --frames <128|256> --duration-seconds <seconds> [--output-dir <directory>] [--energy-evidence <structured-json>] [--require-energy-evidence]"
}

pub(crate) fn ipc_matrix_usage() -> &'static str {
    "usage: cargo xtask ipc-matrix --duration-seconds <seconds> --output-dir <directory>"
}

pub(crate) fn fault_matrix_usage() -> &'static str {
    "usage: cargo xtask fault-matrix --racks <2|4|8> --frames <128|256> --output-dir <directory> [--energy-evidence <structured-json>] [--require-energy-evidence]"
}

#[derive(Debug)]
struct Phase1ReportOptions {
    artifact_directory: PathBuf,
}

#[derive(Serialize)]
struct Phase1ConsolidatedReport {
    report_version: u32,
    report_id: String,
    status: &'static str,
    certified: bool,
    artifact_directory: String,
    required_cells: Vec<String>,
    discovered_artifacts: usize,
    unavailable_reasons: Vec<String>,
}

/// Validates independently produced Phase 1 hardware evidence and publishes its commit marker last.
///
/// The accepted schemas are deliberately small and explicit. A report from an unknown schema is
/// evidence-unavailable, never an opportunity to infer certification from similarly named fields.
#[allow(clippy::too_many_lines)]
pub(crate) fn run_phase1_report(
    workspace_root: &Path,
    arguments: &[String],
) -> Result<CommandOutcome, Phase1Error> {
    let options = parse_phase1_report_options(workspace_root, arguments)
        .map_err(Phase1Error::InvalidConfiguration)?;
    let mut reasons = Vec::new();
    let mut discovered_artifacts = 0_usize;

    let synthetic_manifest = options
        .artifact_directory
        .join("ipc-matrix/ipc-matrix.manifest.json");
    if let Some(artifact) =
        read_expected_phase1_artifact(&synthetic_manifest, "synthetic_matrix", &mut reasons)
    {
        discovered_artifacts = discovered_artifacts.saturating_add(1);
        validate_synthetic_matrix(&artifact.value, &mut reasons);
    }

    for cell in required_ipc_matrix_cells() {
        let manifest = options
            .artifact_directory
            .join("device-matrix")
            .join(&cell)
            .join("device-feasibility.manifest.json");
        let Some(artifact) =
            read_expected_phase1_artifact(&manifest, "active_device_cell", &mut reasons)
        else {
            continue;
        };
        discovered_artifacts = discovered_artifacts.saturating_add(1);
        let reasons_before = reasons.len();
        let observed_cell = validate_active_device_cell(&artifact.value, &mut reasons);
        if observed_cell.as_deref() != Some(cell.as_str()) {
            reasons.push(format!(
                "{cell} manifest contains a mismatched active-device cell {}",
                observed_cell
                    .as_deref()
                    .unwrap_or("with invalid configuration")
            ));
        }
        if reasons.len() == reasons_before && observed_cell.is_none() {
            reasons.push(format!(
                "{cell} is missing a valid active-device cell identity"
            ));
        }
    }

    for (name, relative_path, expected_kind) in [
        (
            "calibrated_load:8r-128f",
            "calibrated-load/8r-128f/device-feasibility.manifest.json",
            "calibrated_load",
        ),
        (
            "calibrated_load:8r-256f",
            "calibrated-load/8r-256f/device-feasibility.manifest.json",
            "calibrated_load",
        ),
        (
            "fault_isolation:self-crash:128f",
            "fault-isolation/self-crash-2r-128f/device-feasibility.manifest.json",
            "fault_isolation",
        ),
        (
            "fault_isolation:hang-after-claim:128f",
            "fault-isolation/hang-2r-128f/device-feasibility.manifest.json",
            "fault_isolation",
        ),
        (
            "fault_isolation:self-crash:256f",
            "fault-isolation/self-crash-2r-256f/device-feasibility.manifest.json",
            "fault_isolation",
        ),
        (
            "fault_isolation:hang-after-claim:256f",
            "fault-isolation/hang-2r-256f/device-feasibility.manifest.json",
            "fault_isolation",
        ),
        (
            "real_vst3_smoke",
            "vst3-smoke/real-vst3-smoke.manifest.json",
            "real_vst3_smoke",
        ),
    ] {
        let manifest = options.artifact_directory.join(relative_path);
        let Some(artifact) = read_expected_phase1_artifact(&manifest, expected_kind, &mut reasons)
        else {
            continue;
        };
        discovered_artifacts = discovered_artifacts.saturating_add(1);
        let reasons_before = reasons.len();
        let passed = validate_supporting_artifact(&artifact.value, expected_kind, &mut reasons);
        if expected_kind == "real_vst3_smoke" && !valid_vst3_raw_source(&artifact) {
            reasons.push(
                "real_vst3_smoke raw host-checker report is missing or has a digest mismatch"
                    .to_owned(),
            );
        }
        let observed = supporting_artifact_identity_from_value(&artifact.value, expected_kind);
        if observed.as_deref() != Some(name) {
            reasons.push(format!(
                "{name} artifact configuration does not match its required path"
            ));
        }
        if !passed && reasons.len() == reasons_before {
            reasons.push(format!("{name} artifact is incomplete or not successful"));
        }
    }

    reasons.sort();
    reasons.dedup();
    let certified = reasons.is_empty();
    let mut report = Phase1ConsolidatedReport {
        report_version: REPORT_VERSION,
        report_id: String::new(),
        status: if certified {
            "certified"
        } else {
            "unavailable"
        },
        certified,
        artifact_directory: options.artifact_directory.display().to_string(),
        required_cells: required_ipc_matrix_cells(),
        discovered_artifacts,
        unavailable_reasons: reasons,
    };
    let model = serde_json::to_vec(&report).map_err(|error| {
        Phase1Error::Infrastructure(format!(
            "could not fingerprint consolidated Phase 1 report: {error}"
        ))
    })?;
    report.report_id = format!("{:016x}", fnv1a64(&model));
    let json = serde_json::to_vec_pretty(&report).map_err(|error| {
        Phase1Error::Infrastructure(format!(
            "could not serialize consolidated Phase 1 report: {error}"
        ))
    })?;
    let markdown = phase1_consolidated_markdown(&report).into_bytes();
    let manifest = serde_json::to_vec_pretty(&artifact_manifest(
        report.report_version,
        &report.report_id,
        "phase1-report.json",
        "phase1-report.md",
        &json,
        &markdown,
    ))
    .map_err(|error| {
        Phase1Error::Infrastructure(format!("could not serialize Phase 1 manifest: {error}"))
    })?;
    publish_artifact_set(
        &options.artifact_directory,
        "phase1-report",
        &report.report_id,
        &json,
        &markdown,
        &manifest,
    )?;
    println!(
        "PHASE1_REPORT: status={}, certified={}, artifacts={}",
        report.status,
        report.certified,
        options.artifact_directory.display()
    );
    Ok(if certified {
        CommandOutcome::passed()
    } else {
        CommandOutcome::evidence_incomplete()
    })
}

/// Runs the SDK Again smoke through the existing scanner-before-worker helper path and stores
/// the result as Phase 1 evidence. The main process only reads the helper-produced JSON marker;
/// it never opens a VST3 bundle itself.
pub(crate) fn run_vst3_smoke(
    workspace_root: &Path,
    arguments: &[String],
) -> Result<CommandOutcome, Phase1Error> {
    let outcome = crate::phase_commands::run_host_checker(workspace_root, arguments)?;
    let source_path = workspace_root.join("target/phase2/host-checker-ready.json");
    let source_bytes = fs::read(&source_path).map_err(|error| {
        Phase1Error::Infrastructure(format!(
            "could not read SDK smoke source {}: {error}",
            source_path.display()
        ))
    })?;
    let source: Value = serde_json::from_slice(&source_bytes).map_err(|error| {
        Phase1Error::Infrastructure(format!(
            "SDK smoke source {} is not JSON: {error}",
            source_path.display()
        ))
    })?;
    let isolated_scanner_accepted = source
        .pointer("/isolated_scan/outcome")
        .and_then(Value::as_str)
        == Some("supported");
    let isolated_worker_processed =
        source.pointer("/worker_smoke/ok").and_then(Value::as_bool) == Some(true);
    let finite_stereo_output = source
        .pointer("/worker_smoke/output_finite")
        .and_then(Value::as_bool)
        == Some(true)
        && source
            .pointer("/worker_smoke/expected_unity_gain_output")
            .and_then(Value::as_bool)
            == Some(true);
    let passed = outcome.exit_code == 0
        && source.get("status").and_then(Value::as_str) == Some("ready")
        && isolated_scanner_accepted
        && isolated_worker_processed
        && finite_stereo_output;
    let output_directory = workspace_root.join("target/phase1/vst3-smoke");
    let raw_host_checker_name = "host-checker-ready.json";
    publish_raw_phase1_file(&output_directory, raw_host_checker_name, &source_bytes)?;
    let raw_host_checker_sha256 = sha256_hex(&source_bytes);
    let mut report = serde_json::json!({
        "report_version": REPORT_VERSION,
        "report_id": "",
        "artifact_kind": "real_vst3_smoke",
        "status": if passed { "passed" } else { "failed" },
        "acceptance_passed": passed,
        "evidence_complete": passed,
        "isolated_scanner_accepted": isolated_scanner_accepted,
        "isolated_worker_processed": isolated_worker_processed,
        "finite_stereo_output": finite_stereo_output,
        "host_checker_raw_report": raw_host_checker_name,
        "host_checker_raw_report_sha256": raw_host_checker_sha256,
        "source_host_checker_report": source_path,
        "source_report_id": source.get("report_id").and_then(Value::as_str),
    });
    let model = serde_json::to_vec(&report).map_err(|error| {
        Phase1Error::Infrastructure(format!("could not fingerprint VST3 smoke report: {error}"))
    })?;
    let report_id = format!("{:016x}", fnv1a64(&model));
    report["report_id"] = Value::String(report_id.clone());
    let json = serde_json::to_vec_pretty(&report).map_err(|error| {
        Phase1Error::Infrastructure(format!("could not serialize VST3 smoke report: {error}"))
    })?;
    let markdown = format!(
        "# Phase 1 real VST3 smoke\nreport_id: {report_id}\nstatus: {}\nscanner accepted: {isolated_scanner_accepted}\nworker processed finite stereo: {finite_stereo_output}\n",
        if passed { "passed" } else { "failed" }
    )
    .into_bytes();
    let manifest = serde_json::to_vec_pretty(&artifact_manifest(
        REPORT_VERSION,
        &report_id,
        "real-vst3-smoke.json",
        "real-vst3-smoke.md",
        &json,
        &markdown,
    ))
    .map_err(|error| {
        Phase1Error::Infrastructure(format!("could not serialize VST3 smoke manifest: {error}"))
    })?;
    publish_artifact_set(
        &output_directory,
        "real-vst3-smoke",
        &report_id,
        &json,
        &markdown,
        &manifest,
    )?;
    println!(
        "PHASE1_VST3_SMOKE: passed={passed}, artifacts={}",
        output_directory.display()
    );
    if passed {
        Ok(CommandOutcome::passed())
    } else {
        Ok(CommandOutcome::acceptance_failure())
    }
}

pub(crate) fn vst3_smoke_usage() -> &'static str {
    "usage: cargo xtask vst3-smoke --sdk <VST3_SDK_DIR>"
}

struct Phase1Artifact {
    kind: String,
    value: Value,
    directory: PathBuf,
}

fn valid_vst3_raw_source(artifact: &Phase1Artifact) -> bool {
    let Some(name) = artifact
        .value
        .pointer("/host_checker_raw_report")
        .and_then(Value::as_str)
    else {
        return false;
    };
    let Some(expected_digest) = artifact
        .value
        .pointer("/host_checker_raw_report_sha256")
        .and_then(Value::as_str)
    else {
        return false;
    };
    if name != "host-checker-ready.json"
        || expected_digest.len() != 64
        || !expected_digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return false;
    }
    fs::read(artifact.directory.join(name))
        .is_ok_and(|contents| sha256_hex(&contents) == expected_digest)
}

fn parse_phase1_report_options(
    workspace_root: &Path,
    arguments: &[String],
) -> Result<Phase1ReportOptions, String> {
    match arguments {
        [] => Ok(Phase1ReportOptions {
            artifact_directory: workspace_root.join("target/phase1"),
        }),
        [option, directory] if option == "--artifact-dir" => Ok(Phase1ReportOptions {
            artifact_directory: PathBuf::from(directory),
        }),
        _ => Err(phase1_report_usage().to_owned()),
    }
}

fn read_expected_phase1_artifact(
    manifest_path: &Path,
    expected_kind: &str,
    reasons: &mut Vec<String>,
) -> Option<Phase1Artifact> {
    match read_phase1_artifact(manifest_path) {
        Ok(artifact) if artifact.kind == expected_kind => Some(artifact),
        Ok(artifact) => {
            reasons.push(format!(
                "{} has artifact kind `{}`; expected `{expected_kind}`",
                manifest_path.display(),
                artifact.kind
            ));
            None
        }
        Err(error) => {
            reasons.push(error);
            None
        }
    }
}

fn supporting_artifact_identity_from_value(value: &Value, kind: &str) -> Option<String> {
    match kind {
        "calibrated_load" => {
            let frames = value.pointer("/configuration/frame_count")?.as_u64()?;
            Some(format!("calibrated_load:8r-{frames}f"))
        }
        "fault_isolation" => {
            let frames = value.pointer("/configuration/frame_count")?.as_u64()?;
            let mode = value.pointer("/fault_isolation/fault_mode")?.as_str()?;
            Some(format!("fault_isolation:{mode}:{frames}f"))
        }
        "real_vst3_smoke" => Some("real_vst3_smoke".to_owned()),
        _ => None,
    }
}

#[allow(clippy::too_many_lines)]
fn read_phase1_artifact(manifest_path: &Path) -> Result<Phase1Artifact, String> {
    let manifest: Value = serde_json::from_slice(&fs::read(manifest_path).map_err(|error| {
        format!(
            "could not read manifest {}: {error}",
            manifest_path.display()
        )
    })?)
    .map_err(|error| format!("manifest {} is not JSON: {error}", manifest_path.display()))?;
    let Some(object) = manifest.as_object() else {
        return Err(format!(
            "manifest {} is not an object",
            manifest_path.display()
        ));
    };
    if object.get("report_version").and_then(Value::as_u64) != Some(u64::from(REPORT_VERSION)) {
        return Err(format!(
            "manifest {} has an unknown report schema",
            manifest_path.display()
        ));
    }
    let Some(report_id) = object
        .get("report_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
    else {
        return Err(format!(
            "manifest {} has no report ID",
            manifest_path.display()
        ));
    };
    let parent = manifest_path.parent().unwrap_or_else(|| Path::new("."));
    let json_name = safe_artifact_name(object.get("json"), manifest_path)?;
    let markdown_name = safe_artifact_name(object.get("markdown"), manifest_path)?;
    let expected_json_sha256 = manifest_digest(object.get("json_sha256"), manifest_path, "JSON")?;
    let expected_markdown_sha256 =
        manifest_digest(object.get("markdown_sha256"), manifest_path, "Markdown")?;
    let json_bytes = fs::read(parent.join(json_name)).map_err(|error| {
        format!(
            "could not read report for {}: {error}",
            manifest_path.display()
        )
    })?;
    if sha256_hex(&json_bytes) != expected_json_sha256 {
        return Err(format!(
            "JSON digest and manifest disagree for {}",
            manifest_path.display()
        ));
    }
    let value: Value = serde_json::from_slice(&json_bytes).map_err(|error| {
        format!(
            "report for {} is not JSON: {error}",
            manifest_path.display()
        )
    })?;
    if value.get("report_version").and_then(Value::as_u64) != Some(u64::from(REPORT_VERSION))
        || value.get("report_id").and_then(Value::as_str) != Some(report_id)
    {
        return Err(format!(
            "report and manifest disagree for {}",
            manifest_path.display()
        ));
    }
    let markdown_bytes = fs::read(parent.join(markdown_name)).map_err(|error| {
        format!(
            "could not read markdown for {}: {error}",
            manifest_path.display()
        )
    })?;
    if sha256_hex(&markdown_bytes) != expected_markdown_sha256 {
        return Err(format!(
            "Markdown digest and manifest disagree for {}",
            manifest_path.display()
        ));
    }
    let markdown = String::from_utf8(markdown_bytes).map_err(|error| {
        format!(
            "markdown for {} is not UTF-8: {error}",
            manifest_path.display()
        )
    })?;
    if !markdown.contains(report_id) {
        return Err(format!(
            "markdown and manifest disagree for {}",
            manifest_path.display()
        ));
    }
    let kind = value
        .get("artifact_kind")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            let scope = value.pointer("/labels/scope").and_then(Value::as_str);
            match scope {
                Some("active_device_callback") => Some("active_device_cell".to_owned()),
                Some("synthetic_preflight") if value.get("cells").is_some() => {
                    Some("synthetic_matrix".to_owned())
                }
                Some("synthetic_preflight") => Some("synthetic_preflight".to_owned()),
                _ => None,
            }
        })
        .unwrap_or_else(|| "unknown".to_owned());
    Ok(Phase1Artifact {
        kind,
        value,
        directory: parent.to_path_buf(),
    })
}

fn manifest_digest<'a>(
    value: Option<&'a Value>,
    manifest: &Path,
    artifact_name: &str,
) -> Result<&'a str, String> {
    let Some(digest) = value.and_then(Value::as_str) else {
        return Err(format!(
            "manifest {} is missing a {artifact_name} SHA-256 digest",
            manifest.display()
        ));
    };
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!(
            "manifest {} has an invalid {artifact_name} SHA-256 digest",
            manifest.display()
        ));
    }
    Ok(digest)
}

fn safe_artifact_name<'a>(value: Option<&'a Value>, manifest: &Path) -> Result<&'a str, String> {
    let Some(name) = value.and_then(Value::as_str) else {
        return Err(format!(
            "manifest {} has an invalid artifact name",
            manifest.display()
        ));
    };
    if Path::new(name).components().count() != 1 {
        return Err(format!(
            "manifest {} has an unsafe artifact name",
            manifest.display()
        ));
    }
    Ok(name)
}

fn strictly_below_period_fraction(
    duration_micros: u64,
    period_micros: u64,
    numerator: u64,
    denominator: u64,
) -> bool {
    u128::from(duration_micros) * u128::from(denominator)
        < u128::from(period_micros) * u128::from(numerator)
}

#[allow(clippy::too_many_lines)]
fn validate_active_device_cell(value: &Value, reasons: &mut Vec<String>) -> Option<String> {
    let cell = format!(
        "{}r-{}f",
        value
            .pointer("/configuration/rack_count")
            .and_then(Value::as_u64)?,
        value
            .pointer("/configuration/frame_count")
            .and_then(Value::as_u64)?
    );
    let required = [
        ("/labels/coreaudio_callback_attached", true),
        ("/labels/active_device_preflight", true),
        ("/acceptance_passed", true),
        ("/evidence_complete", true),
        ("/timing_thresholds/passed", true),
    ];
    for (path, expected) in required {
        if value.pointer(path).and_then(Value::as_bool) != Some(expected) {
            reasons.push(format!("{cell} lacks required {path}"));
        }
    }
    if value
        .pointer("/configuration/requested_duration_seconds")
        .and_then(Value::as_u64)
        != Some(PHASE1_CERTIFICATION_DURATION_SECONDS)
        || value
            .pointer("/configuration/observed_duration_micros")
            .and_then(Value::as_u64)
            .is_none_or(|duration| duration < PHASE1_CERTIFICATION_DURATION_SECONDS * 1_000_000)
    {
        reasons.push(format!(
            "{cell} does not prove an exact 1800-second cell duration"
        ));
    }
    let exact_thresholds = [
        ("/timing_thresholds/dispatch_p9999_limit_micros", 150),
        (
            "/timing_thresholds/dispatch_maximum_limit_micros_exclusive",
            400,
        ),
    ];
    for (path, expected) in exact_thresholds {
        if value.pointer(path).and_then(Value::as_u64) != Some(expected) {
            reasons.push(format!("{cell} has non-plan threshold {path}"));
        }
    }
    for (path, expected) in [
        (
            "/timing_thresholds/callback_p999_limit_period_fraction_exclusive",
            0.7,
        ),
        (
            "/timing_thresholds/callback_p9999_limit_period_fraction_exclusive",
            0.8,
        ),
    ] {
        if value.pointer(path).and_then(Value::as_f64) != Some(expected) {
            reasons.push(format!("{cell} has non-plan threshold {path}"));
        }
    }
    if value
        .pointer("/timing_thresholds/callback_maximum_limit_period_exclusive")
        .and_then(Value::as_bool)
        != Some(true)
    {
        reasons.push(format!("{cell} has non-plan callback maximum threshold"));
    }
    let period = value
        .pointer("/configuration/block_period_micros")
        .and_then(Value::as_u64);
    let timing_within_limits = value
        .pointer("/timing_thresholds/p9999_enforced_for_acceptance")
        .and_then(Value::as_bool)
        == Some(true)
        && value
            .pointer("/timing_thresholds/dispatch_p9999_status")
            .and_then(Value::as_str)
            == Some("available")
        && value
            .pointer("/timing_thresholds/dispatch_p9999_micros")
            .and_then(Value::as_u64)
            .is_some_and(|value| value <= 150)
        && value
            .pointer("/timing_thresholds/dispatch_maximum_micros")
            .and_then(Value::as_u64)
            .is_some_and(|value| value < 400)
        && value
            .pointer("/timing_thresholds/callback_p9999_status")
            .and_then(Value::as_str)
            == Some("available")
        && period.is_some_and(|period| {
            value
                .pointer("/timing_thresholds/callback_p999_micros")
                .and_then(Value::as_u64)
                .is_some_and(|value| strictly_below_period_fraction(value, period, 7, 10))
                && value
                    .pointer("/timing_thresholds/callback_p9999_micros")
                    .and_then(Value::as_u64)
                    .is_some_and(|value| strictly_below_period_fraction(value, period, 8, 10))
                && value
                    .pointer("/timing_thresholds/callback_maximum_micros")
                    .and_then(Value::as_u64)
                    .is_some_and(|value| value < period)
        });
    if !timing_within_limits {
        reasons.push(format!(
            "{cell} timing measurements exceed or omit plan thresholds"
        ));
    }
    let stereo_client_map_is_explicit = value
        .pointer("/device/client_output_channel_map")
        .and_then(Value::as_array)
        .is_some_and(|map| {
            map.len() == 2 && map[0].as_u64() == Some(1) && map[1].as_u64() == Some(2)
        });
    if value
        .pointer("/configuration/sample_rate_hz")
        .and_then(Value::as_u64)
        != Some(48_000)
        || value
            .pointer("/device/channel_count")
            .and_then(Value::as_u64)
            .is_none_or(|channels| channels < 2)
        || value
            .pointer("/device/client_channel_count")
            .and_then(Value::as_u64)
            != Some(2)
        || !stereo_client_map_is_explicit
    {
        reasons.push(format!(
            "{cell} does not prove a fixed 48 kHz stereo client callback mapped to physical channels 1–2"
        ));
    }
    for path in [
        "/callback_stats/callback_overruns",
        "/callback_stats/protocol_faults",
        "/callback_stats/deadline_misses",
    ] {
        if value.pointer(path).and_then(Value::as_u64) != Some(0) {
            reasons.push(format!("{cell} lacks zero counter {path}"));
        }
    }
    let callback_count = value
        .pointer("/callback_telemetry/callbacks")
        .and_then(Value::as_u64);
    let renderer_callback_count = value
        .pointer("/callback_stats/callbacks")
        .and_then(Value::as_u64);
    let silenced = value
        .pointer("/callback_telemetry/silenced")
        .and_then(Value::as_u64);
    if callback_count.is_none_or(|count| count == 0)
        || renderer_callback_count.is_none_or(|count| count == 0)
        || silenced.is_none_or(|count| count == 0)
        || silenced.is_some_and(|count| Some(count) != renderer_callback_count)
        || value
            .pointer("/callback_telemetry/coherent")
            .and_then(Value::as_bool)
            != Some(true)
    {
        reasons.push(format!("{cell} lacks active callback proof"));
    }
    let cpu_complete = value
        .pointer("/cpu_evidence/status")
        .and_then(Value::as_str)
        == Some("collected")
        && [
            "/cpu_evidence/host_before",
            "/cpu_evidence/host_after",
            "/cpu_evidence/reaped_children_before",
            "/cpu_evidence/reaped_children_after",
        ]
        .iter()
        .all(|path| value.pointer(path).is_some_and(Value::is_object));
    if !cpu_complete {
        reasons.push(format!("{cell} lacks complete CPU evidence"));
    }
    let Some(racks) = value
        .pointer("/configuration/rack_count")
        .and_then(Value::as_u64)
        .and_then(|count| usize::try_from(count).ok())
    else {
        reasons.push(format!("{cell} has an invalid rack count"));
        return Some(cell);
    };
    let worker_ids = value
        .pointer("/workers/worker_ids")
        .and_then(Value::as_array);
    let generations = value
        .pointer("/workers/worker_generations")
        .and_then(Value::as_array);
    let process_ids = value
        .pointer("/workers/worker_process_ids")
        .and_then(Value::as_array);
    let banks = value
        .pointer("/workers/bank_identities")
        .and_then(Value::as_array);
    if value
        .pointer("/workers/requested_workers")
        .and_then(Value::as_u64)
        != Some(racks as u64)
        || !distinct_number_identities(worker_ids, racks)
        || !distinct_number_identities(generations, racks)
        || !distinct_number_identities(process_ids, racks)
        || !distinct_string_identities(banks, racks)
        || !complete_worker_provenance(value, racks)
    {
        reasons.push(format!(
            "{cell} lacks worker/bank identities for every rack"
        ));
    }
    if !complete_noop_configuration(value, racks) {
        reasons.push(format!("{cell} is not a no-op active-device workload"));
    }
    if !complete_timing_measurements(value, true) {
        reasons.push(format!(
            "{cell} lacks complete timing histograms with observable p99.99 measurements"
        ));
    }
    if !complete_noop_callback_counters(value) {
        reasons.push(format!(
            "{cell} lacks complete zero-fault callback counters"
        ));
    }
    if !complete_cpu_evidence(value)
        || !complete_energy_evidence(
            value,
            "device_feasibility",
            u64::try_from(racks).unwrap_or(u64::MAX),
            value
                .pointer("/configuration/frame_count")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            PHASE1_CERTIFICATION_DURATION_SECONDS,
        )
    {
        reasons.push(format!("{cell} has incomplete CPU or energy measurements"));
    }
    if value
        .pointer("/heartbeat/startup_heartbeat_verified")
        .and_then(Value::as_bool)
        != Some(true)
        || value
            .pointer("/heartbeat/all_workers_progressed")
            .and_then(Value::as_bool)
            != Some(true)
        || value
            .pointer("/heartbeat/mapped_workers")
            .and_then(Value::as_array)
            .is_none_or(|workers| {
                workers.len() != racks
                    || workers.iter().any(|worker| {
                        worker
                            .get("initial_tick")
                            .and_then(Value::as_u64)
                            .is_none_or(|tick| tick == 0)
                            || worker
                                .get("last_tick")
                                .and_then(Value::as_u64)
                                .is_none_or(|tick| tick == 0)
                            || worker.get("advances").and_then(Value::as_u64) == Some(0)
                            || worker.get("regressions").and_then(Value::as_u64) != Some(0)
                    })
            })
        || value
            .pointer("/heartbeat/worker_exit_liveness_source")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
    {
        reasons.push(format!(
            "{cell} lacks healthy heartbeat progression or worker-exit liveness evidence"
        ));
    }
    Some(cell)
}

fn distinct_number_identities(values: Option<&Vec<Value>>, expected: usize) -> bool {
    let Some(values) = values else {
        return false;
    };
    let identities = values
        .iter()
        .filter_map(Value::as_u64)
        .collect::<BTreeSet<_>>();
    identities.len() == expected && values.len() == expected
}

fn distinct_string_identities(values: Option<&Vec<Value>>, expected: usize) -> bool {
    let Some(values) = values else {
        return false;
    };
    let identities = values
        .iter()
        .filter_map(Value::as_str)
        .filter(|identity| !identity.is_empty())
        .collect::<BTreeSet<_>>();
    identities.len() == expected && values.len() == expected
}

fn complete_noop_configuration(value: &Value, racks: usize) -> bool {
    value
        .pointer("/configuration/rack_count")
        .and_then(Value::as_u64)
        == u64::try_from(racks).ok()
        && matches!(
            value
                .pointer("/configuration/frame_count")
                .and_then(Value::as_u64),
            Some(128 | 256)
        )
        && value
            .pointer("/configuration/workload")
            .and_then(Value::as_str)
            == Some("device_feasibility")
        && value
            .pointer("/configuration/compute_load_mode")
            .and_then(Value::as_str)
            == Some("none")
        && value
            .pointer("/configuration/compute_load_micros")
            .and_then(Value::as_u64)
            == Some(0)
        && value
            .pointer("/configuration/fault_mode")
            .and_then(Value::as_str)
            == Some("none")
        && value
            .pointer("/configuration/fault_target_rack")
            .is_some_and(Value::is_null)
        && value
            .pointer("/labels/phase1_hard_gate_certified")
            .and_then(Value::as_bool)
            == Some(false)
}

fn complete_timing_measurements(value: &Value, require_observable_p9999: bool) -> bool {
    const P9999_MINIMUM_SAMPLE_COUNT: u64 = 10_000;
    let all_histograms_complete = [
        "request_to_claim",
        "processing",
        "completion_observation",
        "observe_dispatch_work",
        "callback_duration",
    ]
    .iter()
    .all(|name| {
        let prefix = format!("/timing/{name}");
        let raw = format!("{prefix}/raw");
        let sample_count = value
            .pointer(&format!("{raw}/sample_count"))
            .and_then(Value::as_u64);
        let p9999_is_statistically_valid = match sample_count {
            Some(count) if count >= P9999_MINIMUM_SAMPLE_COUNT => {
                value
                    .pointer(&format!("{prefix}/p9999_micros"))
                    .and_then(Value::as_u64)
                    .is_some()
                    && value
                        .pointer(&format!("{prefix}/p9999_status"))
                        .and_then(Value::as_str)
                        == Some("available")
            }
            Some(_) => {
                value
                    .pointer(&format!("{prefix}/p9999_micros"))
                    .is_some_and(Value::is_null)
                    && value
                        .pointer(&format!("{prefix}/p9999_status"))
                        .and_then(Value::as_str)
                        == Some("statistically_underpowered")
            }
            None => false,
        };
        sample_count.is_some_and(|count| {
            count > 0 && (!require_observable_p9999 || count >= P9999_MINIMUM_SAMPLE_COUNT)
        }) && value
            .pointer(&format!("{raw}/bucket_width_micros"))
            .and_then(Value::as_u64)
            == Some(1)
            && value
                .pointer(&format!("{prefix}/p999_micros"))
                .and_then(Value::as_u64)
                .is_some()
            && p9999_is_statistically_valid
            && value
                .pointer(&format!("{raw}/max_micros"))
                .and_then(Value::as_u64)
                .is_some()
            && value
                .pointer(&format!("{prefix}/integrity_valid"))
                .and_then(Value::as_bool)
                == Some(true)
    });
    all_histograms_complete
        && value
            .pointer("/timing_thresholds/available")
            .and_then(Value::as_bool)
            == Some(true)
        && value
            .pointer("/timing_thresholds/histograms_valid")
            .and_then(Value::as_bool)
            == Some(true)
        && value
            .pointer("/timing_thresholds/p9999_minimum_sample_count")
            .and_then(Value::as_u64)
            == Some(P9999_MINIMUM_SAMPLE_COUNT)
        && value
            .pointer("/timing_thresholds/p9999_enforced_for_acceptance")
            .and_then(Value::as_bool)
            == Some(require_observable_p9999)
        && [
            "/timing_thresholds/request_to_claim_samples",
            "/timing_thresholds/processing_samples",
            "/timing_thresholds/completion_observation_samples",
            "/timing_thresholds/callback_duration_samples",
        ]
        .iter()
        .all(|path| {
            value
                .pointer(path)
                .and_then(Value::as_u64)
                .is_some_and(|count| {
                    count > 0 && (!require_observable_p9999 || count >= P9999_MINIMUM_SAMPLE_COUNT)
                })
        })
}

fn complete_noop_callback_counters(value: &Value) -> bool {
    value
        .pointer("/callback_stats/callbacks")
        .and_then(Value::as_u64)
        .is_some_and(|count| count > 0)
        && value
            .pointer("/callback_stats/accepted_completions")
            .and_then(Value::as_u64)
            .is_some_and(|count| count > 0)
        && [
            "/callback_stats/deadline_misses",
            "/callback_stats/fallback_events",
            "/callback_stats/callback_overruns",
            "/callback_stats/protocol_faults",
            "/callback_stats/worker_exits",
            "/callback_stats/fatal_error_events",
            "/callback_stats/first_error_code",
            "/callback_stats/last_error_code",
        ]
        .iter()
        .all(|path| value.pointer(path).and_then(Value::as_u64) == Some(0))
        && value
            .pointer("/callback_stats/fatal_error")
            .and_then(Value::as_bool)
            == Some(false)
}

fn complete_worker_provenance(value: &Value, racks: usize) -> bool {
    value
        .pointer("/workers/executable")
        .and_then(Value::as_str)
        .is_some_and(|path| !path.is_empty())
        && value
            .pointer("/workers/executable_sha256")
            .and_then(Value::as_str)
            .is_some_and(|digest| {
                digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        && value
            .pointer("/workers/startup_heartbeat_validation")
            .and_then(Value::as_str)
            .is_some_and(|detail| !detail.is_empty())
        && value
            .pointer("/workers/worker_ids")
            .and_then(Value::as_array)
            .is_some_and(|ids| {
                ids.len() == racks && ids.iter().all(|id| id.as_u64().is_some_and(|id| id > 0))
            })
        && value
            .pointer("/workers/worker_process_ids")
            .and_then(Value::as_array)
            .is_some_and(|ids| {
                ids.len() == racks && ids.iter().all(|id| id.as_u64().is_some_and(|id| id > 0))
            })
}

fn complete_cpu_evidence(value: &Value) -> bool {
    value
        .pointer("/cpu_evidence/status")
        .and_then(Value::as_str)
        == Some("collected")
        && [
            "/cpu_evidence/host_before",
            "/cpu_evidence/host_after",
            "/cpu_evidence/reaped_children_before",
            "/cpu_evidence/reaped_children_after",
        ]
        .iter()
        .all(|path| {
            ["user_cpu_micros", "system_cpu_micros", "max_resident_bytes"]
                .iter()
                .all(|field| {
                    value
                        .pointer(&format!("{path}/{field}"))
                        .and_then(Value::as_u64)
                        .is_some()
                })
        })
}

#[allow(clippy::too_many_lines)]
fn complete_energy_evidence(
    value: &Value,
    _workload: &str,
    rack_count: u64,
    _frame_count: u64,
    minimum_duration_seconds: u64,
) -> bool {
    let minimum_duration_micros = minimum_duration_seconds.saturating_mul(1_000_000);
    let internal = value.pointer("/energy_evidence/internal");
    let Some(internal) = internal else {
        return false;
    };
    let process_complete = |process: &Value| {
        process
            .get("process_id")
            .and_then(Value::as_u64)
            .is_some_and(|pid| pid > 0)
            && process.get("complete").and_then(Value::as_bool) == Some(true)
            && ["before", "after"].iter().all(|point| {
                process
                    .pointer(&format!("/{point}/availability"))
                    .and_then(Value::as_str)
                    == Some("available")
                    && process
                        .pointer(&format!("/{point}/raw_nanojoules"))
                        .and_then(Value::as_u64)
                        .is_some()
                    && process
                        .pointer(&format!("/{point}/joules"))
                        .and_then(Value::as_f64)
                        .is_some_and(f64::is_finite)
            })
            && process
                .get("delta_raw_nanojoules")
                .and_then(Value::as_u64)
                .is_some()
            && process
                .get("delta_joules")
                .and_then(Value::as_f64)
                .is_some_and(|joules| joules.is_finite() && joules >= 0.0)
    };
    let expected_departed_worker = |process: &Value| {
        process.get("expected_to_exit").and_then(Value::as_bool) == Some(true)
            && process
                .pointer("/before/availability")
                .and_then(Value::as_str)
                == Some("available")
            && process
                .pointer("/before/raw_nanojoules")
                .and_then(Value::as_u64)
                .is_some()
            && process
                .pointer("/before/joules")
                .and_then(Value::as_f64)
                .is_some_and(f64::is_finite)
    };
    value
        .pointer("/energy_evidence/status")
        .and_then(Value::as_str)
        == Some("collected")
        && value
            .pointer("/energy_evidence/required")
            .and_then(Value::as_bool)
            == Some(true)
        && value
            .pointer("/energy_evidence/certification_complete")
            .and_then(Value::as_bool)
            == Some(true)
        && internal.get("status").and_then(Value::as_str) == Some("collected")
        && internal.get("collector").and_then(Value::as_str) == Some("superposition-xtask")
        && internal.get("api").and_then(Value::as_str) == Some("proc_pid_rusage")
        && internal
            .get("rusage_flavor")
            .and_then(Value::as_str)
            .is_some_and(|flavor| flavor.contains("ri_energy_nj"))
        && internal
            .get("measurement_duration_micros")
            .and_then(Value::as_u64)
            .is_some_and(|duration| duration >= minimum_duration_micros)
        && internal
            .get("measurement_duration_seconds")
            .and_then(Value::as_f64)
            .is_some_and(|duration| duration.is_finite() && duration > 0.0)
        && internal
            .get("total_delta_raw_nanojoules")
            .and_then(Value::as_u64)
            .is_some_and(|energy| energy > 0)
        && internal
            .get("total_delta_joules")
            .and_then(Value::as_f64)
            .is_some_and(|energy| energy.is_finite() && energy > 0.0)
        && internal
            .get("validation_errors")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
        && internal.get("host").is_some_and(process_complete)
        && internal
            .get("workers")
            .and_then(Value::as_array)
            .is_some_and(|workers| {
                workers.len() == usize::try_from(rack_count).unwrap_or(usize::MAX)
                    && workers
                        .iter()
                        .all(|worker| process_complete(worker) || expected_departed_worker(worker))
                    && distinct_number_identities(
                        Some(
                            &workers
                                .iter()
                                .filter_map(|worker| worker.get("process_id").cloned())
                                .collect(),
                        ),
                        workers.len(),
                    )
            })
}

fn validate_synthetic_matrix(value: &Value, reasons: &mut Vec<String>) -> bool {
    let expected_cells = required_ipc_matrix_cells();
    let cells = value.get("cells").and_then(Value::as_array);
    let observed_cells = cells
        .map(|cells| {
            cells
                .iter()
                .filter(|cell| {
                    cell.get("acceptance_passed").and_then(Value::as_bool) == Some(true)
                        && cell.get("evidence_complete").and_then(Value::as_bool) == Some(true)
                        && cell.get("infrastructure_error").is_none_or(Value::is_null)
                })
                .filter_map(|cell| {
                    Some(format!(
                        "{}r-{}f",
                        cell.get("rack_count")?.as_u64()?,
                        cell.get("frame_count")?.as_u64()?
                    ))
                })
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    let passed = value.pointer("/labels/scope").and_then(Value::as_str)
        == Some("synthetic_preflight")
        && value.get("duration_seconds").and_then(Value::as_u64)
            == Some(PHASE1_CERTIFICATION_DURATION_SECONDS)
        && value.get("acceptance_passed").and_then(Value::as_bool) == Some(true)
        && value.get("evidence_complete").and_then(Value::as_bool) == Some(true)
        && value
            .pointer("/qualification/qualifying")
            .and_then(Value::as_bool)
            == Some(true)
        && value
            .pointer("/qualification/official_duration_seconds")
            .and_then(Value::as_u64)
            == Some(PHASE1_CERTIFICATION_DURATION_SECONDS)
        && value
            .pointer("/qualification/required_matrix_cells")
            .and_then(Value::as_array)
            .is_some_and(|cells| cells.len() == expected_cells.len())
        && observed_cells.len() == expected_cells.len()
        && expected_cells
            .into_iter()
            .all(|cell| observed_cells.contains(&cell));
    if !passed {
        reasons.push("synthetic IPC matrix is incomplete or not successful".to_owned());
    }
    passed
}

fn timing_thresholds_match_plan(value: &Value, require_observable_p9999: bool) -> bool {
    let period = value
        .pointer("/configuration/block_period_micros")
        .and_then(Value::as_u64);
    let exact_thresholds = value
        .pointer("/timing_thresholds/passed")
        .and_then(Value::as_bool)
        == Some(true)
        && value
            .pointer("/timing_thresholds/dispatch_p9999_limit_micros")
            .and_then(Value::as_u64)
            == Some(150)
        && value
            .pointer("/timing_thresholds/dispatch_maximum_limit_micros_exclusive")
            .and_then(Value::as_u64)
            == Some(400)
        && value
            .pointer("/timing_thresholds/callback_p999_limit_period_fraction_exclusive")
            .and_then(Value::as_f64)
            == Some(0.7)
        && value
            .pointer("/timing_thresholds/callback_p9999_limit_period_fraction_exclusive")
            .and_then(Value::as_f64)
            == Some(0.8)
        && value
            .pointer("/timing_thresholds/callback_maximum_limit_period_exclusive")
            .and_then(Value::as_bool)
            == Some(true)
        && value
            .pointer("/timing_thresholds/p9999_minimum_sample_count")
            .and_then(Value::as_u64)
            == Some(10_000)
        && value
            .pointer("/timing_thresholds/p9999_enforced_for_acceptance")
            .and_then(Value::as_bool)
            == Some(require_observable_p9999);
    if !require_observable_p9999 {
        return exact_thresholds;
    }
    exact_thresholds
        && value
            .pointer("/timing_thresholds/dispatch_p9999_status")
            .and_then(Value::as_str)
            == Some("available")
        && value
            .pointer("/timing_thresholds/dispatch_p9999_micros")
            .and_then(Value::as_u64)
            .is_some_and(|micros| micros <= 150)
        && value
            .pointer("/timing_thresholds/dispatch_maximum_micros")
            .and_then(Value::as_u64)
            .is_some_and(|micros| micros < 400)
        && value
            .pointer("/timing_thresholds/callback_p9999_status")
            .and_then(Value::as_str)
            == Some("available")
        && period.is_some_and(|period| {
            value
                .pointer("/timing_thresholds/callback_p999_micros")
                .and_then(Value::as_u64)
                .is_some_and(|micros| strictly_below_period_fraction(micros, period, 7, 10))
                && value
                    .pointer("/timing_thresholds/callback_p9999_micros")
                    .and_then(Value::as_u64)
                    .is_some_and(|micros| strictly_below_period_fraction(micros, period, 8, 10))
                && value
                    .pointer("/timing_thresholds/callback_maximum_micros")
                    .and_then(Value::as_u64)
                    .is_some_and(|micros| micros < period)
        })
}

#[allow(clippy::too_many_lines)]
fn active_device_common_evidence(
    value: &Value,
    expected_racks: u64,
    minimum_duration_seconds: u64,
) -> bool {
    let frames = value
        .pointer("/configuration/frame_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    value
        .pointer("/labels/coreaudio_callback_attached")
        .and_then(Value::as_bool)
        == Some(true)
        && value
            .pointer("/labels/active_device_preflight")
            .and_then(Value::as_bool)
            == Some(true)
        && value
            .pointer("/labels/phase1_hard_gate_certified")
            .and_then(Value::as_bool)
            == Some(false)
        && value.get("evidence_complete").and_then(Value::as_bool) == Some(true)
        && value
            .pointer("/configuration/rack_count")
            .and_then(Value::as_u64)
            == Some(expected_racks)
        && matches!(frames, 128 | 256)
        && value
            .pointer("/configuration/sample_rate_hz")
            .and_then(Value::as_u64)
            == Some(u64::from(SAMPLE_RATE_HZ))
        && value
            .pointer("/configuration/requested_duration_seconds")
            .and_then(Value::as_u64)
            == Some(minimum_duration_seconds)
        && value
            .pointer("/configuration/observed_duration_micros")
            .and_then(Value::as_u64)
            .is_some_and(|duration| duration >= minimum_duration_seconds.saturating_mul(1_000_000))
        && value
            .pointer("/device/channel_count")
            .and_then(Value::as_u64)
            .is_some_and(|channels| channels >= 2)
        && value
            .pointer("/device/client_channel_count")
            .and_then(Value::as_u64)
            == Some(2)
        && value
            .pointer("/device/client_output_channel_map")
            .and_then(Value::as_array)
            .is_some_and(|map| {
                map.len() == 2 && map[0].as_u64() == Some(1) && map[1].as_u64() == Some(2)
            })
        && value
            .pointer("/callback_telemetry/callbacks")
            .and_then(Value::as_u64)
            .is_some_and(|count| count > 0)
        && value
            .pointer("/callback_telemetry/silenced")
            .and_then(Value::as_u64)
            .is_some_and(|count| count > 0)
        && value
            .pointer("/callback_telemetry/coherent")
            .and_then(Value::as_bool)
            == Some(true)
        && value
            .pointer(if frames == 128 {
                "/callback_telemetry/frame_histogram_128"
            } else {
                "/callback_telemetry/frame_histogram_256"
            })
            .and_then(Value::as_u64)
            .is_some_and(|count| count > 0)
        && [
            "/callback_telemetry/invalid_frames",
            "/callback_telemetry/invalid_buffers",
            "/callback_telemetry/invalid_channels",
            "/callback_telemetry/invalid_bytes",
        ]
        .iter()
        .all(|path| value.pointer(path).and_then(Value::as_u64) == Some(0))
        && value
            .pointer("/callback_stats/callbacks")
            .and_then(Value::as_u64)
            .is_some_and(|count| count > 0)
        && value
            .pointer("/callback_stats/accepted_completions")
            .and_then(Value::as_u64)
            .is_some_and(|count| count > 0)
        && complete_timing_measurements(
            value,
            minimum_duration_seconds >= PHASE1_CERTIFICATION_DURATION_SECONDS,
        )
        && timing_thresholds_match_plan(
            value,
            minimum_duration_seconds >= PHASE1_CERTIFICATION_DURATION_SECONDS,
        )
        && complete_worker_provenance(value, usize::try_from(expected_racks).unwrap_or(usize::MAX))
        && value
            .pointer("/heartbeat/startup_heartbeat_verified")
            .and_then(Value::as_bool)
            == Some(true)
        && value
            .pointer("/heartbeat/all_workers_progressed")
            .and_then(Value::as_bool)
            == Some(true)
        && value
            .pointer("/heartbeat/mapped_workers")
            .and_then(Value::as_array)
            .is_some_and(|workers| {
                workers.len() == usize::try_from(expected_racks).unwrap_or(usize::MAX)
                    && workers.iter().all(|worker| {
                        worker
                            .get("initial_tick")
                            .and_then(Value::as_u64)
                            .is_some_and(|tick| tick > 0)
                            && worker
                                .get("last_tick")
                                .and_then(Value::as_u64)
                                .is_some_and(|tick| tick > 0)
                            && worker
                                .get("advances")
                                .and_then(Value::as_u64)
                                .is_some_and(|count| count > 0)
                            && worker.get("regressions").and_then(Value::as_u64) == Some(0)
                    })
            })
        && value
            .pointer("/heartbeat/worker_exit_liveness_source")
            .and_then(Value::as_str)
            .is_some_and(|source| !source.is_empty())
}

#[allow(clippy::too_many_lines)]
fn validate_supporting_artifact(value: &Value, kind: &str, reasons: &mut Vec<String>) -> bool {
    let passed = match kind {
        "calibrated_load" => {
            let frames = value
                .pointer("/configuration/frame_count")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            active_device_common_evidence(value, 8, PHASE1_CERTIFICATION_DURATION_SECONDS)
                && value.get("acceptance_passed").and_then(Value::as_bool) == Some(true)
                && value
                    .pointer("/configuration/workload")
                    .and_then(Value::as_str)
                    == Some("calibrated_cpu")
                && value
                    .pointer("/configuration/compute_load_mode")
                    .and_then(Value::as_str)
                    == Some("calibrated-cpu")
                && value
                    .pointer("/configuration/compute_load_micros")
                    .and_then(Value::as_u64)
                    .is_some_and(|duration| duration > 0)
                && value
                    .pointer("/calibrated_load/mode")
                    .and_then(Value::as_str)
                    == Some("calibrated-cpu")
                && value
                    .pointer("/calibrated_load/deterministic_target_per_request_micros")
                    .and_then(Value::as_u64)
                    == value
                        .pointer("/configuration/compute_load_micros")
                        .and_then(Value::as_u64)
                && value
                    .pointer("/calibrated_load/complete")
                    .and_then(Value::as_bool)
                    == Some(true)
                && value
                    .pointer("/calibrated_load/workers")
                    .and_then(Value::as_array)
                    .is_some_and(|workers| {
                        workers.len() == 8
                            && workers.iter().all(|worker| {
                                worker
                                    .get("operations")
                                    .and_then(Value::as_u64)
                                    .is_some_and(|count| count > 0)
                                    && worker
                                        .get("requested_busy_micros")
                                        .and_then(Value::as_u64)
                                        .is_some_and(|duration| duration > 0)
                                    && worker
                                        .get("observed_busy_micros")
                                        .and_then(Value::as_u64)
                                        .is_some_and(|duration| duration > 0)
                            })
                    })
                && complete_noop_callback_counters(value)
                && complete_cpu_evidence(value)
                && complete_energy_evidence(
                    value,
                    "calibrated_cpu",
                    8,
                    frames,
                    PHASE1_CERTIFICATION_DURATION_SECONDS,
                )
        }
        "fault_isolation" => {
            let frames = value
                .pointer("/configuration/frame_count")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let mode = value
                .pointer("/fault_isolation/fault_mode")
                .and_then(Value::as_str);
            let expected_fault_counter = match mode {
                Some("self-crash") => {
                    value
                        .pointer("/fault_isolation/control_plane_worker_exit_observed")
                        .and_then(Value::as_bool)
                        == Some(true)
                }
                Some("hang-after-claim") => value
                    .pointer("/callback_stats/deadline_misses")
                    .and_then(Value::as_u64)
                    .is_some_and(|count| count > 0),
                _ => false,
            };
            active_device_common_evidence(value, 2, 30)
                && value
                    .pointer("/configuration/workload")
                    .and_then(Value::as_str)
                    == Some("device_fault_isolation")
                && value
                    .pointer("/configuration/fault_target_rack")
                    .and_then(Value::as_u64)
                    == Some(0)
                && value
                    .pointer("/configuration/fault_trigger_sequence")
                    .and_then(Value::as_u64)
                    == Some(2)
                && matches!(mode, Some("self-crash" | "hang-after-claim"))
                && value
                    .pointer("/fault_isolation/active_device_callback")
                    .and_then(Value::as_bool)
                    == Some(true)
                && value
                    .pointer("/fault_isolation/target_fault_observed")
                    .and_then(Value::as_bool)
                    == Some(true)
                && value
                    .pointer("/fault_isolation/unaffected_racks_continued")
                    .and_then(Value::as_bool)
                    == Some(true)
                && value
                    .pointer("/fault_isolation/fallback_by_current_or_next_block")
                    .and_then(Value::as_bool)
                    == Some(true)
                && value
                    .pointer("/fault_isolation/passed")
                    .and_then(Value::as_bool)
                    == Some(true)
                && value
                    .pointer("/callback_stats/fallback_events")
                    .and_then(Value::as_u64)
                    .is_some_and(|count| count > 0)
                && value
                    .pointer("/callback_stats/callback_overruns")
                    .and_then(Value::as_u64)
                    == Some(0)
                && expected_fault_counter
                && complete_cpu_evidence(value)
                && complete_energy_evidence(value, "device_fault_isolation", 2, frames, 30)
        }
        "real_vst3_smoke" => {
            value.get("status").and_then(Value::as_str) == Some("passed")
                && value.get("acceptance_passed").and_then(Value::as_bool) == Some(true)
                && value.get("evidence_complete").and_then(Value::as_bool) == Some(true)
                && value
                    .pointer("/isolated_scanner_accepted")
                    .and_then(Value::as_bool)
                    == Some(true)
                && value
                    .pointer("/isolated_worker_processed")
                    .and_then(Value::as_bool)
                    == Some(true)
                && value
                    .pointer("/finite_stereo_output")
                    .and_then(Value::as_bool)
                    == Some(true)
                && value
                    .pointer("/host_checker_raw_report")
                    .and_then(Value::as_str)
                    == Some("host-checker-ready.json")
                && value
                    .pointer("/host_checker_raw_report_sha256")
                    .and_then(Value::as_str)
                    .is_some_and(|digest| {
                        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
                    })
        }
        _ => false,
    };
    if !passed {
        reasons.push(format!("{kind} artifact is incomplete or not successful"));
    }
    passed
}

fn phase1_consolidated_markdown(report: &Phase1ConsolidatedReport) -> String {
    let mut markdown = format!(
        "# Phase 1 consolidated report\nreport_id: {}\nstatus: {}\ncertified: {}\n",
        report.report_id, report.status, report.certified
    );
    if !report.certified {
        markdown.push_str("## Unavailable evidence\n");
        for reason in &report.unavailable_reasons {
            writeln!(markdown, "- {reason}").expect("String writes cannot fail");
        }
    }
    markdown
}

pub(crate) fn phase1_report_usage() -> &'static str {
    "usage: cargo xtask phase1-report [--artifact-dir <directory>]"
}

pub(crate) fn build_worker(
    workspace_root: &Path,
    fault_injection: bool,
) -> Result<PathBuf, String> {
    let mut command = Command::new("cargo");
    command.args([
        "build",
        "--locked",
        "--package",
        "sp-plugin-worker",
        "--message-format=json-render-diagnostics",
    ]);
    if fault_injection {
        command.args(["--features", "feasibility-fault-injection"]);
    }
    // Phase 1 launches feature-distinct workers (normal and fault injection) as real helper
    // processes. Keep those artifacts under the ignored qualification tree so a feature/ABI
    // variant cannot replace the ordinary workspace `target/debug/sp-plugin-worker` binary.
    let phase1_target_directory = workspace_root.join("target/phase1/worker-build");
    let output = command
        .current_dir(workspace_root)
        .env("CARGO_TARGET_DIR", phase1_target_directory)
        .output()
        .map_err(|error| format!("could not build sp-plugin-worker: {error}"))?;
    let (executable, diagnostics) = worker_artifact_from_cargo_messages(&output.stdout);
    if !output.status.success() {
        return Err(format!(
            "building sp-plugin-worker exited with {}: {}{}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim(),
            diagnostics
        ));
    }
    let executable = executable.ok_or_else(|| {
        format!("cargo built sp-plugin-worker but emitted no executable artifact{diagnostics}")
    })?;
    executable.is_file().then_some(executable).ok_or_else(|| {
        "cargo reported an sp-plugin-worker executable which does not exist".to_owned()
    })
}

fn worker_artifact_from_cargo_messages(messages: &[u8]) -> (Option<PathBuf>, String) {
    let mut executable = None;
    let mut diagnostics = String::new();
    for line in messages.split(|byte| *byte == b'\n') {
        let Ok(message) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        if message.get("reason").and_then(Value::as_str) == Some("compiler-artifact")
            && message.pointer("/target/name").and_then(Value::as_str) == Some("sp-plugin-worker")
        {
            executable = message
                .get("executable")
                .and_then(Value::as_str)
                .map(PathBuf::from);
        }
        if let Some(rendered) = message.pointer("/message/rendered").and_then(Value::as_str) {
            diagnostics.push('\n');
            diagnostics.push_str(rendered.trim());
        }
    }
    (executable, diagnostics)
}

pub(crate) fn fixed_request(frame_count: u32) -> BlockRequest {
    BlockRequest {
        frame_count,
        input_channel_count: 2,
        output_channel_count: 2,
        midi_event_count: 0,
        event_count: 0,
        flags: 0,
    }
}

pub(crate) fn block_period(frame_count: u32) -> Duration {
    Duration::from_secs_f64(f64::from(frame_count) / f64::from(SAMPLE_RATE_HZ))
}

fn duration_to_blocks(duration: Duration, period: Duration) -> Result<u64, String> {
    let blocks = duration.as_nanos() / period.as_nanos();
    u64::try_from(blocks.max(1))
        .map_err(|_| "requested duration creates too many blocks".to_owned())
}

fn duration_to_micros_ceil(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos().div_ceil(1_000)).unwrap_or(u64::MAX)
}

fn format_duration(duration: Duration) -> String {
    format!("{:.3} us", duration.as_secs_f64() * 1_000_000.0)
}

fn timing_thresholds(timing: &TimingHistograms, period: Duration) -> TimingThresholds {
    let dispatch_p9999 = timing
        .request_to_observation
        .percentile_upper_bound(9_999, 10_000);
    let dispatch_max = timing.request_to_observation.max_duration();
    let callback_p999 = timing
        .synthetic_callback_work
        .percentile_upper_bound(999, 1_000);
    let callback_p9999 = timing
        .synthetic_callback_work
        .percentile_upper_bound(9_999, 10_000);
    let callback_max = timing.synthetic_callback_work.max_duration();
    let passed = timing.request_to_observation.sample_count() > 0
        && timing.synthetic_callback_work.sample_count() > 0
        && dispatch_p9999 <= Duration::from_micros(150)
        && dispatch_max < Duration::from_micros(400)
        && callback_p999 < period.mul_f64(0.7)
        && callback_p9999 < period.mul_f64(0.8)
        && callback_max < period;
    TimingThresholds {
        dispatch_p9999_below_micros: 150,
        dispatch_max_below_micros: 400,
        callback_p999_below_period_fraction: 0.7,
        callback_p9999_below_period_fraction: 0.8,
        dispatch_p9999_micros: duration_to_micros_ceil(dispatch_p9999),
        dispatch_max_micros: duration_to_micros_ceil(dispatch_max),
        callback_p999_micros: duration_to_micros_ceil(callback_p999),
        callback_p9999_micros: duration_to_micros_ceil(callback_p9999),
        callback_max_micros: duration_to_micros_ceil(callback_max),
        passed,
    }
}

fn fallback_label(reason: FallbackReason) -> &'static str {
    match reason {
        FallbackReason::DeadlineMiss => "deadline_miss",
        FallbackReason::WorkerExited => "worker_exited",
        FallbackReason::MalformedCompletion => "malformed_completion",
        FallbackReason::StaleCompletion => "stale_completion",
        FallbackReason::InvalidProtocolState => "invalid_protocol_state",
        FallbackReason::SlotUnavailable => "slot_unavailable",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_environment() -> EnvironmentEvidence {
        EnvironmentEvidence {
            operating_system: "macos".to_owned(),
            architecture: "aarch64".to_owned(),
            os_version: "test".to_owned(),
            hardware_model: "test-hardware".to_owned(),
            hardware_memory_bytes: "1".to_owned(),
            rust_version: "rustc test".to_owned(),
            source_revision: "test-revision".to_owned(),
            source_state: "dirty".to_owned(),
        }
    }

    fn test_directory(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "superposition-phase1-{name}-{}-{nonce}",
            std::process::id()
        ))
    }

    #[test]
    fn histogram_has_conservative_one_microsecond_boundaries() {
        let mut histogram = FixedHistogram::new();
        histogram.observe(Duration::ZERO);
        histogram.observe(Duration::from_nanos(1));
        histogram.observe(Duration::from_millis(10));
        histogram.observe(Duration::from_nanos(10_000_001));

        assert_eq!(histogram.buckets[0], 1);
        assert_eq!(histogram.buckets[1], 1);
        assert_eq!(histogram.buckets[HISTOGRAM_MAX_MICROS], 1);
        assert_eq!(histogram.overflow_count, 1);
        assert_eq!(
            histogram.percentile_upper_bound(1, 2),
            Duration::from_micros(1)
        );
        assert_eq!(histogram.max_duration(), Duration::from_micros(10_001));
    }

    #[test]
    fn histogram_aggregation_is_order_independent() {
        let mut first = TimingHistograms::default();
        first.request_to_claim.observe(Duration::from_micros(2));
        let mut second = TimingHistograms::default();
        second.request_to_claim.observe(Duration::from_micros(7));
        let forward = aggregate_baseline_histograms([&first, &second]).unwrap();
        let reverse = aggregate_baseline_histograms([&second, &first]).unwrap();
        assert_eq!(forward, reverse);
        assert_eq!(forward.request_to_claim.sample_count(), 2);
        assert_eq!(
            forward.request_to_claim.max_duration(),
            Duration::from_micros(7)
        );
    }

    #[test]
    fn timing_thresholds_accept_the_inclusive_dispatch_percentile_boundary() {
        let period = Duration::from_millis(10);
        let mut timing = TimingHistograms::default();
        timing
            .request_to_observation
            .observe(Duration::from_micros(150));
        timing
            .synthetic_callback_work
            .observe(Duration::from_micros(1));
        assert!(timing_thresholds(&timing, period).passed);

        let mut timing = TimingHistograms::default();
        timing
            .request_to_observation
            .observe(Duration::from_micros(151));
        timing
            .synthetic_callback_work
            .observe(Duration::from_micros(1));
        assert!(!timing_thresholds(&timing, period).passed);
    }

    #[test]
    fn stale_fault_uses_worker_protocol_injection_and_gate_classification() {
        assert_eq!(
            FaultCase::StaleSequenceCompletion
                .worker_fault(Duration::from_millis(5))
                .mode,
            FaultMode::StaleCompletion
        );
        assert_eq!(
            FaultCase::StaleSequenceCompletion.injection_layer(),
            "worker_sp_test_support_protocol_test_hook"
        );
        assert_eq!(
            FaultCase::StaleGenerationCompletion.injection_layer(),
            "xtask_gate_adapter_boundary"
        );
        let ticket = BlockTicket {
            generation: 4,
            sequence: 8,
        };
        let mut gate = RackGate::new();
        assert_eq!(gate.dispatch(ticket, 1), GateOutcome::DispatchAllowed);
        assert_eq!(
            gate.observe(
                WorkerObservation::ProtocolFault(ProtocolError::StaleCompletion),
                2
            ),
            GateOutcome::UseFallback(FallbackReason::StaleCompletion)
        );
    }

    #[test]
    fn readiness_wait_reports_eof_and_timeout_without_blocking() {
        let (eof_sender, eof_receiver) = mpsc::sync_channel(1);
        eof_sender.send(Ok(String::new())).unwrap();
        let eof = wait_for_readiness(&eof_receiver, Duration::ZERO).unwrap_err();
        assert!(eof.contains("pipe closed"));

        let (timeout_sender, timeout_receiver) = mpsc::sync_channel(1);
        let error = wait_for_readiness(&timeout_receiver, Duration::ZERO).unwrap_err();
        assert!(error.contains("timed out"));
        drop(timeout_sender);
    }

    #[test]
    fn closed_gate_rejects_a_late_completion() {
        let ticket = BlockTicket {
            generation: 1,
            sequence: 1,
        };
        let mut gate = RackGate::new();
        assert_eq!(gate.dispatch(ticket, 2), GateOutcome::DispatchAllowed);
        assert_eq!(
            gate.deadline_expired(3),
            GateOutcome::UseFallback(FallbackReason::DeadlineMiss)
        );
        assert_eq!(
            gate.observe(WorkerObservation::Completed(ticket), 4),
            GateOutcome::UseFallback(FallbackReason::DeadlineMiss)
        );
    }

    #[test]
    fn device_callback_expires_an_uncompleted_request_at_its_budget() {
        let mut harness = DeviceHarness {
            racks: vec![DeviceRackHarness {
                region: SharedMemoryRegion::create(1).unwrap(),
                gate: RackGate::new(),
                live: None,
                worker_exited: Arc::new(AtomicBool::new(false)),
            }],
        };
        let clock = MonotonicClock::new().unwrap();
        let request = fixed_request(128);
        let mut timing = TimingHistograms::default();

        let result = process_device_callback_block(&mut harness, 0, request, clock, 0, &mut timing);
        assert_eq!(result.accepted_racks, 0);
        assert_eq!(result.worker_exits, 0);
        assert_eq!(result.deadline_misses, 1);
        assert_eq!(result.fallback_events, 1);
        assert_eq!(result.first_fallback_block_plus_one[0], 1);
        assert!(matches!(
            harness.racks[0].gate.state(),
            RackGateState::Closed {
                reason: FallbackReason::DeadlineMiss,
                ..
            }
        ));
    }

    #[test]
    fn recovery_only_replaces_the_target_identity() {
        let target_before = RackIdentity {
            rack_index: 0,
            process_id: 10,
            generation: 1,
        };
        let unaffected = RackIdentity {
            rack_index: 1,
            process_id: 11,
            generation: 2,
        };
        let target_after = RackIdentity {
            rack_index: 0,
            process_id: 12,
            generation: 1_001,
        };
        let isolation = IsolationEvidence {
            original: unaffected.clone(),
            final_identity: unaffected,
            no_fallback_protocol_or_deadline_fault: true,
            passed: true,
        };
        let recovery = RecoveryEvidence {
            old_target: target_before,
            replacement: target_after,
            bank_name_changed: true,
            ready_heartbeat_observed: true,
            recovery_blocks_driven: 3,
            unaffected_accepted_during_replacement: 2,
            unaffected_progress_continuous: true,
            accepted_completion: true,
        };
        assert!(isolation.passed);
        assert_ne!(
            recovery.old_target.generation,
            recovery.replacement.generation
        );
        assert!(recovery.unaffected_progress_continuous);
        assert!(recovery.accepted_completion);
    }

    #[test]
    fn report_json_and_markdown_include_required_labels() {
        let options = FaultMatrixOptions {
            rack_count: 2,
            frame_count: 128,
            output_directory: PathBuf::from("/tmp/ignored"),
            energy_evidence_path: None,
            require_energy_evidence: false,
        };
        let mut report = FaultMatrixReport::new(
            &options,
            block_period(128),
            empty_energy_evidence(),
            test_environment(),
            None,
            None,
        );
        assign_fault_report_id(&mut report).unwrap();
        let json = serde_json::to_string(&report).unwrap();
        let parsed: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["configuration"]["rack_count"], 2);
        assert_eq!(parsed["environment"]["hardware_model"], "test-hardware");
        assert_eq!(parsed["report_id"], report.report_id);
        let markdown = report_markdown(&report);
        for required in [
            "synthetic_preflight",
            "active_device_callback",
            "coreaudio_callback_attached",
            "phase1_hard_gate_certified",
        ] {
            assert!(json.contains(required));
            assert!(markdown.contains(required));
        }
        let directory = test_directory("full-report");
        write_report(&directory, &report).unwrap();
        let written: Value =
            serde_json::from_slice(&fs::read(directory.join("fault-matrix.json")).unwrap())
                .unwrap();
        let manifest: Value = serde_json::from_slice(
            &fs::read(directory.join("fault-matrix.manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(written["report_id"], manifest["report_id"]);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn parser_validates_fault_matrix_configuration() {
        let valid = [
            "--frames",
            "256",
            "--output-dir",
            "out",
            "--racks",
            "4",
            "--require-energy-evidence",
        ]
        .map(str::to_owned);
        let parsed = parse_fault_matrix_options(&valid).unwrap();
        assert_eq!(parsed.rack_count, 4);
        assert_eq!(parsed.frame_count, 256);
        assert!(parsed.require_energy_evidence);
        assert!(
            parse_fault_matrix_options(
                &["--racks", "1", "--frames", "128", "--output-dir", "out"].map(str::to_owned)
            )
            .is_err()
        );
        assert!(
            parse_fault_matrix_options(&["--racks", "2", "--frames", "128"].map(str::to_owned))
                .is_err()
        );
    }

    #[test]
    fn parser_validates_timed_ipc_preflight_configuration() {
        let valid =
            ["--duration-seconds", "2", "--racks", "8", "--frames", "128"].map(str::to_owned);
        let parsed = parse_feasibility_options(&valid).unwrap();
        assert_eq!(parsed.rack_count, 8);
        assert_eq!(parsed.frame_count, 128);
        assert_eq!(parsed.duration, Duration::from_secs(2));
        let bad_racks =
            ["--racks", "3", "--frames", "128", "--duration-seconds", "1"].map(str::to_owned);
        assert!(
            parse_feasibility_options(&bad_racks)
                .unwrap_err()
                .contains("--racks")
        );
        let bad_frames =
            ["--racks", "2", "--frames", "64", "--duration-seconds", "1"].map(str::to_owned);
        assert!(
            parse_feasibility_options(&bad_frames)
                .unwrap_err()
                .contains("--frames")
        );
        let bad_duration =
            ["--racks", "2", "--frames", "128", "--duration-seconds", "0"].map(str::to_owned);
        assert!(
            parse_feasibility_options(&bad_duration)
                .unwrap_err()
                .contains("--duration-seconds")
        );
        let matrix = ["--output-dir", "out", "--duration-seconds", "1"].map(str::to_owned);
        assert_eq!(
            parse_ipc_matrix_options(&matrix).unwrap().duration,
            Duration::from_secs(1)
        );
        assert!(parse_ipc_matrix_options(&["--duration-seconds", "0"].map(str::to_owned)).is_err());
    }

    #[allow(clippy::needless_pass_by_value)]
    fn write_phase1_test_artifact(directory: &Path, stem: &str, report: Value) {
        fs::create_dir_all(directory).unwrap();
        let report_id = report["report_id"].as_str().unwrap();
        let json = serde_json::to_vec(&report).unwrap();
        let markdown = format!("report_id: {report_id}\n").into_bytes();
        fs::write(directory.join(format!("{stem}.json")), &json).unwrap();
        fs::write(directory.join(format!("{stem}.md")), &markdown).unwrap();
        fs::write(
            directory.join(format!("{stem}.manifest.json")),
            serde_json::to_vec(&serde_json::json!({
                "report_version": REPORT_VERSION,
                "report_id": report_id,
                "json": format!("{stem}.json"),
                "markdown": format!("{stem}.md"),
                "json_sha256": sha256_hex(&json),
                "markdown_sha256": sha256_hex(&markdown),
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn timing_measurements() -> Value {
        let histogram = serde_json::json!({
            "raw": {"bucket_width_micros": 1, "buckets": [10_000], "sample_count": 10_000, "overflow_count": 0, "max_micros": 1},
            "p999_micros": 1,
            "p9999_micros": 1,
            "p9999_status": "available",
            "integrity_valid": true,
        });
        serde_json::json!({
            "request_to_claim": histogram.clone(),
            "processing": histogram.clone(),
            "completion_observation": histogram.clone(),
            "observe_dispatch_work": histogram.clone(),
            "callback_duration": histogram,
        })
    }

    fn certifying_timing_thresholds() -> Value {
        serde_json::json!({
            "passed": true,
            "available": true,
            "histograms_valid": true,
            "p9999_minimum_sample_count": 10_000,
            "p9999_enforced_for_acceptance": true,
            "request_to_claim_samples": 10_000,
            "processing_samples": 10_000,
            "completion_observation_samples": 10_000,
            "callback_duration_samples": 10_000,
            "dispatch_p9999_limit_micros": 150,
            "dispatch_maximum_limit_micros_exclusive": 400,
            "callback_p999_limit_period_fraction_exclusive": 0.7,
            "callback_p9999_limit_period_fraction_exclusive": 0.8,
            "callback_maximum_limit_period_exclusive": true,
            "dispatch_p9999_micros": 1,
            "dispatch_p9999_status": "available",
            "dispatch_maximum_micros": 1,
            "callback_p999_micros": 1,
            "callback_p9999_micros": 1,
            "callback_p9999_status": "available",
            "callback_maximum_micros": 1,
        })
    }

    fn resource_snapshot() -> Value {
        serde_json::json!({"user_cpu_micros": 1, "system_cpu_micros": 1, "max_resident_bytes": 1})
    }

    const CERTIFYING_ENERGY_DELTA_JOULES: f64 = 0.000_000_1;

    fn certifying_process(role: &str, process_id: u64) -> Value {
        serde_json::json!({
            "role": role,
            "process_id": process_id,
            "before": {"availability": "available", "raw_nanojoules": 100_u64, "joules": CERTIFYING_ENERGY_DELTA_JOULES, "error": null},
            "after": {"availability": "available", "raw_nanojoules": 200_u64, "joules": 2.0 * CERTIFYING_ENERGY_DELTA_JOULES, "error": null},
            "delta_raw_nanojoules": 100_u64,
            "delta_joules": CERTIFYING_ENERGY_DELTA_JOULES,
            "expected_to_exit": false,
            "complete": true,
        })
    }

    fn certifying_energy_evidence(racks: u64, duration_seconds: u64) -> Value {
        let duration_micros = duration_seconds * 1_000_000;
        let duration_seconds = f64::from(u32::try_from(duration_seconds).unwrap());
        let process_count = f64::from(u32::try_from(racks + 1).unwrap());
        serde_json::json!({
            "status": "collected",
            "required": true,
            "certification_complete": true,
            "internal": {
                "status": "collected",
                "collector": "superposition-xtask",
                "api": "proc_pid_rusage",
                "rusage_flavor": "RUSAGE_INFO_V6 (ri_energy_nj)",
                "measurement_duration_micros": duration_micros,
                "measurement_duration_seconds": duration_seconds,
                "host": certifying_process("host", 99),
                "workers": (0..racks).map(|index| certifying_process("worker", 101 + index)).collect::<Vec<_>>(),
                "expected_departed_worker_process_ids": [],
                "total_delta_raw_nanojoules": 100 * (racks + 1),
                "total_delta_joules": CERTIFYING_ENERGY_DELTA_JOULES * process_count,
                "certification_complete": true,
                "validation_errors": [],
            },
            "external_cross_check": {"status": "not_supplied", "imported": false, "path": null, "schema_version": null, "collector": null, "source": null, "measurement_duration_seconds": null, "hardware_identity": null, "workload": null, "rack_count": null, "frame_count": null, "energy_joules": null, "average_power_watts": null, "validation_errors": []},
        })
    }

    fn certifying_device_report(racks: u64, frames: u64) -> Value {
        serde_json::json!({
            "report_version": REPORT_VERSION,
            "report_id": format!("{racks}-{frames}"),
            "artifact_kind": "active_device_cell",
            "labels": {"scope": "active_device_callback", "coreaudio_callback_attached": true, "active_device_preflight": true, "phase1_hard_gate_certified": false},
            "configuration": {"rack_count": racks, "frame_count": frames, "sample_rate_hz": 48_000, "block_period_micros": 3_000, "requested_duration_seconds": 1_800, "observed_duration_micros": 1_800_000_000_u64, "workload": "device_feasibility", "compute_load_mode": "none", "compute_load_micros": 0, "fault_mode": "none", "fault_target_rack": null, "fault_trigger_sequence": 1},
            "device": {"channel_count": 2, "client_channel_count": 2, "client_output_channel_map": [1, 2]},
            "workers": {"executable": "/worker", "executable_sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef", "requested_workers": racks, "worker_ids": (1..=racks).collect::<Vec<_>>(), "worker_generations": (1..=racks).collect::<Vec<_>>(), "worker_process_ids": (101..=100 + racks).collect::<Vec<_>>(), "bank_identities": (1..=racks).map(|index| format!("bank-{index}")).collect::<Vec<_>>(), "startup_heartbeat_validation": "verified"},
            "callback_telemetry": {"callbacks": 2, "silenced": 2, "coherent": true, "frame_histogram_128": 2, "frame_histogram_256": 2, "invalid_frames": 0, "invalid_buffers": 0, "invalid_channels": 0, "invalid_bytes": 0},
            "callback_stats": {"callbacks": 2, "accepted_completions": 2, "callback_overruns": 0, "protocol_faults": 0, "deadline_misses": 0, "fallback_events": 0, "worker_exits": 0, "fatal_error_events": 0, "first_error_code": 0, "last_error_code": 0, "fatal_error": false},
            "timing": timing_measurements(),
            "timing_thresholds": certifying_timing_thresholds(),
            "heartbeat": {"startup_heartbeat_verified": true, "all_workers_progressed": true, "worker_exit_liveness_source": "monitor", "mapped_workers": (1..=racks).map(|worker_id| serde_json::json!({"worker_id": worker_id, "initial_tick": 1, "last_tick": 2, "control_polls": 1, "advances": 1, "regressions": 0, "busy_requested_ticks": 0, "busy_observed_ticks": 0, "busy_operations": 0})).collect::<Vec<_>>()},
            "calibrated_load": {"mode": "none", "deterministic_target_per_request_micros": null, "workers": (1..=racks).map(|worker_id| serde_json::json!({"worker_id": worker_id, "operations": 0, "requested_busy_micros": 0, "observed_busy_micros": 0})).collect::<Vec<_>>(), "requested_busy_micros_total": 0, "observed_busy_micros_total": 0, "operations_total": 0, "complete": true},
            "cpu_evidence": {"status": "collected", "host_before": resource_snapshot(), "host_after": resource_snapshot(), "reaped_children_before": resource_snapshot(), "reaped_children_after": resource_snapshot()},
            "energy_evidence": certifying_energy_evidence(racks, 1_800),
            "acceptance_passed": true,
            "evidence_complete": true,
        })
    }

    #[test]
    fn phase1_report_requires_complete_matrix_and_supporting_evidence() {
        let directory = test_directory("consolidated-incomplete");
        write_phase1_test_artifact(
            &directory.join("1r-128f"),
            "device-feasibility",
            certifying_device_report(1, 128),
        );
        let outcome = run_phase1_report(
            Path::new("/workspace"),
            &["--artifact-dir".to_owned(), directory.display().to_string()],
        )
        .unwrap();
        assert_eq!(outcome.exit_code, 3);
        let report: Value =
            serde_json::from_slice(&fs::read(directory.join("phase1-report.json")).unwrap())
                .unwrap();
        assert_eq!(report["status"], "unavailable");
        assert!(
            report["unavailable_reasons"]
                .as_array()
                .unwrap()
                .iter()
                .any(|reason| reason.as_str().unwrap().contains("2r-128f"))
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn phase1_report_certifies_only_complete_supported_artifacts() {
        let directory = test_directory("consolidated-complete");
        for racks in [1, 2, 4, 8] {
            for frames in [128, 256] {
                write_phase1_test_artifact(
                    &directory.join(format!("device-matrix/{racks}r-{frames}f")),
                    "device-feasibility",
                    certifying_device_report(racks, frames),
                );
            }
        }
        write_phase1_test_artifact(
            &directory.join("ipc-matrix"),
            "ipc-matrix",
            serde_json::json!({
                "report_version": REPORT_VERSION,
                "report_id": "synthetic-ipc-matrix",
                "labels": {"scope": "synthetic_preflight"},
                "duration_seconds": 1_800,
                "qualification": {"qualifying": true, "official_duration_seconds": 1_800, "required_matrix_cells": required_ipc_matrix_cells()},
                "acceptance_passed": true,
                "evidence_complete": true,
                "cells": required_ipc_matrix_cells().into_iter().map(|cell| {
                    let (racks, frames) = cell.split_once("r-").unwrap();
                    serde_json::json!({"rack_count": racks.parse::<u64>().unwrap(), "frame_count": frames.trim_end_matches('f').parse::<u64>().unwrap(), "acceptance_passed": true, "evidence_complete": true, "infrastructure_error": null})
                }).collect::<Vec<_>>(),
            }),
        );
        for frames in [128, 256] {
            let mut report = certifying_device_report(8, frames);
            report["report_id"] = Value::String(format!("calibrated-{frames}"));
            report["artifact_kind"] = Value::String("calibrated_load".to_owned());
            report["configuration"]["workload"] = Value::String("calibrated_cpu".to_owned());
            report["configuration"]["compute_load_mode"] =
                Value::String("calibrated-cpu".to_owned());
            let target = if frames == 128 { 267_u64 } else { 534_u64 };
            report["configuration"]["compute_load_micros"] = Value::from(target);
            report["calibrated_load"]["mode"] = Value::String("calibrated-cpu".to_owned());
            report["calibrated_load"]["deterministic_target_per_request_micros"] =
                Value::from(target);
            report["calibrated_load"]["complete"] = Value::Bool(true);
            report["calibrated_load"]["workers"] = (1..=8)
                .map(|worker_id| serde_json::json!({"worker_id": worker_id, "operations": 1, "requested_busy_micros": target, "observed_busy_micros": target}))
                .collect();
            write_phase1_test_artifact(
                &directory.join(format!("calibrated-load/8r-{frames}f")),
                "device-feasibility",
                report,
            );
        }
        for (mode, directory_name) in [("self-crash", "self-crash"), ("hang-after-claim", "hang")] {
            for frames in [128, 256] {
                let mut report = certifying_device_report(2, frames);
                report["report_id"] = Value::String(format!("fault-{mode}-{frames}"));
                report["artifact_kind"] = Value::String("fault_isolation".to_owned());
                report["configuration"]["requested_duration_seconds"] = Value::from(30_u64);
                report["configuration"]["observed_duration_micros"] = Value::from(30_000_000_u64);
                report["timing_thresholds"]["p9999_enforced_for_acceptance"] = Value::Bool(false);
                report["configuration"]["workload"] =
                    Value::String("device_fault_isolation".to_owned());
                report["configuration"]["fault_mode"] = Value::String(mode.to_owned());
                report["configuration"]["fault_target_rack"] = Value::from(0_u64);
                report["configuration"]["fault_trigger_sequence"] = Value::from(2_u64);
                report["callback_stats"]["fallback_events"] = Value::from(1_u64);
                if mode == "self-crash" {
                    report["callback_stats"]["worker_exits"] = Value::from(1_u64);
                } else {
                    report["callback_stats"]["deadline_misses"] = Value::from(1_u64);
                }
                report["fault_isolation"] = serde_json::json!({
                    "fault_mode": mode,
                    "active_device_callback": true,
                    "target_fault_observed": true,
                    "control_plane_worker_exit_observed": mode == "self-crash",
                    "unaffected_racks_continued": true,
                    "fallback_by_current_or_next_block": true,
                    "passed": true,
                });
                report["energy_evidence"] = certifying_energy_evidence(2, 30);
                write_phase1_test_artifact(
                    &directory.join(format!("fault-isolation/{directory_name}-2r-{frames}f")),
                    "device-feasibility",
                    report,
                );
            }
        }
        let raw_host_checker = br#"{"status":"ready","worker_smoke":{"ok":true}}"#;
        let raw_directory = directory.join("vst3-smoke");
        fs::create_dir_all(&raw_directory).unwrap();
        fs::write(
            raw_directory.join("host-checker-ready.json"),
            raw_host_checker,
        )
        .unwrap();
        write_phase1_test_artifact(
            &raw_directory,
            "real-vst3-smoke",
            serde_json::json!({
                "report_version": REPORT_VERSION,
                "report_id": "real-vst3-smoke",
                "artifact_kind": "real_vst3_smoke",
                "status": "passed",
                "acceptance_passed": true,
                "evidence_complete": true,
                "isolated_scanner_accepted": true,
                "isolated_worker_processed": true,
                "finite_stereo_output": true,
                "host_checker_raw_report": "host-checker-ready.json",
                "host_checker_raw_report_sha256": sha256_hex(raw_host_checker),
            }),
        );
        let outcome = run_phase1_report(
            Path::new("/workspace"),
            &["--artifact-dir".to_owned(), directory.display().to_string()],
        )
        .unwrap();
        assert_eq!(outcome.exit_code, 0);
        let report: Value =
            serde_json::from_slice(&fs::read(directory.join("phase1-report.json")).unwrap())
                .unwrap();
        assert_eq!(report["status"], "certified");
        assert_eq!(report["certified"], true);

        let mut underpowered = certifying_device_report(1, 128);
        underpowered["timing"]["completion_observation"]["raw"]["buckets"] =
            serde_json::json!([9_999]);
        underpowered["timing"]["completion_observation"]["raw"]["sample_count"] =
            Value::from(9_999_u64);
        underpowered["timing"]["completion_observation"]["p9999_micros"] = Value::Null;
        underpowered["timing"]["completion_observation"]["p9999_status"] =
            Value::String("statistically_underpowered".to_owned());
        underpowered["timing_thresholds"]["completion_observation_samples"] =
            Value::from(9_999_u64);
        underpowered["timing_thresholds"]["dispatch_p9999_micros"] = Value::Null;
        underpowered["timing_thresholds"]["dispatch_p9999_status"] =
            Value::String("statistically_underpowered".to_owned());
        write_phase1_test_artifact(
            &directory.join("device-matrix/1r-128f"),
            "device-feasibility",
            underpowered,
        );
        let outcome = run_phase1_report(
            Path::new("/workspace"),
            &["--artifact-dir".to_owned(), directory.display().to_string()],
        )
        .unwrap();
        assert_eq!(outcome.exit_code, 3);
        let report: Value =
            serde_json::from_slice(&fs::read(directory.join("phase1-report.json")).unwrap())
                .unwrap();
        assert_eq!(report["status"], "unavailable");
        assert!(
            report["unavailable_reasons"]
                .as_array()
                .unwrap()
                .iter()
                .any(|reason| reason.as_str().unwrap().contains("1r-128f"))
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn exit_precedence_keeps_evidence_after_behavior() {
        assert!(matches!(
            command_outcome(Some("infrastructure"), false, false),
            Err(Phase1Error::Infrastructure(_))
        ));
        assert_eq!(command_outcome(None, false, false).unwrap().exit_code, 1);
        assert_eq!(command_outcome(None, true, false).unwrap().exit_code, 3);
        assert_eq!(command_outcome(None, true, true).unwrap().exit_code, 0);
    }

    #[test]
    fn structured_energy_import_validates_identity_workload_and_measurement() {
        let directory = test_directory("energy");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("energy.json");
        fs::write(
            &path,
            br#"{
                "schema_version": 1,
                "collector": "powermetrics-import",
                "source": "manual-lab-run",
                "measurement_duration_seconds": 60.0,
                "hardware_identity": "test-hardware",
                "workload": "fault_matrix",
                "rack_count": 2,
                "frame_count": 128,
                "energy_joules": 12.5
            }"#,
        )
        .unwrap();
        let valid =
            load_energy_evidence(Some(&path), "fault_matrix", 2, 128, "test-hardware").unwrap();
        assert!(valid.imported);
        let invalid =
            load_energy_evidence(Some(&path), "fault_matrix", 4, 128, "test-hardware").unwrap();
        assert!(!invalid.imported);
        assert!(
            invalid
                .validation_errors
                .iter()
                .any(|error| error.contains("rack_count"))
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn artifact_publication_commits_manifest_last_with_matching_report_id() {
        let directory = test_directory("artifacts");
        publish_artifact_set(
            &directory,
            "test-report",
            "first",
            br#"{"report_id":"first"}"#,
            b"report_id: first\n",
            br#"{"report_id":"first"}"#,
        )
        .unwrap();
        publish_artifact_set(
            &directory,
            "test-report",
            "second",
            br#"{"report_id":"second"}"#,
            b"report_id: second\n",
            br#"{"report_id":"second"}"#,
        )
        .unwrap();
        let manifest: Value =
            serde_json::from_slice(&fs::read(directory.join("test-report.manifest.json")).unwrap())
                .unwrap();
        let json: Value =
            serde_json::from_slice(&fs::read(directory.join("test-report.json")).unwrap()).unwrap();
        assert_eq!(manifest["report_id"], "second");
        assert_eq!(json["report_id"], manifest["report_id"]);
        assert!(
            fs::read_to_string(directory.join("test-report.md"))
                .unwrap()
                .contains("report_id: second")
        );
        assert!(fs::read_dir(&directory).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn cargo_json_artifact_resolution_uses_reported_executable() {
        let messages = br#"{"reason":"compiler-artifact","target":{"name":"sp-plugin-worker"},"executable":"/custom/target/debug/sp-plugin-worker"}
{"reason":"build-finished","success":true}
"#;
        let (executable, diagnostics) = worker_artifact_from_cargo_messages(messages);
        assert_eq!(
            executable,
            Some(PathBuf::from("/custom/target/debug/sp-plugin-worker"))
        );
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn graceful_shutdown_and_already_exited_reap_paths_preserve_status() {
        let mut child = Command::new("sh")
            .args(["-c", "read _line; exit 0"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let control = child.stdin.take();
        let mut worker = WorkerProcess { child, control };
        assert!(worker.stop_and_reap().unwrap().success());

        let mut child = Command::new("sh")
            .args(["-c", "exit 7"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let control = child.stdin.take();
        thread::sleep(Duration::from_millis(20));
        let mut worker = WorkerProcess { child, control };
        assert_eq!(worker.stop_and_reap().unwrap().code(), Some(7));

        let mut child = Command::new("sh")
            .args(["-c", "exit 9"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let control = child.stdin.take();
        thread::sleep(Duration::from_millis(20));
        let message = failed_startup_message(child, control, 4, "readiness pipe EOF");
        assert!(message.contains("already exited"));
        assert!(message.contains('9'));
    }

    // Instrumented coverage builds alter scheduler timing, so this deadline-sensitive process
    // test is exercised by the ordinary nextest gate instead of being misclassified by coverage.
    #[cfg(all(target_os = "macos", target_arch = "aarch64", not(coverage)))]
    #[test]
    fn real_process_fault_cases_prove_classification_recovery_and_isolation() {
        let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap();
        let worker = build_worker(workspace_root, true).unwrap();
        let clock = MonotonicClock::new().unwrap();
        let options = FaultMatrixOptions {
            rack_count: 2,
            frame_count: 256,
            output_directory: test_directory("unused-real-smoke"),
            energy_evidence_path: None,
            require_energy_evidence: false,
        };
        let period = block_period(options.frame_count);
        for (index, case) in FaultCase::ALL.into_iter().enumerate() {
            let trial = run_fault_case(case, index, &options, &worker, clock, period).unwrap();
            assert!(trial.fault_passed, "{} classification", case.id());
            assert!(
                trial
                    .unaffected_rack_isolation_before_recovery
                    .iter()
                    .all(|entry| entry.original == entry.final_identity)
            );
            assert!(
                trial
                    .unaffected_rack_isolation_after_recovery
                    .iter()
                    .all(|entry| entry.original == entry.final_identity)
            );
            assert!(
                trial
                    .recovery
                    .as_ref()
                    .is_none_or(|recovery| recovery.accepted_completion)
            );
        }
    }

    #[test]
    fn fault_case_classification_is_stable() {
        assert_eq!(
            FaultCase::MalformedCompletion.expected_fallback(),
            Some(FallbackReason::MalformedCompletion)
        );
        assert_eq!(FaultCase::DelayBeforeClaimTimely.expected_fallback(), None);
        assert_eq!(
            FaultCase::StaleSequenceCompletion.injection_layer(),
            "worker_sp_test_support_protocol_test_hook"
        );
        assert_eq!(
            FaultCase::StaleGenerationCompletion.injection_layer(),
            "xtask_gate_adapter_boundary"
        );
    }
}
