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
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use serde_json::Value;
use sp_engine::{FallbackReason, GateOutcome, RackGate, RackGateState, WorkerObservation};
use sp_shared_memory::{
    BLOCK_SLOT_COUNT, BlockRequest, BlockTicket, BlockTiming, ProtocolError, SlotState,
};
use sp_shared_memory_macos::{
    MonotonicClock, ProcessResourceUsage, SharedMemoryRegion, child_process_resource_usage,
    current_process_resource_usage,
};
use sp_test_support::{FaultConfiguration, FaultMode};

const SAMPLE_RATE_HZ: u32 = 48_000;
const WORKER_READY: &str = "ready";
const WORKER_READINESS_TIMEOUT: Duration = Duration::from_secs(5);
const WORKER_GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(250);
const WORKER_EXIT_POLL_INTERVAL: Duration = Duration::from_millis(5);
const HISTOGRAM_MAX_MICROS: usize = 10_000;
const HISTOGRAM_BUCKET_COUNT: usize = HISTOGRAM_MAX_MICROS + 1;
const REPORT_VERSION: u32 = 1;
const FAULT_TRIGGER_SEQUENCE: u64 = 2;

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
    synthetic_callback_work: FixedHistogram,
}

impl TimingHistograms {
    fn observe_timing(&mut self, timing: BlockTiming, clock: MonotonicClock) {
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
    }

    fn observe_callback_work(&mut self, duration: Duration) {
        self.synthetic_callback_work.observe(duration);
    }

    fn merge(&mut self, other: &Self) {
        self.request_to_claim.merge(&other.request_to_claim);
        self.claim_to_completion.merge(&other.claim_to_completion);
        self.request_to_completion
            .merge(&other.request_to_completion);
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
    let acceptance_passed =
        infrastructure_error.is_none() && acceptance_failure.is_none() && timing_thresholds.passed;
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
    let baseline_passed = baseline_block.accepted_racks == options.rack_count
        && harness
            .racks
            .iter()
            .all(|rack| matches!(rack.gate.state(), RackGateState::Awaiting { .. }))
        && baseline_timing_thresholds.passed;

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

pub(crate) fn start_noop_harness(
    rack_count: usize,
    generation_seed: u64,
    worker: &Path,
    period: Duration,
) -> Result<Harness, String> {
    start_harness(rack_count, generation_seed, worker, FaultCase::None, period)
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
    let region = SharedMemoryRegion::create(generation)
        .map_err(|error| format!("could not create bank for rack {index}: {error}"))?;
    let worker_id = u32::try_from(index + 1).map_err(|_| "rack worker ID overflows u32")?;
    let mut process = start_worker(worker, region.name(), worker_id, fault)?;
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

pub(crate) struct RackHarness {
    index: usize,
    generation: u64,
    region: SharedMemoryRegion,
    worker: Option<WorkerProcess>,
    gate: RackGate,
    live: Option<LiveRequest>,
    original_identity: RackIdentity,
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

fn start_worker(
    executable: &Path,
    bank_name: &str,
    worker_id: u32,
    fault: FaultConfiguration,
) -> Result<WorkerProcess, String> {
    let arguments = [
        "--feasibility-bank".to_owned(),
        bank_name.to_owned(),
        "--worker-id".to_owned(),
        worker_id.to_string(),
        "--fault-mode".to_owned(),
        fault.mode.as_str().to_owned(),
        "--fault-trigger-sequence".to_owned(),
        fault.trigger_request_sequence.to_string(),
        "--fault-delay-micros".to_owned(),
        fault.fault_delay.as_micros().to_string(),
        "--work-duration-micros".to_owned(),
        fault.work_duration.as_micros().to_string(),
    ];
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

/// Runs one observe/dispatch cycle without pacing sleeps. Intended for a device callback.
pub(crate) fn process_device_callback_block(
    harness: &mut Harness,
    block_index: u64,
    request: BlockRequest,
    clock: MonotonicClock,
    timing: &mut TimingHistograms,
) -> Result<DeviceBlockCounters, String> {
    let result = run_synthetic_block(
        harness,
        block_index,
        request,
        clock,
        Duration::ZERO,
        true,
        FaultCase::None,
        timing,
    )?;
    Ok(DeviceBlockCounters {
        accepted_racks: result.accepted_racks,
        deadline_misses: result.counters.deadline_misses,
        fallback_events: result.counters.fallback_events,
        protocol_faults: result.counters.protocol_faults,
        worker_exits: result.counters.worker_exits,
    })
}

/// Aggregate counters from a device-callback block.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct DeviceBlockCounters {
    pub(crate) accepted_racks: usize,
    pub(crate) deadline_misses: u64,
    pub(crate) fallback_events: u64,
    pub(crate) protocol_faults: u64,
    pub(crate) worker_exits: u64,
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
                        rack.gate.deadline_expired(block_index)
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
            timing.observe_timing(snapshot.timing, clock);
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
            timing.observe_timing(block_timing, clock);
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
            timing.observe_timing(block_timing, clock);
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
    baseline_timing_thresholds: TimingThresholds,
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
    let manifest = serde_json::to_vec_pretty(&ArtifactManifest {
        report_version: report.report_version,
        report_id: &report.report_id,
        json: "ipc-feasibility.json",
        markdown: "ipc-feasibility.md",
    })
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
    let manifest = serde_json::to_vec_pretty(&ArtifactManifest {
        report_version: report.report_version,
        report_id: &report.report_id,
        json: "ipc-matrix.json",
        markdown: "ipc-matrix.md",
    })
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
    let manifest = serde_json::to_vec_pretty(&ArtifactManifest {
        report_version: report.report_version,
        report_id: &report.report_id,
        json: "fault-matrix.json",
        markdown: "fault-matrix.md",
    })
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
    writeln!(markdown, "## Configuration and thresholds").expect("String writes cannot fail");
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
        "- timing passed: {}; dispatch p99.99={} us (<150), max={} us (<400); callback p99.9={} us (<70% period), p99.99={} us (<80% period), max={} us (<period)",
        report.timing_thresholds.passed,
        report.timing_thresholds.dispatch_p9999_micros,
        report.timing_thresholds.dispatch_max_micros,
        report.timing_thresholds.callback_p999_micros,
        report.timing_thresholds.callback_p9999_micros,
        report.timing_thresholds.callback_max_micros,
    )
    .expect("String writes cannot fail");
    markdown.push_str(&histogram_markdown(
        "timing histograms",
        &report.timing_histograms,
    ));
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
        "  wake p99.99 {}; processing p99.99 {}; total p99.99 {}, max {}; callback p99.9 {}, p99.99 {}, max {} [{}]",
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
        if report.timing_thresholds.passed {
            "within target"
        } else {
            "outside target"
        },
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
    let output = command
        .current_dir(workspace_root)
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
        .request_to_completion
        .percentile_upper_bound(9_999, 10_000);
    let dispatch_max = timing.request_to_completion.max_duration();
    let callback_p999 = timing
        .synthetic_callback_work
        .percentile_upper_bound(999, 1_000);
    let callback_p9999 = timing
        .synthetic_callback_work
        .percentile_upper_bound(9_999, 10_000);
    let callback_max = timing.synthetic_callback_work.max_duration();
    let passed = timing.request_to_completion.sample_count() > 0
        && timing.synthetic_callback_work.sample_count() > 0
        && dispatch_p9999 < Duration::from_micros(150)
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
    fn timing_thresholds_are_strict_at_dispatch_boundaries() {
        let period = Duration::from_millis(10);
        let mut timing = TimingHistograms::default();
        timing
            .request_to_completion
            .observe(Duration::from_micros(150));
        timing
            .synthetic_callback_work
            .observe(Duration::from_micros(1));
        assert!(!timing_thresholds(&timing, period).passed);

        let mut timing = TimingHistograms::default();
        timing
            .request_to_completion
            .observe(Duration::from_micros(149));
        timing
            .synthetic_callback_work
            .observe(Duration::from_micros(1));
        assert!(timing_thresholds(&timing, period).passed);
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

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
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
