//! Desktop application composition root.
//!
//! All plug-in discovery and hosting is driven from this process, but all third-party code stays
//! in the scanner or one retained worker process per rack.

#[cfg(target_os = "macos")]
mod engine;
mod headless;
mod maintenance;
#[cfg(test)]
mod maintenance_tests;
mod ui;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    time::{Duration, Instant},
};

use eframe::egui;
use sp_protocol::{
    control::{
        BankIdentity, ControlOperation, ControlResponse, ControlResponseStatus, ControlTarget,
        RackIdentity, SlotIdentity,
    },
    payload::{
        Bypass, ControlPayloadCodec, EditorPosition, MAX_PARAMETER_BATCH_SIZE,
        ParameterId as WireParameterId, ParameterIds, ParameterValues, ParameterWrite,
        RackTopology as WireRackTopology, SlotOrder, StateRestore,
    },
};
use sp_session::{
    CapturedPluginState, EditorPreviewFile, PluginActivationMetadata, PluginActivationState,
    PluginStateMetadata, SessionController,
};
use sp_shared_memory_macos::{
    PARAMETER_FEEDBACK_CAPACITY_PER_SLOT, RackRecoverySignal, RackRecoveryState, RestartCursor,
    RestartSnapshot, SharedMemoryRegion, reset_reaped_region,
};
use sp_supervisor::{
    BundleFingerprint, HelperKind, HelperLaunch, HelperStatus, PersistentQuarantine, PluginCatalog,
    PluginFailureKind, ProcessSupervisor, Scanner, WorkerControlLaunch, WorkerControlSession,
};

use ui::LiveRackApp;

const QUARANTINE_THRESHOLD: u32 = 3;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);
const RAPID_RESTART_WINDOW: Duration = Duration::from_secs(10);
const RAPID_RESTART_LIMIT: u8 = 3;
const WORKER_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(2);
/// A rack's editor pictures are polled at most this often.
const EDITOR_PREVIEW_POLL_INTERVAL: Duration = Duration::from_secs(1);
const MAX_PENDING_PARAMETER_UPDATES: usize =
    sp_model::MAX_RACKS * sp_model::MAX_SLOTS_PER_RACK * PARAMETER_FEEDBACK_CAPACITY_PER_SLOT;
/// VST3 `kReloadComponent | kIoChanged` need a fresh worker. `kLatencyChanged` does not: the
/// worker republishes latency every loop and the mixer retunes dry delay without a dropout.
/// Replacing the worker for it looped forever with plug-ins that report latency on activation.
const PLANNED_RESTART_FLAGS: u32 = 1 | 2;

fn editor_response_requires_recovery(status: Option<ControlResponseStatus>) -> bool {
    !matches!(
        status,
        Some(
            ControlResponseStatus::Ok
                | ControlResponseStatus::Failed
                | ControlResponseStatus::Rejected
                | ControlResponseStatus::Unsupported
        )
    )
}

struct HeartbeatWatch {
    last_tick: u64,
    observed_at: Instant,
}

impl HeartbeatWatch {
    fn new(now: Instant) -> Self {
        Self {
            last_tick: 0,
            observed_at: now,
        }
    }

    fn stalled(&mut self, tick: u64, now: Instant) -> bool {
        if tick != 0 && tick != self.last_tick {
            self.last_tick = tick;
            self.observed_at = now;
            return false;
        }
        now.saturating_duration_since(self.observed_at) >= WORKER_HEARTBEAT_TIMEOUT
    }
}

/// What the host knows about one slot's native editor window.
#[derive(Clone, Copy, Default)]
struct EditorWindowState {
    /// An `OpenNativeEditor` request for this slot is in flight.
    opening: bool,
    /// The worker last reported the window open, or its open request succeeded since.
    open: bool,
    /// Sequence of the newest picture received from the current worker process.
    preview_sequence: u64,
}

#[derive(Default)]
struct RestartBudget {
    window_started: Option<Instant>,
    failures: u8,
}

impl RestartBudget {
    fn allows_restart(&mut self, now: Instant) -> bool {
        if self
            .window_started
            .is_none_or(|started| now.saturating_duration_since(started) >= RAPID_RESTART_WINDOW)
        {
            self.window_started = Some(now);
            self.failures = 0;
        }
        self.failures = self.failures.saturating_add(1);
        self.failures <= RAPID_RESTART_LIMIT
    }
}

/// Control-plane state owned by the desktop product, never by the audio callback.
pub(crate) struct ProductRuntime {
    app_support: PathBuf,
    scanner: Scanner,
    helpers: HelperBinaries,
    processes: ProcessSupervisor,
    workers: BTreeMap<usize, RetainedWorker>,
    /// Racks with an outstanding native-editor control request.
    editor_requests: BTreeSet<usize>,
    diagnostics: Vec<String>,
    pending_parameter_updates: BTreeMap<(usize, usize, u32), MirroredParameterUpdate>,
    maintenance_events_tx: Sender<maintenance::Event>,
    maintenance_events_rx: Receiver<maintenance::Event>,
    next_maintenance_task_id: u64,
    cancelled_banks: Vec<CancelledBanks>,
    /// Workers removed by a live layout change, kept until the callback stops reading them.
    retiring_workers: Vec<RetainedWorker>,
}

/// How one rack position is produced by a live layout change.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RackPlan {
    /// The rack's worker previously at this position continues unchanged.
    Keep(usize),
    /// Start a new worker; `Some` replaces the rack previously at that position. The rack
    /// fades to dry, then fades to the new worker once it delivers three valid blocks. Both
    /// fades last 16 samples.
    Rebuild(Option<usize>),
}

/// A plug-in slot change the running worker applies in place, without restarting the rack.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SlotEdit {
    /// A plug-in was appended at this slot. It processes its input beside the chain, output
    /// ignored, until it produces three good blocks; then it fades in over 16 samples.
    Add(usize),
    /// This slot was removed; later slots moved up by one.
    Remove(usize),
    /// Two slots swapped places.
    Swap(usize, usize),
}

struct CancelledBanks {
    banks: [SharedMemoryRegion; 2],
    process_ids: Vec<u64>,
}

#[derive(Clone)]
pub(crate) struct MirroredParameterUpdate {
    pub(crate) rack_index: usize,
    pub(crate) slot_index: usize,
    pub(crate) rack_id: sp_model::RackId,
    pub(crate) slot_id: sp_model::PluginInstanceId,
    pub(crate) fingerprint: String,
    pub(crate) class_id: String,
    pub(crate) bank_generation: u64,
    pub(crate) parameter_id: u32,
    pub(crate) value: sp_model::NormalizedValue,
}

#[derive(Default)]
struct ParameterFeedbackCursor {
    epoch: u64,
    revision: u64,
    overflow_count: u64,
    last_values: BTreeMap<u32, u32>,
    known_ids: BTreeSet<u32>,
    writable_ids: BTreeSet<u32>,
}

struct AcceptedParameterFeedback {
    values: Vec<(u32, sp_model::NormalizedValue)>,
    incomplete: bool,
}

struct PlannedMaintenance {
    flags: u32,
    requests: Vec<RestartSnapshot>,
    terminal: bool,
    failed: bool,
    task_id: Option<u64>,
    cancel: Option<Arc<AtomicBool>>,
    old_pid: Option<u64>,
    new_pid: Option<u64>,
    ready: Option<maintenance::Ready>,
}

impl ParameterFeedbackCursor {
    fn new(parameters: &[sp_model::PluginParameterMetadata]) -> Self {
        Self {
            known_ids: parameters.iter().map(|parameter| parameter.id).collect(),
            writable_ids: parameters
                .iter()
                .filter(|parameter| !parameter.read_only)
                .map(|parameter| parameter.id)
                .collect(),
            ..Self::default()
        }
    }

    fn accept(
        &mut self,
        epoch: u64,
        revision: u64,
        overflow_count: u64,
        values: &[(u32, f64)],
    ) -> AcceptedParameterFeedback {
        if self.epoch != epoch {
            self.last_values.clear();
            self.overflow_count = 0;
        }
        let mut incomplete = overflow_count > self.overflow_count;
        self.epoch = epoch;
        self.revision = revision;
        self.overflow_count = overflow_count;
        let mut changed = Vec::new();
        for &(id, value) in values {
            if !self.known_ids.contains(&id) {
                incomplete = true;
                continue;
            }
            if !self.writable_ids.contains(&id) {
                continue;
            }
            if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                incomplete = true;
                continue;
            }
            #[allow(
                clippy::cast_possible_truncation,
                reason = "validated normalized value is bounded to 0.0..=1.0"
            )]
            let Ok(value) = sp_model::NormalizedValue::new(value as f32) else {
                incomplete = true;
                continue;
            };
            if self.last_values.get(&id) == Some(&value.get().to_bits()) {
                continue;
            }
            self.last_values.insert(id, value.get().to_bits());
            changed.push((id, value));
        }
        AcceptedParameterFeedback {
            values: changed,
            incomplete,
        }
    }
}

struct HelperBinaries {
    scanner: PathBuf,
    worker: PathBuf,
}

struct RetainedWorker {
    /// Rack index in this worker's control and bank identity. It stays fixed when the rack
    /// moves to another position during a live layout change.
    wire_index: usize,
    fingerprint: BundleFingerprint,
    /// Both host-side mappings remain alive across alternating worker generations.
    banks: [SharedMemoryRegion; 2],
    active_bank_index: usize,
    session: Option<WorkerControlSession>,
    parameters: Vec<Vec<sp_model::PluginParameterMetadata>>,
    parameter_feedback: Vec<ParameterFeedbackCursor>,
    restart_cursors: Vec<RestartCursor>,
    loaded_slots: Vec<bool>,
    rack: sp_model::Rack,
    saved_states: Vec<Option<CapturedPluginState>>,
    recovery: Arc<RackRecoverySignal>,
    recovery_failed: bool,
    recovery_recorded: bool,
    restart_budget: RestartBudget,
    suspected_slot: Option<usize>,
    latency_samples: u32,
    restart_flags: u32,
    parameter_mirror_incomplete: bool,
    maintenance: Option<PlannedMaintenance>,
    maintenance_budget: RestartBudget,
    heartbeat: HeartbeatWatch,
    /// Replacements by crash recovery or planned maintenance since this worker was loaded.
    restarts: u32,
    /// Per-slot editor windows of the current worker process.
    editors: Vec<EditorWindowState>,
    next_preview_poll: Instant,
}

impl RetainedWorker {
    /// Sends `edit` to the running worker and updates the per-slot bookkeeping to match
    /// `rack`'s new slot order. `candidate` is the resolved plug-in for [`SlotEdit::Add`].
    fn apply_slot_edit(
        &mut self,
        rack: &sp_model::Rack,
        edit: SlotEdit,
        candidate: Option<ResolvedSlot>,
    ) -> Result<(), String> {
        let session = self.session.as_mut().ok_or("rack worker is recovering")?;
        match edit {
            SlotEdit::Add(slot) => {
                let candidate = candidate.ok_or("added plug-in is not in the catalog")?;
                let payload = wire_slot_configuration(slot, &candidate)
                    .encode()
                    .map_err(|error| error.to_string())?;
                request_ok(
                    session,
                    ControlOperation::LoadPlugin,
                    Some(slot_identity(slot)?),
                    &payload,
                )?;
                self.parameter_feedback
                    .push(ParameterFeedbackCursor::new(&candidate.parameters));
                self.parameters.push(candidate.parameters);
                self.restart_cursors.push(RestartCursor::default());
                self.loaded_slots.push(true);
                self.saved_states.push(None);
                self.editors.push(EditorWindowState::default());
            }
            SlotEdit::Remove(slot) => {
                let last = self.loaded_slots.len() - 1;
                request_ok(
                    session,
                    ControlOperation::UnloadSlot,
                    Some(slot_identity(slot)?),
                    &[],
                )?;
                if slot < last {
                    // Move each later plug-in up one slot.
                    let [first, end] =
                        [slot, last].map(|slot| u8::try_from(slot).expect("rack slot fits u8"));
                    let payload = SlotOrder {
                        current: (first..=end).collect(),
                        order: (first + 1..=end).chain([first]).collect(),
                    }
                    .encode()
                    .map_err(|error| error.to_string())?;
                    request_ok(session, ControlOperation::ReorderRack, None, &payload)?;
                }
                self.parameters.remove(slot);
                self.parameter_feedback.remove(slot);
                self.restart_cursors.remove(slot);
                self.loaded_slots.remove(slot);
                self.saved_states.remove(slot);
                self.editors.remove(slot);
            }
            SlotEdit::Swap(a, b) => {
                let [a_wire, b_wire] =
                    [a, b].map(|slot| u8::try_from(slot).expect("rack slot fits u8"));
                let payload = SlotOrder {
                    current: vec![a_wire, b_wire],
                    order: vec![b_wire, a_wire],
                }
                .encode()
                .map_err(|error| error.to_string())?;
                request_ok(session, ControlOperation::ReorderRack, None, &payload)?;
                self.parameters.swap(a, b);
                self.parameter_feedback.swap(a, b);
                self.restart_cursors.swap(a, b);
                self.loaded_slots.swap(a, b);
                self.saved_states.swap(a, b);
                self.editors.swap(a, b);
            }
        }
        self.rack = rack.clone();
        Ok(())
    }
}

pub(crate) struct LaunchedWorker {
    fingerprint: BundleFingerprint,
    session: WorkerControlSession,
    parameters: Vec<Vec<sp_model::PluginParameterMetadata>>,
    loaded_slots: Vec<bool>,
}

#[derive(Clone)]
pub(crate) struct ResolvedSlot {
    bundle: PathBuf,
    fingerprint: BundleFingerprint,
    class_id: String,
    parameters: Vec<sp_model::PluginParameterMetadata>,
    input_channels: u8,
    output_channels: u8,
    event_input_active: bool,
    /// The model slot feeds the plug-in's aux input.
    sidechain: bool,
}

/// One supported scanner result shown by the in-app plug-in browser.
#[derive(Clone)]
pub(crate) struct CatalogPlugin {
    pub(crate) descriptor: sp_model::PluginDescriptor,
    pub(crate) parameters: sp_model::NormalizedParameters,
    pub(crate) sidechain_capable: bool,
}

/// A cataloged plug-in this machine cannot load.
pub(crate) struct UnavailablePlugin {
    pub(crate) name: String,
    pub(crate) vendor: String,
    /// Why it cannot load, as shown in the picker and the catalog table.
    pub(crate) reason: &'static str,
}

pub(crate) struct QuarantineEntry {
    pub(crate) fingerprint: BundleFingerprint,
    pub(crate) name: String,
    pub(crate) failures: u32,
}

/// Screen position for a newly opened editor window, top-left origin, in points.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EditorPlacement {
    pub(crate) left: f32,
    pub(crate) top: f32,
}

/// One rack's callback-owned audio bank pair and its recovery handshake.
type AudioBankMapping = (
    usize,
    [SharedMemoryRegion; 2],
    usize,
    Arc<RackRecoverySignal>,
);
type StoppedRackOwnership<'a> = dyn Fn(usize, &Arc<RackRecoverySignal>) -> bool + 'a;

impl ProductRuntime {
    fn open(app_support: PathBuf) -> Result<Self, String> {
        fs::create_dir_all(&app_support)
            .map_err(|error| format!("cannot create Application Support state: {error}"))?;
        let catalog = PluginCatalog::open(app_support.join("plugin-catalog.json"))
            .map_err(|error| format!("cannot open plug-in catalog: {error}"))?;
        let quarantine =
            PersistentQuarantine::open(app_support.join("quarantine.json"), QUARANTINE_THRESHOLD)
                .map_err(|error| format!("cannot open plug-in quarantine: {error}"))?;
        let helpers = HelperBinaries::resolve()?;
        let (maintenance_events_tx, maintenance_events_rx) = mpsc::channel();
        Ok(Self {
            scanner: Scanner::with_default_timeout(&helpers.scanner, catalog)
                .with_quarantine(quarantine),
            helpers,
            processes: ProcessSupervisor::new(),
            workers: BTreeMap::new(),
            editor_requests: BTreeSet::new(),
            diagnostics: Vec::new(),
            pending_parameter_updates: BTreeMap::new(),
            maintenance_events_tx,
            maintenance_events_rx,
            next_maintenance_task_id: 1,
            cancelled_banks: Vec::new(),
            retiring_workers: Vec::new(),
            app_support,
        })
    }

    pub(crate) fn scan_standard_locations(&mut self, full_rescan: bool) -> Result<usize, String> {
        if full_rescan {
            self.scanner
                .clear_catalog()
                .map_err(|error| error.to_string())?;
        }
        let scans = self
            .scanner
            .scan_standard_locations(std::env::var_os("HOME").as_deref().map(Path::new))
            .map_err(|error| format!("VST3 scan failed: {error}"))?;
        self.note(format!("scanned {} VST3 bundles", scans.len()));
        Ok(scans.len())
    }

    pub(crate) fn catalog_count(&self) -> usize {
        self.scanner.catalog().entries().count()
    }

    pub(crate) fn catalog_plugins(&self) -> Vec<CatalogPlugin> {
        self.scanner
            .catalog()
            .entries()
            .filter(|entry| entry.is_supported())
            .flat_map(|entry| {
                entry.metadata.classes.iter().map(|class| {
                    let values = class
                        .parameters
                        .iter()
                        .filter_map(|parameter| {
                            Some((
                                sp_model::ParameterId(parameter.id.to_string()),
                                #[allow(
                                    clippy::cast_possible_truncation,
                                    reason = "normalized defaults are bounded to 0.0..=1.0"
                                )]
                                sp_model::NormalizedValue::new(
                                    parameter.default_normalized.unwrap_or(0.0) as f32,
                                )
                                .ok()?,
                            ))
                        })
                        .collect();
                    CatalogPlugin {
                        descriptor: sp_model::PluginDescriptor {
                            identity: class.identity.clone(),
                            fingerprint: sp_model::PluginFingerprint {
                                algorithm: entry.fingerprint.algorithm.clone(),
                                digest: entry.fingerprint.digest.clone(),
                                plugin_version: class.version.clone(),
                            },
                        },
                        parameters: sp_model::NormalizedParameters { values },
                        sidechain_capable: class.sidechain_capable,
                    }
                })
            })
            .collect()
    }

    /// Catalog entries the scanner found but this machine cannot load.
    pub(crate) fn unavailable_plugins(&self) -> Vec<UnavailablePlugin> {
        self.scanner
            .catalog()
            .entries()
            .filter(|entry| !entry.is_supported())
            .map(|entry| {
                let class = entry.metadata.classes.first();
                UnavailablePlugin {
                    name: class.map_or_else(
                        || {
                            entry
                                .bundle
                                .file_stem()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .into_owned()
                        },
                        |class| class.identity.name.clone(),
                    ),
                    vendor: class.map_or_else(String::new, |class| class.identity.vendor.clone()),
                    reason: match (entry.metadata.architecture, entry.metadata.outcome) {
                        (sp_model::PluginArchitecture::X86_64, _)
                        | (_, sp_model::PluginScanOutcome::UnsupportedArchitecture) => "Intel only",
                        (_, sp_model::PluginScanOutcome::TimedOut) => "scan timed out",
                        (_, sp_model::PluginScanOutcome::Crashed) => "scan crashed",
                        (_, sp_model::PluginScanOutcome::InvalidBundle) => "invalid bundle",
                        (_, sp_model::PluginScanOutcome::SdkError) => "failed to load",
                        _ => "unreadable",
                    },
                }
            })
            .collect()
    }

    /// Catalog entries scanned before sidechain detection; a rescan refreshes them.
    pub(crate) fn stale_catalog_entries(&self) -> usize {
        self.scanner
            .catalog()
            .entries()
            .filter(|entry| entry.metadata.version != sp_model::PLUGIN_SCAN_METADATA_VERSION)
            .count()
    }

    pub(crate) fn quarantined_count(&self) -> usize {
        self.scanner
            .quarantine()
            .expect("product scanner has quarantine")
            .table()
            .records()
            .values()
            .filter(|record| record.quarantined)
            .count()
    }

    pub(crate) fn quarantined_plugins(&self) -> Vec<QuarantineEntry> {
        self.scanner
            .quarantine()
            .expect("product scanner has quarantine")
            .table()
            .records()
            .iter()
            .filter(|(_, record)| record.quarantined)
            .map(|(fingerprint, record)| {
                let name = self
                    .scanner
                    .catalog()
                    .entries()
                    .find(|entry| &entry.fingerprint == fingerprint)
                    .map_or_else(
                        || format!("{}:{}", fingerprint.algorithm, fingerprint.digest),
                        |entry| {
                            entry.metadata.classes.first().map_or_else(
                                || {
                                    entry
                                        .bundle
                                        .file_stem()
                                        .unwrap_or_default()
                                        .to_string_lossy()
                                        .into_owned()
                                },
                                |class| class.identity.name.clone(),
                            )
                        },
                    );
                QuarantineEntry {
                    fingerprint: fingerprint.clone(),
                    name,
                    failures: record.failure_count,
                }
            })
            .collect()
    }

    pub(crate) fn retry_quarantined_plugin(
        &mut self,
        fingerprint: &BundleFingerprint,
    ) -> Result<(), String> {
        self.scanner
            .clear_quarantine(fingerprint)
            .map_err(|error| error.to_string())?;
        self.note(format!(
            "allowed plug-in retry: {}:{}",
            fingerprint.algorithm, fingerprint.digest
        ));
        Ok(())
    }

    pub(crate) fn poll(&mut self) {
        self.poll_inner(None);
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn poll_with_stopped_dispatcher(
        &mut self,
        access: Option<&sp_audio_io_macos::StoppedRecoveryAccess<'_>>,
    ) {
        let owns = |rack_index, signal: &Arc<RackRecoverySignal>| {
            access.is_some_and(|access| access.owns(rack_index, signal))
        };
        self.poll_inner(access.map(|_| &owns as &StoppedRackOwnership<'_>));
    }

    fn poll_inner(&mut self, stopped_owns: Option<&StoppedRackOwnership<'_>>) {
        if let Some(owns) = stopped_owns {
            self.service_unattached_recoveries(owns);
        }
        for worker in self.workers.values_mut() {
            if worker.recovery.state() == RackRecoveryState::Idle
                && worker.maintenance.is_some()
                && worker
                    .maintenance
                    .as_ref()
                    .is_some_and(|maintenance| !maintenance.failed)
            {
                worker.maintenance = None;
            }
        }
        self.poll_parameter_feedback();
        for event in self.processes.reap() {
            self.note(format!("worker process event: {event:?}"));
        }
        self.poll_maintenance_events();
        self.finish_ready_maintenance();
        self.cancelled_banks.retain(|entry| {
            !entry
                .process_ids
                .iter()
                .all(|pid| self.processes.is_reaped(*pid))
        });
        for worker in self.workers.values_mut() {
            if worker.recovery.state() == RackRecoveryState::Idle {
                worker.recovery_recorded = false;
            }
        }
        let callback_faults: Vec<usize> = self
            .workers
            .iter()
            .filter_map(|(&rack, worker)| {
                (!worker.recovery_failed
                    && worker.maintenance.is_none()
                    && !worker.recovery_recorded
                    && matches!(
                        worker.recovery.state(),
                        RackRecoveryState::QuiesceRequested | RackRecoveryState::Quiescent
                    ))
                .then_some(rack)
            })
            .collect();
        for rack in callback_faults {
            self.record_worker_fault(rack, PluginFailureKind::WorkerTimeout);
        }
        let now = Instant::now();
        let processes = &self.processes;
        let failed: Vec<(usize, String, PluginFailureKind)> = self
            .workers
            .iter_mut()
            .filter_map(|(&rack, worker)| {
                if worker.recovery_failed || worker.recovery.state() != RackRecoveryState::Idle {
                    return None;
                }
                let session = worker.session.as_ref()?;
                if processes.status(session.process_id()) == HelperStatus::Failed {
                    return Some((
                        rack,
                        "worker process exited".to_owned(),
                        PluginFailureKind::WorkerCrash,
                    ));
                }
                let header = &worker.banks[worker.active_bank_index].bank().header;
                worker.latency_samples = header.worker_latency_samples.load(Ordering::Acquire);
                worker.restart_flags |= header.worker_restart_requested.swap(0, Ordering::AcqRel);
                worker
                    .heartbeat
                    .stalled(header.worker_heartbeat(), now)
                    .then(|| {
                        (
                            rack,
                            "worker heartbeat stopped".to_owned(),
                            PluginFailureKind::WorkerHang,
                        )
                    })
            })
            .collect();
        for (rack, detail, kind) in failed {
            self.note(format!("rack {rack} worker fault: {detail}"));
            self.recover_worker(rack, kind);
        }
        self.poll_planned_restarts();
        if let Some(owns) = stopped_owns {
            self.service_unattached_recoveries(owns);
        }
        self.drive_recoveries();
    }

    fn service_unattached_recoveries(&mut self, owns: &StoppedRackOwnership<'_>) {
        for (&rack_index, worker) in &mut self.workers {
            if owns(rack_index, &worker.recovery) {
                continue;
            }
            match worker.recovery.state() {
                RackRecoveryState::QuiesceRequested => {
                    worker.recovery.mark_quiescent();
                }
                RackRecoveryState::ReplacementReady {
                    bank_index,
                    generation,
                } if bank_index == worker.active_bank_index
                    && worker
                        .banks
                        .get(bank_index)
                        .is_some_and(|bank| bank.generation() == generation) =>
                {
                    worker.recovery.mark_replacement_active(bank_index ^ 1);
                }
                RackRecoveryState::RetiredReset {
                    bank_index,
                    generation,
                } if bank_index == (worker.active_bank_index ^ 1)
                    && worker
                        .banks
                        .get(bank_index)
                        .is_some_and(|bank| bank.generation() == generation) =>
                {
                    worker.recovery.complete_retirement();
                }
                _ => {}
            }
        }
    }

    fn maintenance_event_matches(
        &self,
        rack_index: usize,
        task_id: u64,
        generation: u64,
        recovery: &Arc<RackRecoverySignal>,
    ) -> bool {
        self.workers.get(&rack_index).is_some_and(|worker| {
            Arc::ptr_eq(&worker.recovery, recovery)
                && worker.recovery.state() == RackRecoveryState::Quiescent
                && worker.banks[worker.active_bank_index ^ 1].generation() == generation
                && worker.maintenance.as_ref().is_some_and(|maintenance| {
                    maintenance.task_id == Some(task_id)
                        && !maintenance.failed
                        && !maintenance
                            .cancel
                            .as_ref()
                            .is_some_and(|cancel| cancel.load(Ordering::Acquire))
                })
        })
    }

    #[allow(
        clippy::too_many_lines,
        reason = "both asynchronous result variants must validate ownership before changing rack state"
    )]
    fn poll_maintenance_events(&mut self) {
        while let Ok(event) = self.maintenance_events_rx.try_recv() {
            match event {
                maintenance::Event::Spawn {
                    rack_index,
                    task_id,
                    generation,
                    recovery,
                    launch,
                    reply,
                } => {
                    let result = if self
                        .maintenance_event_matches(rack_index, task_id, generation, &recovery)
                    {
                        launch
                            .validate()
                            .map_err(|error| error.to_string())
                            .and_then(|()| {
                                self.processes
                                    .launch(&launch.launch)
                                    .map_err(|error| error.to_string())
                            })
                    } else {
                        Err("stale or cancelled rack maintenance launch".to_owned())
                    };
                    match result {
                        Ok(pid) => {
                            if let Some(maintenance) = self
                                .workers
                                .get_mut(&rack_index)
                                .and_then(|worker| worker.maintenance.as_mut())
                            {
                                maintenance.new_pid = Some(pid);
                                if reply.send(Ok(pid)).is_err() {
                                    let _ = self.processes.request_stop(pid);
                                }
                            } else {
                                let _ = self.processes.request_stop(pid);
                                let _ =
                                    reply.send(Err("rack maintenance was cancelled".to_owned()));
                            }
                        }
                        Err(error) => {
                            let _ = reply.send(Err(error));
                        }
                    }
                }
                maintenance::Event::Finished {
                    rack_index,
                    task_id,
                    generation,
                    recovery,
                    old_session,
                    result,
                } => {
                    if !self.maintenance_event_matches(rack_index, task_id, generation, &recovery) {
                        let _ = self.processes.request_stop(old_session.process_id());
                        if let Ok(ready) = *result {
                            let _ = self
                                .processes
                                .request_stop(ready.launched.session.process_id());
                        }
                        continue;
                    }
                    let old_pid = old_session.process_id();
                    let worker = self
                        .workers
                        .get_mut(&rack_index)
                        .expect("event was validated");
                    let maintenance = worker.maintenance.as_mut().expect("event was validated");
                    maintenance.task_id = None;
                    maintenance.cancel = None;
                    match *result {
                        Err(error) => {
                            worker.session = Some(old_session);
                            maintenance.failed = true;
                            if let Some(pid) = maintenance.new_pid {
                                let _ = self.processes.request_stop(pid);
                            }
                            self.note(format!(
                                "rack {} planned restart failed: {error}; old worker retained and rack remains dry",
                                rack_index + 1
                            ));
                        }
                        Ok(ready) => {
                            if maintenance.new_pid != Some(ready.launched.session.process_id()) {
                                maintenance.failed = true;
                                worker.session = Some(old_session);
                                let _ = self
                                    .processes
                                    .request_stop(ready.launched.session.process_id());
                                self.note(format!(
                                    "rack {} replacement worker identity failed; rack remains dry",
                                    rack_index + 1
                                ));
                            } else if let Err(error) = self.processes.request_stop(old_pid) {
                                maintenance.failed = true;
                                worker.session = Some(old_session);
                                let _ = self
                                    .processes
                                    .request_stop(ready.launched.session.process_id());
                                self.note(format!(
                                    "rack {} could not retire old worker: {error}; rack remains dry",
                                    rack_index + 1
                                ));
                            } else {
                                drop(old_session);
                                maintenance.ready = Some(ready);
                            }
                        }
                    }
                }
            }
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the ready phase installs one complete worker after child reap proof"
    )]
    fn finish_ready_maintenance(&mut self) {
        let ready_racks = self
            .workers
            .iter()
            .filter_map(|(&rack_index, worker)| {
                let maintenance = worker.maintenance.as_ref()?;
                (maintenance.ready.is_some()
                    && maintenance
                        .old_pid
                        .is_some_and(|pid| self.processes.is_reaped(pid)))
                .then_some(rack_index)
            })
            .collect::<Vec<_>>();
        for rack_index in ready_racks {
            let new_pid = self.workers[&rack_index]
                .maintenance
                .as_ref()
                .and_then(|maintenance| maintenance.new_pid);
            if new_pid.is_none_or(|pid| {
                self.processes.is_reaped(pid)
                    || self.processes.status(pid) != HelperStatus::Running
                    || self.workers[&rack_index]
                        .maintenance
                        .as_ref()
                        .and_then(|maintenance| maintenance.ready.as_ref())
                        .is_none_or(|ready| ready.launched.session.process_id() != pid)
            }) {
                let worker = self
                    .workers
                    .get_mut(&rack_index)
                    .expect("ready rack exists");
                let ready = worker
                    .maintenance
                    .as_mut()
                    .expect("ready maintenance exists")
                    .ready
                    .take()
                    .expect("ready result exists");
                let generation = worker.banks[worker.active_bank_index].generation();
                Self::apply_maintenance_rack(
                    worker,
                    rack_index,
                    ready.rack,
                    generation,
                    &mut self.pending_parameter_updates,
                );
                worker.saved_states = ready.states;
                worker.editors = vec![EditorWindowState::default(); worker.rack.slots.len()];
                let maintenance = worker
                    .maintenance
                    .as_mut()
                    .expect("ready maintenance exists");
                maintenance.failed = true;
                maintenance.terminal = true;
                self.note(format!(
                    "rack {} replacement worker exited before handoff; fresh captured state is retained and rack remains dry",
                    rack_index + 1
                ));
                continue;
            }
            let worker = self
                .workers
                .get_mut(&rack_index)
                .expect("ready rack exists");
            let ready = worker
                .maintenance
                .as_mut()
                .expect("ready maintenance exists")
                .ready
                .take()
                .expect("ready result exists");
            let replacement_index = worker.active_bank_index ^ 1;
            let generation = worker.banks[replacement_index].generation();
            Self::apply_maintenance_rack(
                worker,
                rack_index,
                ready.rack,
                generation,
                &mut self.pending_parameter_updates,
            );
            worker.saved_states = ready.states;
            worker.fingerprint = ready.launched.fingerprint;
            worker.session = Some(ready.launched.session);
            worker.parameter_feedback = ready
                .launched
                .parameters
                .iter()
                .map(|parameters| ParameterFeedbackCursor::new(parameters))
                .collect();
            let fulfilled_flags = worker.maintenance.as_ref().map_or(0, |m| m.flags);
            let fulfilled_slots = worker.maintenance.as_ref().map_or(0, |m| m.requests.len());
            worker.restart_flags &= !fulfilled_flags;
            worker.restart_cursors =
                vec![RestartCursor::default(); ready.launched.parameters.len()];
            worker.parameters = ready.launched.parameters;
            worker.loaded_slots = ready.launched.loaded_slots;
            worker.editors = vec![EditorWindowState::default(); worker.rack.slots.len()];
            worker.active_bank_index = replacement_index;
            worker.latency_samples = worker.banks[replacement_index]
                .bank()
                .header
                .worker_latency_samples
                .load(Ordering::Acquire);
            worker.heartbeat = HeartbeatWatch::new(Instant::now());
            if worker
                .recovery
                .publish_replacement(replacement_index, generation)
            {
                worker.restarts = worker.restarts.saturating_add(1);
                self.note(format!(
                    "rack {} planned restart ready on bank {replacement_index} (generation {generation}, {fulfilled_slots} requesting slots)",
                    rack_index + 1
                ));
            } else {
                let maintenance = worker
                    .maintenance
                    .as_mut()
                    .expect("ready maintenance exists");
                maintenance.failed = true;
                maintenance.terminal = true;
                if let Some(pid) = maintenance.new_pid {
                    let _ = self.processes.request_stop(pid);
                }
                self.note(format!(
                    "rack {} planned restart handoff failed; rack remains dry",
                    rack_index + 1
                ));
            }
        }
    }

    fn apply_maintenance_rack(
        worker: &mut RetainedWorker,
        rack_index: usize,
        rack: sp_model::Rack,
        generation: u64,
        pending: &mut BTreeMap<(usize, usize, u32), MirroredParameterUpdate>,
    ) {
        for (slot_index, slot) in rack.slots.iter().enumerate() {
            let Some(old_slot) = worker.rack.slots.get(slot_index) else {
                continue;
            };
            for (id, &value) in &slot.parameters.values {
                if old_slot.parameters.values.get(id) == Some(&value) {
                    continue;
                }
                let Ok(parameter_id) = id.0.parse() else {
                    continue;
                };
                pending.insert(
                    (rack_index, slot_index, parameter_id),
                    MirroredParameterUpdate {
                        rack_index,
                        slot_index,
                        rack_id: rack.id.clone(),
                        slot_id: slot.id.clone(),
                        fingerprint: slot.plugin.fingerprint.digest.clone(),
                        class_id: slot.plugin.identity.unique_id.clone(),
                        bank_generation: generation,
                        parameter_id,
                        value,
                    },
                );
            }
        }
        worker.rack = rack;
    }

    fn poll_planned_restarts(&mut self) {
        let now = Instant::now();
        let mut notices = Vec::new();
        for (&rack_index, worker) in &mut self.workers {
            if worker.maintenance.is_some()
                || worker
                    .maintenance
                    .as_ref()
                    .is_some_and(|maintenance| maintenance.failed)
                || worker.recovery_failed
                || worker.recovery.state() != RackRecoveryState::Idle
                || self.editor_requests.contains(&rack_index)
            {
                continue;
            }
            let Some(session) = worker.session.as_mut() else {
                continue;
            };
            let bank_index = worker.active_bank_index;
            let bank = &worker.banks[bank_index];
            let target = session.client_mut().target();
            if usize::from(target.rack().index()) != worker.wire_index
                || target.rack().generation() != bank.generation()
                || usize::from(target.bank().index()) != bank_index
                || target.bank().generation() != bank.generation()
            {
                continue;
            }
            let mut flags = 0;
            let mut requests = Vec::new();
            for (slot_index, cursor) in worker.restart_cursors.iter_mut().enumerate() {
                if worker.loaded_slots.get(slot_index) != Some(&true) {
                    continue;
                }
                let Some(snapshot) = bank.bank().feedback.restart_snapshot(slot_index, cursor)
                else {
                    continue;
                };
                let requested = snapshot.flags & PLANNED_RESTART_FLAGS;
                if requested == 0 {
                    cursor.ack(&snapshot, 0);
                    continue;
                }
                flags |= requested;
                requests.push(snapshot);
            }
            if flags == 0 || !worker.recovery.request_quiesce() {
                continue;
            }
            worker.restart_flags |= flags;
            let terminal = !worker.maintenance_budget.allows_restart(now);
            worker.maintenance = Some(PlannedMaintenance {
                flags,
                requests,
                terminal,
                failed: false,
                task_id: None,
                cancel: None,
                old_pid: None,
                new_pid: None,
                ready: None,
            });
            notices.push((rack_index, flags, terminal));
        }
        for (rack_index, flags, terminal) in notices {
            if terminal {
                self.note(format!(
                    "rack {} planned restart repeated too often (flags 0x{flags:X}); affected rack remains dry until manually reloaded",
                    rack_index + 1
                ));
            } else {
                self.note(format!(
                    "rack {} plug-in requested lifecycle maintenance (flags 0x{flags:X}); affected rack is temporarily dry",
                    rack_index + 1
                ));
            }
        }
    }

    fn poll_parameter_feedback(&mut self) {
        let pending = &mut self.pending_parameter_updates;
        let mut incomplete_racks = Vec::new();
        for (&rack_index, worker) in &mut self.workers {
            let Some(session) = worker.session.as_mut() else {
                continue;
            };
            let bank_index = worker.active_bank_index;
            let bank = &worker.banks[bank_index];
            let target = session.client_mut().target();
            if usize::from(target.rack().index()) != worker.wire_index
                || target.rack().generation() != bank.generation()
                || usize::from(target.bank().index()) != bank_index
                || target.bank().generation() != bank.generation()
            {
                continue;
            }
            for slot_index in 0..worker.rack.slots.len() {
                if worker.loaded_slots.get(slot_index) != Some(&true) {
                    continue;
                }
                let Some(cursor) = worker.parameter_feedback.get_mut(slot_index) else {
                    continue;
                };
                let Some(snapshot) = bank.bank().feedback.snapshot_changed(
                    slot_index,
                    cursor.epoch,
                    cursor.revision,
                ) else {
                    continue;
                };
                let accepted = cursor.accept(
                    snapshot.epoch,
                    snapshot.revision,
                    snapshot.overflow_count,
                    &snapshot.values,
                );
                let mut incomplete = accepted.incomplete;
                let rack_id = worker.rack.id.clone();
                let slot = &mut worker.rack.slots[slot_index];
                for (parameter_id, value) in accepted.values {
                    let id = sp_model::ParameterId(parameter_id.to_string());
                    if slot.parameters.values.get(&id) == Some(&value) {
                        continue;
                    }
                    slot.parameters.values.insert(id, value);
                    let key = (rack_index, slot_index, parameter_id);
                    if pending.len() >= MAX_PENDING_PARAMETER_UPDATES && !pending.contains_key(&key)
                    {
                        incomplete = true;
                        continue;
                    }
                    pending.insert(
                        key,
                        MirroredParameterUpdate {
                            rack_index,
                            slot_index,
                            rack_id: rack_id.clone(),
                            slot_id: slot.id.clone(),
                            fingerprint: slot.plugin.fingerprint.digest.clone(),
                            class_id: slot.plugin.identity.unique_id.clone(),
                            bank_generation: bank.generation(),
                            parameter_id,
                            value,
                        },
                    );
                }
                if incomplete && !worker.parameter_mirror_incomplete {
                    worker.parameter_mirror_incomplete = true;
                    incomplete_racks.push(rack_index);
                }
            }
        }
        for rack_index in incomplete_racks {
            self.note(format!(
                "rack {} parameter mirror is incomplete; stop and explicitly Save to capture the full plug-in state",
                rack_index + 1
            ));
        }
    }

    pub(crate) fn take_parameter_updates(&mut self) -> Vec<MirroredParameterUpdate> {
        std::mem::take(&mut self.pending_parameter_updates)
            .into_values()
            .filter(|update| {
                self.workers.get(&update.rack_index).is_some_and(|worker| {
                    worker.banks[worker.active_bank_index].generation() >= update.bank_generation
                        && worker.rack.id == update.rack_id
                        && worker
                            .rack
                            .slots
                            .get(update.slot_index)
                            .is_some_and(|slot| {
                                slot.id == update.slot_id
                                    && slot.plugin.fingerprint.digest == update.fingerprint
                                    && slot.plugin.identity.unique_id == update.class_id
                                    && slot.parameters.values.get(&sp_model::ParameterId(
                                        update.parameter_id.to_string(),
                                    )) == Some(&update.value)
                            })
                })
            })
            .collect()
    }

    /// Starts a retained worker and transactionally rebuilds its complete rack topology.
    pub(crate) fn load_rack(
        &mut self,
        rack_index: usize,
        rack: &sp_model::Rack,
        saved_states: &[Option<CapturedPluginState>],
    ) -> Result<(), String> {
        if rack.slots.is_empty() {
            return Err("rack has no plug-in slots to load".to_owned());
        }
        self.pending_parameter_updates
            .retain(|(index, _, _), _| *index != rack_index);
        self.unload_rack(rack_index);
        self.editor_requests.remove(&rack_index);
        if let Some(worker) = self.start_worker(rack_index, rack, saved_states)? {
            self.workers.insert(rack_index, worker);
        }
        Ok(())
    }

    /// Launches a worker for `rack` without touching any running worker. Returns `None` when
    /// every slot is a missing plug-in, so the rack only passes dry audio.
    fn start_worker(
        &mut self,
        rack_index: usize,
        rack: &sp_model::Rack,
        saved_states: &[Option<CapturedPluginState>],
    ) -> Result<Option<RetainedWorker>, String> {
        if rack.slots.is_empty() {
            return Ok(None);
        }
        let candidates = self.resolve_rack_slots(rack)?;
        if candidates.iter().all(Option::is_none) {
            self.note(format!(
                "rack {} contains only missing plug-in placeholders; dry fallback remains active",
                rack_index + 1
            ));
            return Ok(None);
        }
        let generation = self.next_generation(rack_index);
        let banks = [
            SharedMemoryRegion::create(generation)
                .map_err(|error| format!("cannot create active rack bank: {error}"))?,
            SharedMemoryRegion::create(generation.saturating_add(1))
                .map_err(|error| format!("cannot create inactive rack bank: {error}"))?,
        ];
        let launched = self.launch_worker(
            rack_index,
            rack,
            saved_states,
            &banks[0],
            0,
            generation,
            None,
        )?;
        let mut retained_states = saved_states.to_vec();
        retained_states.resize(rack.slots.len(), None);
        let parameter_feedback = launched
            .parameters
            .iter()
            .map(|parameters| ParameterFeedbackCursor::new(parameters))
            .collect();
        let restart_cursors = vec![RestartCursor::default(); launched.parameters.len()];
        let worker = RetainedWorker {
            wire_index: rack_index,
            fingerprint: launched.fingerprint,
            banks,
            active_bank_index: 0,
            session: Some(launched.session),
            parameters: launched.parameters,
            parameter_feedback,
            restart_cursors,
            loaded_slots: launched.loaded_slots,
            rack: rack.clone(),
            saved_states: retained_states,
            recovery: Arc::new(RackRecoverySignal::new()),
            recovery_failed: false,
            recovery_recorded: false,
            restart_budget: RestartBudget::default(),
            suspected_slot: None,
            latency_samples: 0,
            restart_flags: 0,
            parameter_mirror_incomplete: false,
            maintenance: None,
            maintenance_budget: RestartBudget::default(),
            heartbeat: HeartbeatWatch::new(Instant::now()),
            restarts: 0,
            editors: vec![EditorWindowState::default(); rack.slots.len()],
            next_preview_poll: Instant::now(),
        };
        self.note(format!(
            "rack {} worker initialized (generation {generation})",
            rack_index + 1
        ));
        Ok(Some(worker))
    }

    /// Changes the rack layout while audio runs. Racks listed in `plan` with
    /// [`RackPlan::Keep`] continue without interruption; rebuilt and new racks start their
    /// workers first and fade to them over 16 samples. Racks not listed are removed.
    ///
    /// The callback applies the layout at one block boundary. Workers it retires stop after
    /// [`Self::finish_live_topology`] confirms the callback no longer reads their banks.
    #[cfg(target_os = "macos")]
    pub(crate) fn publish_live_topology(
        &mut self,
        control: &mut sp_audio_io_macos::ProductControl,
        model: &sp_model::Session,
        plan: &[RackPlan],
        saved_states: &[Vec<Option<CapturedPluginState>>],
    ) -> Result<(), String> {
        use sp_audio_io_macos::{LaneWorker, PreparedRackLane, TopologyLane};

        if !control.changes_applied() {
            return Err("the previous rack change is still being applied; try again".to_owned());
        }
        if self.workers.values().any(|worker| {
            worker.maintenance.is_some() || worker.recovery.state() != RackRecoveryState::Idle
        }) {
            return Err("a rack is completing a worker handoff; try again shortly".to_owned());
        }
        if !self.editor_requests.is_empty() {
            return Err("a plug-in editor is still opening or closing; try again".to_owned());
        }
        let graph = sp_engine::PreparedGraph::compile(model)
            .map_err(|error| format!("Rack graph is not ready: {error}"))?;

        // Start every new worker before touching live audio. On failure, stop the new workers
        // and leave the running layout unchanged.
        let mut started = BTreeMap::new();
        for (position, (rack, step)) in model.racks.iter().zip(plan).enumerate() {
            if matches!(step, RackPlan::Keep(_)) {
                continue;
            }
            let states = saved_states.get(position).map_or(&[][..], Vec::as_slice);
            match self.start_worker(position, rack, states) {
                Ok(Some(worker)) => {
                    started.insert(position, worker);
                }
                Ok(None) => {}
                Err(error) => {
                    for (_, worker) in started {
                        self.stop_worker(worker);
                    }
                    return Err(format!("Rack {} could not start: {error}", position + 1));
                }
            }
        }

        let mut lanes = Vec::with_capacity(model.racks.len());
        for (position, (rack, step)) in model.racks.iter().zip(plan).enumerate() {
            let from = match step {
                RackPlan::Keep(from) | RackPlan::Rebuild(Some(from)) => Some(*from),
                RackPlan::Rebuild(None) => None,
            };
            let worker = match (step, started.get(&position)) {
                (RackPlan::Keep(from), _) if self.workers.contains_key(from) => LaneWorker::Keep,
                (_, Some(worker)) => {
                    let regions = [
                        SharedMemoryRegion::open(worker.banks[0].name())
                            .map_err(|error| format!("cannot map rack bank for audio: {error}"))?,
                        SharedMemoryRegion::open(worker.banks[1].name())
                            .map_err(|error| format!("cannot map rack bank for audio: {error}"))?,
                    ];
                    LaneWorker::Replace(
                        PreparedRackLane::new(
                            regions,
                            worker.active_bank_index,
                            Arc::clone(&worker.recovery),
                        )
                        .map_err(|error| format!("cannot prepare rack for audio: {error}"))?,
                    )
                }
                _ => LaneWorker::Dry,
            };
            lanes.push(TopologyLane {
                from,
                worker,
                settings: sp_engine::RackSettings {
                    gain: 10.0_f32.powf(rack.gain_db.get() / 20.0),
                    muted: rack.muted,
                    bypassed: rack.bypassed,
                    latency_frames: 0,
                },
                dry_fallback: self.rack_dry_fallback_available(rack),
            });
        }
        if control.publish_topology(model, graph, lanes).is_err() {
            for (_, worker) in started {
                self.stop_worker(worker);
            }
            return Err("the previous rack change is still being applied; try again".to_owned());
        }

        // Reindex retained workers to their new positions. Workers that no longer have a
        // position wait until the callback has released their banks.
        let mut previous = std::mem::take(&mut self.workers);
        for (position, step) in plan.iter().enumerate() {
            if let RackPlan::Keep(from) = step
                && let Some(worker) = previous.remove(from)
            {
                self.workers.insert(position, worker);
            }
        }
        self.workers.extend(started);
        self.retiring_workers.extend(previous.into_values());
        self.pending_parameter_updates.clear();
        Ok(())
    }

    /// Applies `edit` to the running worker of `rack_index`, whose model is now `rack`. Other
    /// slots keep processing throughout. Returns `false` when the edit needs a rebuilt worker
    /// instead: the rack has no running worker, has a missing plug-in, became empty, or the
    /// worker rejected the edit.
    ///
    /// # Errors
    /// Returns an error, changing nothing, while [`Self::publish_live_topology`] would refuse
    /// the layout change that must follow.
    pub(crate) fn edit_slot_live(
        &mut self,
        rack_index: usize,
        rack: &sp_model::Rack,
        edit: SlotEdit,
    ) -> Result<bool, String> {
        if self.workers.values().any(|worker| {
            worker.maintenance.is_some() || worker.recovery.state() != RackRecoveryState::Idle
        }) {
            return Err("a rack is completing a worker handoff; try again shortly".to_owned());
        }
        if !self.editor_requests.is_empty() {
            return Err("a plug-in editor is still opening or closing; try again".to_owned());
        }
        if rack.slots.is_empty() {
            return Ok(false);
        }
        let (candidate, slots_before) = match edit {
            SlotEdit::Add(slot) => {
                let Some(candidate) = self.resolve_rack_slots(rack)?.swap_remove(slot) else {
                    return Ok(false);
                };
                (Some(candidate), rack.slots.len() - 1)
            }
            SlotEdit::Remove(_) => (None, rack.slots.len() + 1),
            SlotEdit::Swap(..) => (None, rack.slots.len()),
        };
        let Some(worker) = self.workers.get_mut(&rack_index) else {
            return Ok(false);
        };
        if worker.loaded_slots.len() != slots_before
            || !worker.loaded_slots.iter().all(|&loaded| loaded)
        {
            return Ok(false);
        }
        if let Err(error) = worker.apply_slot_edit(rack, edit, candidate) {
            self.note(format!(
                "rack {} could not change plug-ins in place ({error}); rebuilding it",
                rack_index + 1
            ));
            return Ok(false);
        }
        Ok(true)
    }

    /// Stops workers retired by a live layout change once the callback has applied it.
    #[cfg(target_os = "macos")]
    pub(crate) fn finish_live_topology(&mut self, control: &mut sp_audio_io_macos::ProductControl) {
        if self.retiring_workers.is_empty() || !control.changes_applied() {
            return;
        }
        self.stop_retiring_workers();
    }

    /// Stops retired workers after the audio callback has stopped.
    pub(crate) fn stop_retiring_workers(&mut self) {
        for worker in std::mem::take(&mut self.retiring_workers) {
            self.stop_worker(worker);
        }
    }

    /// Stops a worker that the callback no longer reads.
    fn stop_worker(&mut self, mut worker: RetainedWorker) {
        let Some(session) = worker.session.take() else {
            return;
        };
        let pid = session.process_id();
        let _ = session.shutdown(&mut self.processes);
        if !self.processes.is_reaped(pid) {
            let _ = self.processes.request_stop(pid);
            self.cancelled_banks.push(CancelledBanks {
                banks: worker.banks,
                process_ids: vec![pid],
            });
        }
    }

    pub(crate) fn unload_rack(&mut self, rack_index: usize) {
        self.pending_parameter_updates
            .retain(|(index, _, _), _| *index != rack_index);
        if let Some(mut worker) = self.workers.remove(&rack_index) {
            if let Some(maintenance) = worker.maintenance.as_mut()
                && maintenance.old_pid.is_some()
            {
                if let Some(cancel) = &maintenance.cancel {
                    cancel.store(true, Ordering::Release);
                }
                let process_ids = [maintenance.old_pid, maintenance.new_pid]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>();
                for &pid in &process_ids {
                    let _ = self.processes.request_stop(pid);
                }
                drop(worker.session.take());
                self.cancelled_banks.push(CancelledBanks {
                    banks: worker.banks,
                    process_ids,
                });
            } else if let Some(session) = worker.session.take() {
                let pid = session.process_id();
                let _ = session.shutdown(&mut self.processes);
                if !self.processes.is_reaped(pid) {
                    let _ = self.processes.request_stop(pid);
                    self.cancelled_banks.push(CancelledBanks {
                        banks: worker.banks,
                        process_ids: vec![pid],
                    });
                }
            }
        }
        self.editor_requests.remove(&rack_index);
    }

    pub(crate) fn unload_all_racks(&mut self) {
        let rack_indices: Vec<_> = self.workers.keys().copied().collect();
        for rack_index in rack_indices {
            self.unload_rack(rack_index);
        }
    }

    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "worker launch names every isolation input and step in one sequence"
    )]
    fn launch_worker(
        &mut self,
        rack_index: usize,
        rack: &sp_model::Rack,
        saved_states: &[Option<CapturedPluginState>],
        bank: &SharedMemoryRegion,
        bank_index: usize,
        generation: u64,
        suspected_slot: Option<usize>,
    ) -> Result<LaunchedWorker, String> {
        let candidates = self.resolve_rack_slots(rack)?;
        let first = candidates
            .iter()
            .flatten()
            .next()
            .ok_or("rack contains no available plug-ins")?;
        for candidate in candidates.iter().flatten() {
            self.scanner.quarantine().expect("product scanner has quarantine")
                .ensure_launch_permitted(&candidate.fingerprint)
                .map_err(|error| {
                    let name = self.scanner.catalog().entries()
                        .find(|entry| entry.fingerprint == candidate.fingerprint)
                        .and_then(|entry| entry.metadata.classes.iter().find(|class| class.identity.unique_id == candidate.class_id))
                        .map_or("Plug-in", |class| class.identity.name.as_str());
                    format!("{name} is quarantined after {} failures. Use Settings → Plug-ins → Allow retry for this plug-in", error.failure_count())
                })?;
        }
        let target = control_target(rack_index, bank_index, generation)?;
        // The bank's exclusive nonce also isolates sockets between host processes.
        let socket = self
            .app_support
            .join(format!("{}.sock", bank.name().trim_start_matches('/')));
        let _ = fs::remove_file(&socket);
        let launch = WorkerControlLaunch {
            launch: HelperLaunch {
                kind: HelperKind::PluginWorker,
                executable: self.helpers.worker.clone(),
                arguments: worker_arguments(
                    bank.name(),
                    &socket,
                    target,
                    &first.bundle,
                    &first.class_id,
                ),
            },
            control_socket: socket,
            target,
            timeout: CONTROL_TIMEOUT,
        };
        let session = WorkerControlSession::launch(&mut self.processes, &launch)
            .map_err(|error| format!("could not launch rack worker: {error}"))?;
        let process_id = session.process_id();
        match configure_worker_session(session, rack, &candidates, saved_states, suspected_slot) {
            Ok(launched) => Ok(launched),
            Err(error) => {
                let _ = self.processes.stop(process_id);
                Err(error)
            }
        }
    }

    pub(crate) fn capture_plugin_states(
        &mut self,
        model: &sp_model::Session,
    ) -> Result<Vec<CapturedPluginState>, String> {
        let mut captures = Vec::new();
        let rack_indices: Vec<_> = self.workers.keys().copied().collect();
        for rack_index in rack_indices {
            let worker = &self.workers[&rack_index];
            let rack = model
                .racks
                .get(rack_index)
                .ok_or("running worker has no matching session rack")?;
            if rack.id != worker.rack.id
                || rack.slots.len() != worker.rack.slots.len()
                || rack
                    .slots
                    .iter()
                    .zip(&worker.rack.slots)
                    .any(|(current, loaded)| {
                        current.id != loaded.id
                            || current.plugin.fingerprint.digest != loaded.plugin.fingerprint.digest
                    })
            {
                return Err(format!(
                    "rack {} changed since its worker loaded; reload it before saving",
                    rack_index + 1
                ));
            }
            captures.extend(self.capture_rack_states(rack_index)?);
        }
        Ok(captures)
    }

    /// Captures the worker's slots without interrupting their audio processing.
    pub(crate) fn capture_rack_states(
        &mut self,
        rack_index: usize,
    ) -> Result<Vec<CapturedPluginState>, String> {
        let Some(worker) = self.workers.get_mut(&rack_index) else {
            return Ok(Vec::new());
        };
        if worker.maintenance.is_some() || worker.recovery.state() != RackRecoveryState::Idle {
            return Err(format!(
                "rack {} is completing a worker handoff",
                rack_index + 1
            ));
        }
        let Some(session) = worker.session.as_mut() else {
            return Err(format!("rack {} worker is recovering", rack_index + 1));
        };
        let mut captures = Vec::new();
        for (slot, model_slot) in worker.rack.slots.iter().enumerate() {
            if worker.loaded_slots.get(slot) != Some(&true) {
                continue;
            }
            // The worker serializes state on its loading thread; the slot keeps processing.
            let state = session
                .client_mut()
                .capture_state(slot_identity(slot)?)
                .map_err(|error| format!("could not capture plug-in state: {error}"))?;
            let fingerprint = model_slot.plugin.fingerprint.digest.clone();
            let captured_state = CapturedPluginState {
                instance_id: model_slot.id.0.clone(),
                component: state.component,
                controller: state.controller,
                metadata: PluginStateMetadata {
                    fingerprint: fingerprint.clone(),
                    capture_schema_version: 1,
                    activation: PluginActivationMetadata {
                        captured_fingerprint: fingerprint,
                        last_known_state: PluginActivationState::Ready,
                        diagnostic: None,
                    },
                    ..PluginStateMetadata::default()
                },
            };
            if let Some(saved) = worker.saved_states.get_mut(slot) {
                *saved = Some(captured_state.clone());
            }
            captures.push(captured_state);
        }
        Ok(captures)
    }

    pub(crate) fn set_slot_bypass(
        &mut self,
        rack_index: usize,
        slot: usize,
        enabled: bool,
    ) -> Result<(), String> {
        let worker = self
            .workers
            .get_mut(&rack_index)
            .ok_or("rack worker is not running")?;
        if worker.maintenance.is_some() || worker.recovery.state() != RackRecoveryState::Idle {
            return Err("rack is waiting for plug-in maintenance".to_owned());
        }
        let session = worker
            .session
            .as_mut()
            .ok_or("rack worker is currently recovering")?;
        let slot_index = slot;
        let slot = slot_identity(slot_index)?;
        let payload = Bypass { enabled }
            .encode()
            .map_err(|error| error.to_string())?;
        let response = session
            .client_mut()
            .request(ControlOperation::SetSlotBypass, Some(slot), &payload)
            .map_err(|error| error.to_string())?;
        if response.status() == ControlResponseStatus::Ok {
            if let Some(model_slot) = worker.rack.slots.get_mut(slot_index) {
                model_slot.bypassed = enabled;
            }
            Ok(())
        } else {
            Err(format!("worker rejected bypass: {:?}", response.status()))
        }
    }

    /// Writes one parameter through a running worker and records it in the worker's rack.
    #[cfg(test)]
    pub(crate) fn write_parameter(
        &mut self,
        rack_index: usize,
        slot: usize,
        parameter_id: u32,
        normalized: f64,
    ) -> Result<(), String> {
        let worker = self
            .workers
            .get_mut(&rack_index)
            .ok_or("rack worker is not running")?;
        let session = worker
            .session
            .as_mut()
            .ok_or("rack worker is currently recovering")?;
        write_worker_parameter(session, slot, parameter_id, normalized)?;
        if let Some(model_slot) = worker.rack.slots.get_mut(slot) {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "normalized parameter values are bounded to 0.0..=1.0"
            )]
            let value = sp_model::NormalizedValue(normalized as f32);
            model_slot
                .parameters
                .values
                .insert(sp_model::ParameterId(parameter_id.to_string()), value);
        }
        Ok(())
    }

    /// Reads one parameter's normalized value from a running worker.
    #[cfg(test)]
    pub(crate) fn read_worker_parameter(
        &mut self,
        rack_index: usize,
        slot: usize,
        parameter_id: u32,
    ) -> Result<f64, String> {
        let session = self
            .workers
            .get_mut(&rack_index)
            .and_then(|worker| worker.session.as_mut())
            .ok_or("rack worker is not running")?;
        worker_parameter_metadata(session, slot, parameter_id).map(|metadata| metadata.normalized)
    }

    /// Starts editor attachment without blocking the desktop event loop or stopping audio.
    /// `placement` positions a newly created window; an open one is only brought to front.
    pub(crate) fn begin_native_editor(
        &mut self,
        rack_index: usize,
        slot: usize,
        audio_running: bool,
        placement: Option<EditorPlacement>,
    ) -> Result<(), String> {
        let identity = slot_identity(slot)?;
        let payload = placement
            .map(|placement| {
                EditorPosition {
                    left: placement.left,
                    top: placement.top,
                }
                .encode()
            })
            .transpose()
            .map_err(|error| error.to_string())?
            .unwrap_or_default();
        let worker = self
            .workers
            .get_mut(&rack_index)
            .ok_or("rack worker is not running")?;
        if worker.maintenance.is_some() || worker.recovery.state() != RackRecoveryState::Idle {
            return Err("rack is waiting for plug-in maintenance".to_owned());
        }
        let session = worker
            .session
            .as_mut()
            .ok_or("rack worker is currently recovering")?;
        let result = session.client_mut().begin_request(
            ControlOperation::OpenNativeEditor,
            Some(identity),
            &payload,
        );
        match result {
            Ok(()) => {}
            Err(sp_supervisor::WorkerControlError::Busy) => {
                return Err("An editor request is already in progress for this rack".to_owned());
            }
            Err(error) => {
                return self.finish_editor_request(rack_index, audio_running, Err(error));
            }
        }
        if let Some(editor) = worker.editors.get_mut(slot) {
            editor.opening = true;
        }
        self.editor_requests.insert(rack_index);
        Ok(())
    }

    pub(crate) fn native_editor_pending(&self) -> bool {
        !self.editor_requests.is_empty()
    }

    /// Delivers one completed editor request. A successful open marks the slot's editor open;
    /// picture polls learn when the user closes it.
    pub(crate) fn poll_native_editor_result(
        &mut self,
        audio_running: bool,
    ) -> Option<Result<(), String>> {
        let completed = self.editor_requests.iter().find_map(|&rack| {
            self.workers
                .get_mut(&rack)?
                .session
                .as_mut()?
                .client_mut()
                .poll_request()
                .map(|response| (rack, response))
        });
        let (rack, response) = completed?;
        self.editor_requests.remove(&rack);
        let opened = response
            .as_ref()
            .is_ok_and(|response| response.status() == ControlResponseStatus::Ok);
        if let Some(worker) = self.workers.get_mut(&rack) {
            for editor in &mut worker.editors {
                if std::mem::take(&mut editor.opening) && opened {
                    editor.open = true;
                }
            }
        }
        Some(self.finish_editor_request(rack, audio_running, response))
    }

    /// Previews captured since the last call. Cheap to call every UI frame: it polls a rack's
    /// worker at most once per second, and only while one of its editors is open or just closed.
    ///
    /// A slot is polled until its worker reports the window closed; that reply carries the
    /// picture taken as it closed. A rack with an editor request in flight waits a round. A
    /// failed poll records no worker fault; a rack whose control connection failed stops
    /// polling.
    pub(crate) fn take_editor_previews(&mut self) -> Vec<EditorPreviewFile> {
        let now = Instant::now();
        let mut updates = Vec::new();
        let mut failures = Vec::new();
        for (&rack_index, worker) in &mut self.workers {
            if now < worker.next_preview_poll
                || !worker.editors.iter().any(|editor| editor.open)
                || self.editor_requests.contains(&rack_index)
                || worker.maintenance.is_some()
                || worker.recovery.state() != RackRecoveryState::Idle
            {
                continue;
            }
            let Some(session) = worker.session.as_mut() else {
                continue;
            };
            worker.next_preview_poll = now + EDITOR_PREVIEW_POLL_INTERVAL;
            let mut failure = None;
            for (slot_index, editor) in worker.editors.iter_mut().enumerate() {
                let Ok(slot) = slot_identity(slot_index) else {
                    continue;
                };
                if !editor.open {
                    continue;
                }
                match session
                    .client_mut()
                    .capture_editor_preview(slot, editor.preview_sequence)
                {
                    Ok(poll) => {
                        editor.open = poll.editor_open;
                        editor.preview_sequence = poll.sequence;
                        if let (Some(png), Some(model_slot)) =
                            (poll.png, worker.rack.slots.get(slot_index))
                        {
                            updates.push(EditorPreviewFile {
                                instance_id: model_slot.id.0.clone(),
                                png,
                                captured_at_unix_ms: poll.captured_at_unix_ms,
                            });
                        }
                    }
                    // The worker is busy or refused this slot; poll again next round.
                    Err(sp_supervisor::WorkerControlError::Rejected { status, .. })
                        if !editor_response_requires_recovery(Some(status)) => {}
                    Err(error) => {
                        failure = Some(error);
                        break;
                    }
                }
            }
            if let Some(error) = failure {
                for editor in &mut worker.editors {
                    editor.open = false;
                }
                failures.push(format!(
                    "rack {} editor pictures stopped: {error}",
                    rack_index + 1
                ));
            }
        }
        for failure in failures {
            self.note(failure);
        }
        updates
    }

    /// Whether this slot's native editor window is open, as last reported by its worker.
    pub(crate) fn editor_open(&self, rack_index: usize, slot: usize) -> bool {
        self.workers
            .get(&rack_index)
            .and_then(|worker| worker.editors.get(slot))
            .is_some_and(|editor| editor.open)
    }

    /// Opens or closes a slot's editor window and waits for the worker. Closing works like the
    /// window's close button: the worker takes a final picture, which the next preview poll
    /// delivers.
    pub(crate) fn set_native_editor_open(
        &mut self,
        rack_index: usize,
        slot: usize,
        open: bool,
        audio_running: bool,
    ) -> Result<(), String> {
        let operation = if open {
            ControlOperation::OpenNativeEditor
        } else {
            ControlOperation::CloseNativeEditor
        };
        let session = self
            .workers
            .get_mut(&rack_index)
            .and_then(|worker| worker.session.as_mut())
            .ok_or("rack worker is not running")?;
        let response = session
            .client_mut()
            .request(operation, Some(slot_identity(slot)?), &[]);
        self.finish_editor_request(rack_index, audio_running, response)
    }

    fn finish_editor_request(
        &mut self,
        rack_index: usize,
        audio_running: bool,
        response: Result<ControlResponse, sp_supervisor::WorkerControlError>,
    ) -> Result<(), String> {
        if editor_response_requires_recovery(response.as_ref().ok().map(ControlResponse::status)) {
            if audio_running {
                // Callback mappings must remain alive until the callback confirms quiescence.
                // A control failure alone is not evidence that a plug-in should be quarantined.
                self.recover_worker(rack_index, PluginFailureKind::WorkerTimeout);
            } else {
                self.unload_rack(rack_index);
            }
        }
        let response =
            response.map_err(|error| format!("native editor is unavailable: {error}"))?;
        if response.status() == ControlResponseStatus::Ok {
            Ok(())
        } else {
            let detail = response.error_record().map_or_else(
                || format!("{:?}", response.status()),
                |error| error.message().to_owned(),
            );
            Err(format!("native editor is unavailable: {detail}"))
        }
    }

    pub(crate) fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }

    pub(crate) fn worker_running(&self, rack_index: usize) -> bool {
        self.workers
            .get(&rack_index)
            .is_some_and(|worker| worker.session.is_some())
    }

    pub(crate) fn slot_running(&self, rack_index: usize, slot: usize) -> bool {
        self.workers.get(&rack_index).is_some_and(|worker| {
            worker.session.is_some() && worker.loaded_slots.get(slot) == Some(&true)
        })
    }

    pub(crate) fn worker_recovering(&self, rack_index: usize) -> bool {
        !self.worker_recovery_failed(rack_index)
            && self
                .workers
                .get(&rack_index)
                .is_some_and(|worker| worker.recovery.state() != RackRecoveryState::Idle)
    }

    pub(crate) fn worker_recovery_failed(&self, rack_index: usize) -> bool {
        self.workers.get(&rack_index).is_some_and(|worker| {
            worker.recovery_failed
                || worker
                    .maintenance
                    .as_ref()
                    .is_some_and(|maintenance| maintenance.failed || maintenance.terminal)
        })
    }

    pub(crate) fn rack_latency_samples(&self, rack_index: usize) -> Option<u32> {
        self.workers
            .get(&rack_index)
            .map(|worker| worker.latency_samples)
    }

    /// Worker restarts (crash recovery and planned maintenance) for the rack at this position
    /// since its worker was first loaded.
    pub(crate) fn rack_restart_count(&self, rack_index: usize) -> u32 {
        self.workers
            .get(&rack_index)
            .map_or(0, |worker| worker.restarts)
    }

    pub(crate) fn rack_restart_flags(&self, rack_index: usize) -> u32 {
        self.workers
            .get(&rack_index)
            .map_or(0, |worker| worker.restart_flags)
    }

    pub(crate) fn planned_maintenance_failed(&self, rack_index: usize) -> bool {
        self.workers.get(&rack_index).is_some_and(|worker| {
            worker.recovery.state() == RackRecoveryState::Quiescent
                && worker.session.is_some()
                && worker
                    .maintenance
                    .as_ref()
                    .is_some_and(|maintenance| maintenance.failed || maintenance.terminal)
        })
    }

    pub(crate) fn maintenance_pending(&self) -> bool {
        self.workers.values().any(|worker| {
            worker.maintenance.as_ref().is_some_and(|maintenance| {
                !maintenance.failed
                    && !maintenance.terminal
                    && worker.recovery.state() != RackRecoveryState::Idle
            })
        })
    }

    pub(crate) fn retry_planned_maintenance(&mut self, rack_index: usize) -> Result<(), String> {
        let worker = self
            .workers
            .get_mut(&rack_index)
            .ok_or("rack worker is not retained")?;
        let maintenance = worker
            .maintenance
            .as_mut()
            .ok_or("rack has no planned restart to retry")?;
        if !maintenance.failed && !maintenance.terminal {
            return Err("rack restart is already in progress".to_owned());
        }
        if worker.recovery.state() != RackRecoveryState::Quiescent {
            return Err("rack is not quiescent; retry is unsafe".to_owned());
        }
        if worker.session.is_none() {
            return Err("old worker is no longer available for fresh state capture".to_owned());
        }
        if maintenance.task_id.is_some() || maintenance.ready.is_some() {
            return Err("previous rack restart has not finished".to_owned());
        }
        if maintenance
            .new_pid
            .is_some_and(|pid| !self.processes.is_reaped(pid))
        {
            return Err("previous replacement worker has not exited".to_owned());
        }
        if self.editor_requests.contains(&rack_index) {
            return Err("finish the native editor request before retrying".to_owned());
        }
        maintenance.failed = false;
        maintenance.terminal = false;
        worker.maintenance_budget = RestartBudget::default();
        self.note(format!(
            "rack {} planned restart retry requested",
            rack_index + 1
        ));
        Ok(())
    }

    pub(crate) fn rack_dry_fallback_available(&self, rack: &sp_model::Rack) -> bool {
        let Ok(slots) = self.resolve_rack_slots(rack) else {
            return false;
        };
        let mut loaded = slots.iter().flatten();
        let Some(first) = loaded.next() else {
            return true;
        };
        let last = loaded.last().unwrap_or(first);
        first.input_channels == 2 && last.output_channels == 2
    }

    pub(crate) fn update_rack_snapshot(&mut self, rack_index: usize, rack: &sp_model::Rack) {
        if let Some(worker) = self.workers.get_mut(&rack_index) {
            if worker.maintenance.is_some() || worker.recovery.state() != RackRecoveryState::Idle {
                return;
            }
            for (slot_index, slot) in rack.slots.iter().enumerate() {
                let Some(old) = worker.rack.slots.get(slot_index) else {
                    continue;
                };
                if old.id != slot.id || old.plugin != slot.plugin {
                    self.pending_parameter_updates
                        .retain(|(index, position, _), _| {
                            *index != rack_index || *position != slot_index
                        });
                    continue;
                }
                for (id, value) in &slot.parameters.values {
                    if old.parameters.values.get(id) != Some(value)
                        && let Ok(parameter_id) = id.0.parse::<u32>()
                    {
                        self.pending_parameter_updates.remove(&(
                            rack_index,
                            slot_index,
                            parameter_id,
                        ));
                    }
                }
            }
            worker.rack = rack.clone();
        }
    }

    pub(crate) fn scene_parameter_metadata(
        &self,
        slot: &sp_model::PluginSlot,
    ) -> &[sp_model::PluginParameterMetadata] {
        self.scanner
            .catalog()
            .entries()
            .find(|entry| entry.fingerprint.digest == slot.plugin.fingerprint.digest)
            .and_then(|entry| {
                entry
                    .metadata
                    .classes
                    .iter()
                    .find(|class| class.identity.unique_id == slot.plugin.identity.unique_id)
            })
            .map_or(&[], |class| class.parameters.as_slice())
    }

    /// Reads only the selected scene targets while audio keeps running. Cached scanner
    /// metadata supplies the discrete/continuous distinction.
    pub(crate) fn capture_scene_parameters(
        &mut self,
        model: &mut sp_model::Session,
        scene: &mut sp_model::Scene,
    ) -> Result<(), String> {
        for target in &mut scene.parameter_values {
            let worker = self
                .workers
                .values_mut()
                .find(|worker| worker.rack.id == target.rack_id)
                .ok_or("a selected scene parameter has no running rack worker")?;
            let slot_index = worker
                .rack
                .slots
                .iter()
                .position(|slot| slot.id == target.slot_id)
                .ok_or("a selected scene parameter no longer has a plug-in slot")?;
            let session = worker
                .session
                .as_mut()
                .ok_or("a selected scene parameter's worker is recovering")?;
            let parameter_id = target
                .parameter_id
                .0
                .parse::<u32>()
                .map_err(|_| "scene parameter ID is not a VST3 u32")?;
            let parameter = worker
                .parameters
                .get(slot_index)
                .and_then(|parameters| {
                    parameters
                        .iter()
                        .find(|parameter| parameter.id == parameter_id)
                })
                .ok_or(
                    "Selected parameter is missing from plug-in metadata; edit the scene capture",
                )?;
            if !parameter.automatable || parameter.read_only || parameter.bypass {
                return Err(format!(
                    "{} is not a recallable parameter; remove it in Edit capture",
                    parameter.name
                ));
            }
            let step_count = parameter.step_count;
            // A live read: the worker answers from its loading thread while audio continues.
            let read = worker_parameter_values(
                session,
                slot_index,
                vec![WireParameterId {
                    value: u64::from(parameter_id),
                }],
            )?;
            let normalized = read
                .first()
                .ok_or("worker returned no scene parameter value")?
                .normalized;
            #[allow(
                clippy::cast_possible_truncation,
                reason = "normalized values are bounded to 0..=1"
            )]
            let value = sp_model::NormalizedValue::new(normalized as f32)
                .map_err(|error| error.to_string())?;
            target.value = value;
            if step_count > 0 {
                target.transition = sp_model::SceneParameterTransition::Step;
            }
            if let Some(slot) = model
                .racks
                .iter_mut()
                .find(|rack| rack.id == target.rack_id)
                .and_then(|rack| rack.slots.iter_mut().find(|slot| slot.id == target.slot_id))
            {
                slot.parameters
                    .values
                    .insert(target.parameter_id.clone(), value);
            }
        }
        // Commit the recovery snapshot only after every selected read succeeds.
        for worker in self.workers.values_mut() {
            if let Some(rack) = model.racks.iter().find(|rack| rack.id == worker.rack.id) {
                worker.rack = rack.clone();
            }
        }
        Ok(())
    }

    /// Copies current worker values into the session before a save or scene snapshot.
    pub(crate) fn sync_worker_parameters(
        &mut self,
        model: &mut sp_model::Session,
    ) -> Result<(), String> {
        for (&rack_index, worker) in &mut self.workers {
            if worker.maintenance.is_some() || worker.recovery.state() != RackRecoveryState::Idle {
                return Err(format!(
                    "rack {} is completing a worker handoff",
                    rack_index + 1
                ));
            }
            let session = worker
                .session
                .as_mut()
                .ok_or_else(|| format!("rack {} worker is recovering", rack_index + 1))?;
            let rack_id = &worker.rack.id;
            for (slot_index, worker_slot) in worker.rack.slots.iter_mut().enumerate() {
                if worker.loaded_slots.get(slot_index) != Some(&true) {
                    continue;
                }
                let Some(model_slot) = model
                    .racks
                    .iter_mut()
                    .find(|rack| rack.id == *rack_id)
                    .and_then(|rack| {
                        rack.slots.iter_mut().find(|slot| {
                            slot.id == worker_slot.id
                                && slot.plugin.fingerprint.digest
                                    == worker_slot.plugin.fingerprint.digest
                        })
                    })
                else {
                    continue;
                };
                let parameters = worker
                    .parameters
                    .get(slot_index)
                    .map_or(&[][..], Vec::as_slice);
                for batch in parameters.chunks(MAX_PARAMETER_BATCH_SIZE) {
                    let ids = batch
                        .iter()
                        .filter(|parameter| !parameter.read_only)
                        .map(|parameter| WireParameterId {
                            value: u64::from(parameter.id),
                        })
                        .collect::<Vec<_>>();
                    if ids.is_empty() {
                        continue;
                    }
                    for parameter in worker_parameter_values(session, slot_index, ids)? {
                        #[allow(
                            clippy::cast_possible_truncation,
                            reason = "VST3 normalized values are bounded to 0.0..=1.0"
                        )]
                        let value = sp_model::NormalizedValue::new(parameter.normalized as f32)
                            .map_err(|error| error.to_string())?;
                        let id = sp_model::ParameterId(parameter.id.value.to_string());
                        worker_slot.parameters.values.insert(id.clone(), value);
                        model_slot.parameters.values.insert(id, value);
                    }
                }
            }
        }
        self.pending_parameter_updates.clear();
        for worker in self.workers.values_mut() {
            worker.parameter_mirror_incomplete = false;
        }
        Ok(())
    }

    /// Opens callback-owned mappings for every retained rack worker before `CoreAudio` starts.
    /// The worker and callback have separate mappings of the same POSIX objects.
    pub(crate) fn audio_bank_mappings(&self) -> Result<Vec<AudioBankMapping>, String> {
        self.workers
            .iter()
            .map(|(&rack_index, worker)| {
                if worker.recovery.state() != RackRecoveryState::Idle
                    || worker.maintenance.as_ref().is_some_and(|maintenance| maintenance.failed)
                {
                    return Err(format!(
                        "rack {} has an unfinished worker handoff; wait for maintenance or reload the rack before starting audio",
                        rack_index + 1
                    ));
                }
                if worker.active_bank_index >= worker.banks.len() {
                    return Err(format!("rack {} has an invalid active bank", rack_index + 1));
                }
                Ok((
                    rack_index,
                    [
                        SharedMemoryRegion::open(worker.banks[0].name()).map_err(|error| {
                            format!("cannot map rack bank 0 for audio: {error}")
                        })?,
                        SharedMemoryRegion::open(worker.banks[1].name()).map_err(|error| {
                            format!("cannot map rack bank 1 for audio: {error}")
                        })?,
                    ],
                    worker.active_bank_index,
                    Arc::clone(&worker.recovery),
                ))
            })
            .collect()
    }

    fn recover_worker(&mut self, rack_index: usize, kind: PluginFailureKind) {
        let Some(worker) = self.workers.get_mut(&rack_index) else {
            return;
        };
        if !worker.recovery.request_quiesce() {
            return;
        }
        self.record_worker_fault(rack_index, kind);
    }

    fn record_worker_fault(&mut self, rack_index: usize, kind: PluginFailureKind) {
        let Some(worker) = self.workers.get_mut(&rack_index) else {
            return;
        };
        if worker.recovery_recorded {
            return;
        }
        worker.recovery_recorded = true;
        let restart_allowed = worker.restart_budget.allows_restart(Instant::now());
        if !restart_allowed {
            worker.recovery_failed = true;
        }
        // A missed audio deadline does not identify a faulty plug-in. Only a
        // confirmed control health failure can quarantine or bypass a plug-in.
        worker.suspected_slot = (kind == PluginFailureKind::WorkerHang)
            .then(|| {
                worker.session.as_mut().and_then(|session| {
                    session
                        .client_mut()
                        .request(ControlOperation::QuerySlotAttribution, None, &[])
                        .ok()
                        .filter(|response| response.status() == ControlResponseStatus::Ok)
                        .and_then(|response| {
                            response.payload().try_into().ok().map(u64::from_le_bytes)
                        })
                        .and_then(|slot| usize::try_from(slot.checked_sub(1)?).ok())
                        .filter(|&slot| slot < worker.rack.slots.len())
                })
            })
            .flatten();
        if kind == PluginFailureKind::WorkerHang {
            let suspected_digest = worker.suspected_slot.and_then(|slot| {
                worker
                    .rack
                    .slots
                    .get(slot)
                    .map(|slot| slot.plugin.fingerprint.digest.clone())
            });
            let fingerprint = suspected_digest
                .as_ref()
                .and_then(|digest| {
                    self.scanner
                        .catalog()
                        .entries()
                        .find(|entry| entry.fingerprint.digest == *digest)
                        .map(|entry| entry.fingerprint.clone())
                })
                .unwrap_or_else(|| worker.fingerprint.clone());
            let _ = self
                .scanner
                .quarantine_mut()
                .expect("product scanner has quarantine")
                .record_failure(fingerprint, kind);
        }
        self.editor_requests.remove(&rack_index);
        if restart_allowed {
            self.note(format!(
                "rack {rack_index} entered dry fallback ({kind:?}) while its worker is replaced"
            ));
        } else {
            self.note(format!(
                "rack {rack_index} exceeded {RAPID_RESTART_LIMIT} recoveries in 10 seconds ({kind:?}); its worker will stop after callback quiescence and dry fallback will remain active"
            ));
        }
    }

    fn drive_recoveries(&mut self) {
        let rack_indices: Vec<usize> = self.workers.keys().copied().collect();
        for rack_index in rack_indices {
            let Some(state) = self
                .workers
                .get(&rack_index)
                .map(|worker| worker.recovery.state())
            else {
                continue;
            };
            match state {
                RackRecoveryState::Quiescent => {
                    if self.workers[&rack_index].maintenance.is_some() {
                        self.launch_planned_replacement(rack_index);
                    } else {
                        self.launch_replacement(rack_index);
                    }
                }
                RackRecoveryState::ReplacementActive {
                    retiring_bank_index,
                } => self.reset_retired_bank(rack_index, retiring_bank_index),
                RackRecoveryState::Idle
                | RackRecoveryState::QuiesceRequested
                | RackRecoveryState::ReplacementReady { .. }
                | RackRecoveryState::RetiredReset { .. } => {}
            }
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "planned launch freezes worker identity and starts one bounded background task"
    )]
    fn launch_planned_replacement(&mut self, rack_index: usize) {
        if self.editor_requests.contains(&rack_index) {
            return;
        }
        let Some(mut worker) = self.workers.remove(&rack_index) else {
            return;
        };
        let result = (|| {
            let maintenance = worker
                .maintenance
                .as_ref()
                .ok_or("rack has no planned maintenance")?;
            if maintenance.failed
                || maintenance.terminal
                || maintenance.task_id.is_some()
                || maintenance.ready.is_some()
            {
                return Ok(());
            }
            if let Some(pid) = maintenance.new_pid
                && !self.processes.is_reaped(pid)
            {
                return Err("previous replacement worker has not exited".to_owned());
            }
            let candidates = self.resolve_rack_slots(&worker.rack)?;
            let first = candidates
                .iter()
                .flatten()
                .next()
                .ok_or("rack contains no available plug-ins")?;
            for candidate in candidates.iter().flatten() {
                self.scanner
                    .quarantine()
                    .expect("product scanner has quarantine")
                    .ensure_launch_permitted(&candidate.fingerprint)
                    .map_err(|error| {
                        format!("plug-in cannot restart while quarantined: {error}")
                    })?;
            }
            let replacement_index = worker.active_bank_index ^ 1;
            let generation = next_bank_generation(&worker.banks);
            reset_reaped_region(&mut worker.banks[replacement_index], generation)
                .map_err(|error| format!("cannot reset replacement bank: {error}"))?;
            let bank_name = worker.banks[replacement_index].name();
            let target = control_target(worker.wire_index, replacement_index, generation)?;
            let socket = self
                .app_support
                .join(format!("{}.sock", bank_name.trim_start_matches('/')));
            let _ = fs::remove_file(&socket);
            let launch = WorkerControlLaunch {
                launch: HelperLaunch {
                    kind: HelperKind::PluginWorker,
                    executable: self.helpers.worker.clone(),
                    arguments: worker_arguments(
                        bank_name,
                        &socket,
                        target,
                        &first.bundle,
                        &first.class_id,
                    ),
                },
                control_socket: socket,
                target,
                timeout: CONTROL_TIMEOUT,
            };
            launch.validate().map_err(|error| error.to_string())?;
            let task_id = self.next_maintenance_task_id;
            self.next_maintenance_task_id = self
                .next_maintenance_task_id
                .checked_add(1)
                .ok_or("maintenance task identifier exhausted")?;
            let old_session = worker
                .session
                .take()
                .ok_or("old rack worker is unavailable for fresh state capture")?;
            let old_pid = old_session.process_id();
            let cancel = Arc::new(AtomicBool::new(false));
            let input = maintenance::Input {
                rack_index,
                task_id,
                generation,
                recovery: Arc::clone(&worker.recovery),
                rack: worker.rack.clone(),
                loaded_slots: worker.loaded_slots.clone(),
                parameters: worker.parameters.clone(),
                candidates,
                launch,
                cancel: Arc::clone(&cancel),
            };
            if let Err(failure) =
                maintenance::start(input, old_session, self.maintenance_events_tx.clone())
            {
                let (error, session) = *failure;
                worker.session = Some(session);
                return Err(error);
            }
            let maintenance = worker
                .maintenance
                .as_mut()
                .expect("planned maintenance exists");
            maintenance.task_id = Some(task_id);
            maintenance.cancel = Some(cancel);
            maintenance.old_pid = Some(old_pid);
            maintenance.new_pid = None;
            maintenance.ready = None;
            self.note(format!(
                "rack {} fresh state capture and replacement started in background",
                rack_index + 1
            ));
            Ok(())
        })();
        if let Err(error) = result {
            worker
                .maintenance
                .as_mut()
                .expect("planned maintenance exists")
                .failed = true;
            self.note(format!(
                "rack {} planned restart failed: {error}; old worker retained and rack remains dry",
                rack_index + 1
            ));
        }
        self.workers.insert(rack_index, worker);
    }
    fn launch_replacement(&mut self, rack_index: usize) {
        if self
            .workers
            .get(&rack_index)
            .is_some_and(|worker| !worker.recovery_recorded)
        {
            self.record_worker_fault(rack_index, PluginFailureKind::WorkerTimeout);
        }
        let Some(mut worker) = self.workers.remove(&rack_index) else {
            return;
        };
        if let Some(session) = worker.session.take() {
            let _ = session.shutdown(&mut self.processes);
        }
        worker.editors = vec![EditorWindowState::default(); worker.rack.slots.len()];
        if worker.recovery_failed {
            self.workers.insert(rack_index, worker);
            return;
        }
        let replacement_index = worker.active_bank_index ^ 1;
        let generation = next_bank_generation(&worker.banks);
        let reset = reset_reaped_region(&mut worker.banks[replacement_index], generation);
        let launched = reset
            .map_err(|error| format!("cannot reset replacement bank: {error}"))
            .and_then(|()| {
                self.launch_worker(
                    worker.wire_index,
                    &worker.rack,
                    &worker.saved_states,
                    &worker.banks[replacement_index],
                    replacement_index,
                    generation,
                    worker.suspected_slot,
                )
            });
        match launched {
            Ok(launched) => {
                worker.fingerprint = launched.fingerprint;
                worker.session = Some(launched.session);
                worker.parameter_feedback = launched
                    .parameters
                    .iter()
                    .map(|parameters| ParameterFeedbackCursor::new(parameters))
                    .collect();
                worker.restart_cursors = vec![RestartCursor::default(); launched.parameters.len()];
                worker.parameters = launched.parameters;
                worker.loaded_slots = launched.loaded_slots;
                worker.active_bank_index = replacement_index;
                worker.heartbeat = HeartbeatWatch::new(Instant::now());
                worker.suspected_slot = None;
                if worker
                    .recovery
                    .publish_replacement(replacement_index, generation)
                {
                    worker.restarts = worker.restarts.saturating_add(1);
                    self.note(format!(
                        "rack {rack_index} replacement worker is ready on bank {replacement_index} (generation {generation})"
                    ));
                } else {
                    worker.recovery_failed = true;
                    if let Some(session) = worker.session.take() {
                        let _ = session.shutdown(&mut self.processes);
                    }
                    self.note(format!(
                        "rack {rack_index} replacement handoff was rejected; dry fallback remains active"
                    ));
                }
            }
            Err(error) => {
                worker.recovery_failed = true;
                self.note(format!(
                    "rack {rack_index} replacement failed: {error}; dry fallback remains active"
                ));
            }
        }
        self.workers.insert(rack_index, worker);
    }

    fn reset_retired_bank(&mut self, rack_index: usize, retired_bank_index: usize) {
        let Some(worker) = self.workers.get_mut(&rack_index) else {
            return;
        };
        let generation = next_bank_generation(&worker.banks);
        let Some(bank) = worker.banks.get_mut(retired_bank_index) else {
            if let Some(maintenance) = worker.maintenance.as_mut() {
                maintenance.failed = true;
            } else {
                worker.recovery_failed = true;
            }
            return;
        };
        match reset_reaped_region(bank, generation) {
            Ok(())
                if worker
                    .recovery
                    .publish_retired_reset(retired_bank_index, generation) =>
            {
                self.note(format!(
                    "rack {rack_index} switched workers; bank {retired_bank_index} is ready for the next recovery"
                ));
            }
            Ok(()) => {
                if let Some(maintenance) = worker.maintenance.as_mut() {
                    maintenance.failed = true;
                } else {
                    worker.recovery_failed = true;
                }
                self.note(format!(
                    "rack {rack_index} retired-bank handoff failed; future rack restarts are blocked"
                ));
            }
            Err(error) => {
                if let Some(maintenance) = worker.maintenance.as_mut() {
                    maintenance.failed = true;
                } else {
                    worker.recovery_failed = true;
                }
                self.note(format!(
                    "rack {rack_index} could not reset retired bank: {error}; future rack restarts are blocked"
                ));
            }
        }
    }

    fn resolve_rack_slots(
        &self,
        rack: &sp_model::Rack,
    ) -> Result<Vec<Option<ResolvedSlot>>, String> {
        rack.slots
            .iter()
            .map(|slot| {
                let Some(entry) = self.scanner.catalog().entries().find(|entry| {
                    entry.fingerprint.digest == slot.plugin.fingerprint.digest
                        && entry.is_supported()
                }) else {
                    return Ok(None);
                };
                let Some(class) = entry
                    .metadata
                    .classes
                    .iter()
                    .find(|class| class.identity.unique_id == slot.plugin.identity.unique_id)
                else {
                    return Ok(None);
                };
                let input_channels = class
                    .buses
                    .inputs
                    .iter()
                    .find(|bus| bus.main && !bus.event)
                    .map_or(0, |bus| bus.channels);
                let output_channels = class
                    .buses
                    .outputs
                    .iter()
                    .find(|bus| bus.main && !bus.event)
                    .map_or(0, |bus| bus.channels);
                Ok(Some(ResolvedSlot {
                    bundle: entry.bundle.clone(),
                    fingerprint: entry.fingerprint.clone(),
                    class_id: class.identity.unique_id.clone(),
                    parameters: class.parameters.clone(),
                    input_channels,
                    output_channels,
                    event_input_active: class.buses.inputs.iter().any(|bus| bus.event),
                    sidechain: slot.sidechain.is_some(),
                }))
            })
            .collect()
    }

    fn next_generation(&self, rack_index: usize) -> u64 {
        // A live rack is replaced only after its old session is removed, so retain a monotonically
        // increasing identifier in the control socket namespace rather than reusing bank identity.
        // The current worker protocol exposes no immutable target accessor on a retained session.
        u64::try_from(
            self.diagnostics
                .len()
                .saturating_add(rack_index)
                .saturating_add(1),
        )
        .unwrap_or(u64::MAX)
        .max(1)
    }

    fn note(&mut self, message: String) {
        self.diagnostics.push(message);
        if self.diagnostics.len() > 32 {
            self.diagnostics.remove(0);
        }
    }
}

fn slot_identity(slot: usize) -> Result<SlotIdentity, String> {
    SlotIdentity::new(
        u64::try_from(slot)
            .map_err(|_| "slot index exceeds product capacity")?
            .saturating_add(1),
    )
    .map_err(|error| error.to_string())
}

#[allow(
    clippy::too_many_lines,
    reason = "isolated worker configuration restores a complete rack before it becomes audible"
)]
fn configure_worker_session(
    mut session: WorkerControlSession,
    rack: &sp_model::Rack,
    candidates: &[Option<ResolvedSlot>],
    saved_states: &[Option<CapturedPluginState>],
    suspected_slot: Option<usize>,
) -> Result<LaunchedWorker, String> {
    let first = candidates
        .iter()
        .flatten()
        .next()
        .ok_or("rack contains no available plug-ins")?;
    let topology = WireRackTopology {
        slots: candidates
            .iter()
            .enumerate()
            .filter_map(|(slot, candidate)| {
                Some(wire_slot_configuration(slot, candidate.as_ref()?))
            })
            .collect(),
    };
    let payload = topology.encode().map_err(|error| error.to_string())?;
    let response = session
        .client_mut()
        .request(ControlOperation::RebuildRack, None, &payload)
        .map_err(|error| format!("rack rebuild request failed: {error}"))?;
    if response.status() != ControlResponseStatus::Ok {
        let detail = response.error_record().map_or_else(
            || format!("{:?}", response.status()),
            |error| error.message().to_owned(),
        );
        return Err(format!("worker rejected rack rebuild: {detail}"));
    }
    let restore = (|| {
        for (slot, state) in saved_states.iter().enumerate() {
            if candidates.get(slot).is_none_or(Option::is_none) {
                continue;
            }
            let Some(state) = state else {
                continue;
            };
            let expected = &rack
                .slots
                .get(slot)
                .ok_or("saved state slot exceeds rack topology")?
                .plugin
                .fingerprint
                .digest;
            if state.metadata.fingerprint != *expected
                || state.metadata.activation.captured_fingerprint != *expected
            {
                continue;
            }
            restore_worker_state(&mut session, slot, state)
                .map_err(|error| format!("could not restore slot {} state: {error}", slot + 1))?;
        }
        for (slot, model_slot) in rack.slots.iter().enumerate() {
            if candidates.get(slot).is_none_or(Option::is_none) {
                continue;
            }
            let restoring_parameters = !model_slot.parameters.values.is_empty();
            if restoring_parameters {
                request_ok(
                    &mut session,
                    ControlOperation::DeactivateSlot,
                    Some(slot_identity(slot)?),
                    &[],
                )?;
            }
            for (parameter_id, value) in &model_slot.parameters.values {
                let parameter_id = parameter_id.0.parse::<u32>().map_err(|_| {
                    format!("saved parameter ID `{}` is not a VST3 u32", parameter_id.0)
                })?;
                if candidates[slot].as_ref().is_some_and(|candidate| {
                    candidate
                        .parameters
                        .iter()
                        .any(|parameter| parameter.id == parameter_id && parameter.read_only)
                }) {
                    continue;
                }
                write_worker_parameter(&mut session, slot, parameter_id, f64::from(value.0))?;
            }
            if restoring_parameters {
                // Activation flushes saved values before the bank receives live audio,
                // including slots that will stay bypassed until later.
                request_ok(
                    &mut session,
                    ControlOperation::ActivateSlot,
                    Some(slot_identity(slot)?),
                    &[],
                )?;
            }
            if model_slot.bypassed {
                let payload = Bypass { enabled: true }
                    .encode()
                    .map_err(|error| error.to_string())?;
                request_ok(
                    &mut session,
                    ControlOperation::SetSlotBypass,
                    Some(slot_identity(slot)?),
                    &payload,
                )?;
            }
        }
        Ok::<_, String>(())
    })();
    restore?;
    // A bypassed plug-in still processes its input, so a suspected plug-in is deactivated
    // instead: it stays out of the chain until the rack reloads.
    if let Some(slot) = suspected_slot {
        request_ok(
            &mut session,
            ControlOperation::DeactivateSlot,
            Some(slot_identity(slot)?),
            &[],
        )?;
    }
    Ok(LaunchedWorker {
        fingerprint: first.fingerprint.clone(),
        session,
        parameters: candidates
            .iter()
            .map(|candidate| {
                candidate
                    .as_ref()
                    .map_or_else(Vec::new, |candidate| candidate.parameters.clone())
            })
            .collect(),
        loaded_slots: candidates.iter().map(Option::is_some).collect(),
    })
}

fn wire_slot_configuration(
    slot: usize,
    candidate: &ResolvedSlot,
) -> sp_protocol::payload::PluginSlotConfiguration {
    sp_protocol::payload::PluginSlotConfiguration {
        slot: u8::try_from(slot).expect("model limits slots to u8"),
        input_channels: candidate.input_channels,
        output_channels: candidate.output_channels,
        event_input_active: candidate.event_input_active,
        sidechain_active: candidate.sidechain,
        bundle_path: candidate.bundle.display().to_string(),
        class_id: Some(candidate.class_id.clone()),
    }
}

fn request_ok(
    session: &mut WorkerControlSession,
    operation: ControlOperation,
    slot: Option<SlotIdentity>,
    payload: &[u8],
) -> Result<ControlResponse, String> {
    let response = session
        .client_mut()
        .request(operation, slot, payload)
        .map_err(|error| error.to_string())?;
    if response.status() == ControlResponseStatus::Ok {
        Ok(response)
    } else {
        Err(format!(
            "worker rejected {operation:?}: {:?}",
            response.status()
        ))
    }
}

fn restore_worker_state(
    session: &mut WorkerControlSession,
    slot: usize,
    state: &CapturedPluginState,
) -> Result<(), String> {
    let slot = slot_identity(slot)?;
    request_ok(session, ControlOperation::DeactivateSlot, Some(slot), &[])?;
    let state = StateRestore {
        component: state.component.clone(),
        controller: state.controller.clone(),
    };
    session
        .client_mut()
        .restore_state(slot, &state)
        .map_err(|error| format!("could not restore plug-in state: {error}"))?;
    request_ok(session, ControlOperation::ActivateSlot, Some(slot), &[])?;
    Ok(())
}

fn write_worker_parameter(
    session: &mut WorkerControlSession,
    slot: usize,
    parameter_id: u32,
    normalized: f64,
) -> Result<(), String> {
    let payload = ParameterWrite {
        id: WireParameterId {
            value: u64::from(parameter_id),
        },
        normalized,
    }
    .encode()
    .map_err(|error| error.to_string())?;
    request_ok(
        session,
        ControlOperation::WriteParameter,
        Some(slot_identity(slot)?),
        &payload,
    )?;
    Ok(())
}

#[cfg(test)]
fn worker_parameter_metadata(
    session: &mut WorkerControlSession,
    slot: usize,
    parameter_id: u32,
) -> Result<sp_protocol::payload::ParameterMetadata, String> {
    let payload = WireParameterId {
        value: u64::from(parameter_id),
    }
    .encode()
    .map_err(|error| error.to_string())?;
    let response = request_ok(
        session,
        ControlOperation::ParameterMetadata,
        Some(slot_identity(slot)?),
        &payload,
    )?;
    sp_protocol::payload::ParameterMetadata::decode(response.payload())
        .map_err(|error| error.to_string())
}

fn worker_parameter_values(
    session: &mut WorkerControlSession,
    slot: usize,
    parameters: Vec<WireParameterId>,
) -> Result<Vec<ParameterWrite>, String> {
    let request = ParameterIds { parameters };
    let payload = request.encode().map_err(|error| error.to_string())?;
    let response = request_ok(
        session,
        ControlOperation::ReadParameters,
        Some(slot_identity(slot)?),
        &payload,
    )?;
    let values = ParameterValues::decode(response.payload()).map_err(|error| error.to_string())?;
    if values.parameters.len() != request.parameters.len()
        || values
            .parameters
            .iter()
            .zip(&request.parameters)
            .any(|(value, id)| value.id != *id)
    {
        return Err("worker parameter batch did not match requested IDs".to_owned());
    }
    Ok(values.parameters)
}

impl Drop for ProductRuntime {
    fn drop(&mut self) {
        for (_, mut worker) in std::mem::take(&mut self.workers) {
            let mut process_ids = Vec::new();
            if let Some(maintenance) = worker.maintenance.as_mut() {
                if let Some(cancel) = &maintenance.cancel {
                    cancel.store(true, Ordering::Release);
                }
                process_ids.extend(
                    [maintenance.old_pid, maintenance.new_pid]
                        .into_iter()
                        .flatten(),
                );
                maintenance.ready = None;
            }
            if let Some(session) = worker.session.take() {
                let pid = session.process_id();
                if worker.maintenance.is_none() {
                    let _ = session.shutdown(&mut self.processes);
                } else {
                    drop(session);
                }
                process_ids.push(pid);
            }
            process_ids.sort_unstable();
            process_ids.dedup();
            for &pid in &process_ids {
                if !self.processes.is_reaped(pid) {
                    let _ = self.processes.stop(pid);
                }
            }
            if process_ids
                .iter()
                .any(|pid| !self.processes.is_reaped(*pid))
            {
                std::mem::forget(worker.banks);
            }
        }
        for cancelled in std::mem::take(&mut self.cancelled_banks) {
            for &pid in &cancelled.process_ids {
                if !self.processes.is_reaped(pid) {
                    let _ = self.processes.stop(pid);
                }
            }
            if cancelled
                .process_ids
                .iter()
                .any(|pid| !self.processes.is_reaped(*pid))
            {
                std::mem::forget(cancelled.banks);
            }
        }
    }
}

impl HelperBinaries {
    fn resolve() -> Result<Self, String> {
        let directory = std::env::var_os("SUPERPOSITION_HELPERS_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::current_exe().ok().and_then(|exe| {
                    let parent = exe.parent()?;
                    let bundled = parent.join("../Helpers");
                    if bundled.is_dir() {
                        Some(bundled)
                    } else {
                        Some(parent.to_path_buf())
                    }
                })
            })
            .ok_or("cannot resolve helper directory; set SUPERPOSITION_HELPERS_DIR")?;
        let scanner = directory.join(HelperKind::PluginScanner.executable_name());
        let worker = directory.join(HelperKind::PluginWorker.executable_name());
        for helper in [&scanner, &worker] {
            if !helper.is_absolute() || !helper.is_file() {
                return Err(format!("required helper is missing: {}", helper.display()));
            }
        }
        Ok(Self { scanner, worker })
    }
}

fn control_target(
    rack: usize,
    bank_index: usize,
    generation: u64,
) -> Result<ControlTarget, String> {
    let rack = RackIdentity::new(
        u8::try_from(rack).map_err(|_| "rack index exceeds product capacity")?,
        generation,
    )
    .map_err(|error| error.to_string())?;
    let bank = BankIdentity::new(
        u8::try_from(bank_index).map_err(|_| "bank index exceeds product capacity")?,
        generation,
    )
    .map_err(|error| error.to_string())?;
    Ok(ControlTarget::new(rack, bank))
}

fn next_bank_generation(banks: &[SharedMemoryRegion; 2]) -> u64 {
    banks
        .iter()
        .map(SharedMemoryRegion::generation)
        .max()
        .unwrap_or_default()
        .saturating_add(1)
        .max(1)
}

fn worker_arguments(
    bank: &str,
    socket: &Path,
    target: ControlTarget,
    bundle: &Path,
    class_id: &str,
) -> Vec<String> {
    vec![
        "--worker".to_owned(),
        "--bank".to_owned(),
        bank.to_owned(),
        "--worker-id".to_owned(),
        "1".to_owned(),
        "--rack-index".to_owned(),
        target.rack().index().to_string(),
        "--rack-generation".to_owned(),
        target.rack().generation().to_string(),
        "--bank-index".to_owned(),
        target.bank().index().to_string(),
        "--bank-generation".to_owned(),
        target.bank().generation().to_string(),
        "--control-socket".to_owned(),
        socket.display().to_string(),
        "--bundle".to_owned(),
        bundle.display().to_string(),
        "--class-id".to_owned(),
        class_id.to_owned(),
    ]
}

fn main() -> eframe::Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if !arguments.is_empty() {
        if let Err(error) = headless::run(&arguments) {
            eprintln!("Superposition: {error}");
            std::process::exit(1);
        }
        return Ok(());
    }
    let session_root = default_session_root();
    let controller = SessionController::open(&session_root)
        .map_err(|error| eframe::Error::AppCreation(Box::new(error)))?;
    let recovery_offered = controller.recovery_offered();
    let runtime = ProductRuntime::open(default_application_support());
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 720.0])
            .with_min_inner_size([960.0, 640.0])
            .with_title("Superposition"),
        ..Default::default()
    };
    eframe::run_native(
        "Superposition",
        native_options,
        Box::new(move |cc| {
            ui::install_style(&cc.egui_ctx);
            Ok(Box::new(LiveRackApp::new(
                controller,
                recovery_offered,
                runtime,
            )))
        }),
    )
}

fn default_application_support() -> PathBuf {
    std::env::var_os("SUPERPOSITION_APP_SUPPORT").map_or_else(
        || dirs_fallback().join("Library/Application Support/Superposition"),
        PathBuf::from,
    )
}
fn default_session_root() -> PathBuf {
    std::env::var_os("SUPERPOSITION_SESSION").map_or_else(
        || default_application_support().join("Default.superposition"),
        PathBuf::from,
    )
}
fn dirs_fallback() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::{
        HeartbeatWatch, ParameterFeedbackCursor, RAPID_RESTART_LIMIT, RestartBudget,
        editor_response_requires_recovery,
    };
    use sp_model::PluginParameterMetadata;
    use sp_protocol::control::ControlResponseStatus;
    use std::time::{Duration, Instant};

    #[test]
    fn rapid_restarts_are_bounded_per_rack_and_reset_after_ten_seconds() {
        let start = Instant::now();
        let mut first = RestartBudget::default();
        let mut second = RestartBudget::default();

        for elapsed_seconds in 0..u64::from(RAPID_RESTART_LIMIT) {
            assert!(first.allows_restart(start + Duration::from_secs(elapsed_seconds)));
        }
        assert!(!first.allows_restart(start + Duration::from_secs(3)));
        assert!(second.allows_restart(start + Duration::from_secs(3)));
        assert!(first.allows_restart(start + Duration::from_secs(10)));
    }

    #[test]
    fn heartbeat_deadline_resets_only_when_the_tick_advances() {
        let start = Instant::now();
        let mut watch = HeartbeatWatch::new(start);
        assert!(!watch.stalled(10, start));
        assert!(!watch.stalled(10, start + Duration::from_millis(1_999)));
        assert!(!watch.stalled(11, start + Duration::from_millis(1_999)));
        assert!(!watch.stalled(11, start + Duration::from_millis(3_998)));
        assert!(watch.stalled(11, start + Duration::from_millis(3_999)));
    }

    #[test]
    fn editor_failure_policy_recovers_only_unusable_workers() {
        assert!(editor_response_requires_recovery(None));
        for status in [
            ControlResponseStatus::StaleGeneration,
            ControlResponseStatus::DuplicateRequest,
            ControlResponseStatus::ShuttingDown,
        ] {
            assert!(editor_response_requires_recovery(Some(status)));
        }
        for status in [
            ControlResponseStatus::Ok,
            ControlResponseStatus::Failed,
            ControlResponseStatus::Rejected,
            ControlResponseStatus::Unsupported,
        ] {
            assert!(!editor_response_requires_recovery(Some(status)));
        }
    }

    #[test]
    fn feedback_cursor_keeps_new_values_without_replaying_unchanged_host_edits() {
        let mut cursor = ParameterFeedbackCursor::new(&[
            PluginParameterMetadata {
                id: 7,
                ..PluginParameterMetadata::default()
            },
            PluginParameterMetadata {
                id: 8,
                ..PluginParameterMetadata::default()
            },
            PluginParameterMetadata {
                id: 9,
                read_only: true,
                ..PluginParameterMetadata::default()
            },
        ]);
        let initial = cursor.accept(2, 1, 0, &[(7, 0.2), (9, 0.9)]);
        assert_eq!(initial.values.len(), 1);
        assert_eq!(initial.values[0].0, 7);
        // The host can now edit parameter 7 to 0.8. A later full slot snapshot still
        // contains its old 0.2 value, but only parameter 8 is new feedback.
        let later = cursor.accept(2, 2, 0, &[(7, 0.2), (8, 0.5), (9, 0.9)]);
        assert_eq!(later.values.len(), 1);
        assert_eq!(later.values[0].0, 8);
        assert!(!later.incomplete);
        let overflow = cursor.accept(2, 3, 1, &[(7, 0.2), (8, 0.5)]);
        assert!(overflow.values.is_empty());
        assert!(overflow.incomplete);
        let replacement = cursor.accept(4, 1, 0, &[(7, 0.8)]);
        assert_eq!(replacement.values.len(), 1);
        assert_eq!(replacement.values[0].1.get().to_bits(), 0.8_f32.to_bits());
    }
}
