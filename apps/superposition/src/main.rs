//! Desktop application composition root.
//!
//! All plug-in discovery and hosting is driven from this process, but all third-party code stays
//! in the scanner or one retained worker process per rack.

mod ui;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use eframe::egui;
use sp_protocol::{
    control::{
        BankIdentity, ControlOperation, ControlResponse, ControlResponseStatus, ControlTarget,
        RackIdentity, SlotIdentity,
    },
    payload::{
        Bypass, ControlPayloadCodec, HealthReport, ParameterId as WireParameterId,
        ParameterMetadata as WireParameterMetadata, ParameterWrite,
        RackTopology as WireRackTopology, StateRestore,
    },
};
use sp_session::{
    CapturedPluginState, PluginActivationMetadata, PluginActivationState, PluginStateMetadata,
    SessionController,
};
use sp_shared_memory_macos::{
    RackRecoverySignal, RackRecoveryState, SharedMemoryRegion, reset_reaped_region,
};
use sp_supervisor::{
    BundleFingerprint, HelperKind, HelperLaunch, PersistentQuarantine, PluginCatalog,
    PluginFailureKind, ProcessSupervisor, Scanner, WorkerControlLaunch, WorkerControlSession,
};

use ui::LiveRackApp;

const QUARANTINE_THRESHOLD: u32 = 3;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);

/// Control-plane state owned by the desktop product, never by the audio callback.
pub(crate) struct ProductRuntime {
    app_support: PathBuf,
    scanner: Scanner,
    quarantine: PersistentQuarantine,
    helpers: HelperBinaries,
    processes: ProcessSupervisor,
    workers: BTreeMap<usize, RetainedWorker>,
    open_editors: BTreeSet<(usize, usize)>,
    diagnostics: Vec<String>,
}

struct HelperBinaries {
    scanner: PathBuf,
    worker: PathBuf,
}

struct RetainedWorker {
    fingerprint: BundleFingerprint,
    /// Both host-side mappings remain alive across alternating worker generations.
    banks: [SharedMemoryRegion; 2],
    active_bank_index: usize,
    session: Option<WorkerControlSession>,
    parameters: Vec<Vec<sp_model::PluginParameterMetadata>>,
    loaded_slots: Vec<bool>,
    rack: sp_model::Rack,
    saved_states: Vec<Option<CapturedPluginState>>,
    recovery: Arc<RackRecoverySignal>,
    recovery_failed: bool,
    recovery_recorded: bool,
    suspected_slot: Option<usize>,
    latency_samples: u32,
    restart_requested: bool,
}

struct LaunchedWorker {
    fingerprint: BundleFingerprint,
    session: WorkerControlSession,
    parameters: Vec<Vec<sp_model::PluginParameterMetadata>>,
    loaded_slots: Vec<bool>,
}

struct ResolvedSlot {
    bundle: PathBuf,
    fingerprint: BundleFingerprint,
    class_id: String,
    parameters: Vec<sp_model::PluginParameterMetadata>,
    input_channels: u8,
    output_channels: u8,
    event_input_active: bool,
}

/// SDK-free row presented by the app's generic parameter editor.
pub(crate) struct GenericParameter {
    pub(crate) id: u32,
    pub(crate) name: String,
    pub(crate) unit: String,
    pub(crate) formatted: String,
    pub(crate) normalized: f64,
    pub(crate) default_normalized: f64,
    pub(crate) read_only: bool,
    pub(crate) discrete: bool,
    pub(crate) step_count: i32,
    pub(crate) automatable: bool,
}

/// One supported scanner result shown by the in-app plug-in browser.
#[derive(Clone)]
pub(crate) struct CatalogPlugin {
    pub(crate) descriptor: sp_model::PluginDescriptor,
    pub(crate) parameters: sp_model::NormalizedParameters,
}

/// One rack's callback-owned audio bank pair and its recovery handshake.
type AudioBankMapping = (usize, [SharedMemoryRegion; 2], Arc<RackRecoverySignal>);

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
        Ok(Self {
            scanner: Scanner::with_default_timeout(&helpers.scanner, catalog),
            quarantine,
            helpers,
            processes: ProcessSupervisor::new(),
            workers: BTreeMap::new(),
            open_editors: BTreeSet::new(),
            diagnostics: Vec::new(),
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
        for scan in &scans {
            let kind = match scan.outcome() {
                sp_model::PluginScanOutcome::TimedOut => Some(PluginFailureKind::ScannerTimeout),
                sp_model::PluginScanOutcome::Crashed => Some(PluginFailureKind::ScannerCrash),
                _ => None,
            };
            if let Some(kind) = kind {
                self.quarantine
                    .record_failure(scan.fingerprint.clone(), kind)
                    .map_err(|error| format!("cannot persist scanner quarantine: {error}"))?;
            }
        }
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
                    }
                })
            })
            .collect()
    }

    pub(crate) fn quarantined_count(&self) -> usize {
        self.quarantine
            .table()
            .records()
            .values()
            .filter(|record| record.quarantined)
            .count()
    }

    pub(crate) fn clear_quarantine(&mut self) -> Result<(), String> {
        self.quarantine
            .clear_all()
            .map_err(|error| error.to_string())?;
        self.note("cleared plug-in quarantine".to_owned());
        Ok(())
    }

    pub(crate) fn poll(&mut self) {
        for event in self.processes.reap() {
            self.note(format!("worker process event: {event:?}"));
        }
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
                    && !worker.recovery_recorded
                    && worker.recovery.state() == RackRecoveryState::QuiesceRequested)
                    .then_some(rack)
            })
            .collect();
        for rack in callback_faults {
            self.record_worker_fault(rack);
        }
        let failed: Vec<(usize, String)> = self
            .workers
            .iter_mut()
            .filter_map(|(&rack, worker)| {
                if worker.recovery_failed || worker.recovery.state() != RackRecoveryState::Idle {
                    return None;
                }
                match worker.session.as_mut()?.client_mut().request(
                    ControlOperation::QueryHealth,
                    None,
                    &[],
                ) {
                    Ok(response) if response.status() == ControlResponseStatus::Ok => {
                        match HealthReport::decode(response.payload()) {
                            Ok(report) => {
                                worker.latency_samples = report.latency_samples;
                                worker.restart_requested = report.restart_requested;
                                None
                            }
                            Err(error) => Some((rack, format!("invalid health report: {error}"))),
                        }
                    }
                    Ok(response) => {
                        Some((rack, format!("health rejected: {:?}", response.status())))
                    }
                    Err(error) => Some((rack, error.to_string())),
                }
            })
            .collect();
        for (rack, detail) in failed {
            self.note(format!("rack {rack} worker fault: {detail}"));
            self.recover_worker(rack);
        }
        self.drive_recoveries();
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
        if let Some(mut old) = self.workers.remove(&rack_index)
            && let Some(session) = old.session.take()
        {
            let _ = session.shutdown(&mut self.processes);
        }
        self.open_editors.retain(|(rack, _)| *rack != rack_index);
        let candidates = self.resolve_rack_slots(rack)?;
        if candidates.iter().all(Option::is_none) {
            self.note(format!(
                "rack {rack_index} contains only missing plug-in placeholders; dry fallback remains active"
            ));
            return Ok(());
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
        self.workers.insert(
            rack_index,
            RetainedWorker {
                fingerprint: launched.fingerprint,
                banks,
                active_bank_index: 0,
                session: Some(launched.session),
                parameters: launched.parameters,
                loaded_slots: launched.loaded_slots,
                rack: rack.clone(),
                saved_states: retained_states,
                recovery: Arc::new(RackRecoverySignal::new()),
                recovery_failed: false,
                recovery_recorded: false,
                suspected_slot: None,
                latency_samples: 0,
                restart_requested: false,
            },
        );
        self.note(format!(
            "rack {rack_index} worker initialized (generation {generation})"
        ));
        Ok(())
    }

    pub(crate) fn unload_rack(&mut self, rack_index: usize) {
        if let Some(mut worker) = self.workers.remove(&rack_index)
            && let Some(session) = worker.session.take()
        {
            let _ = session.shutdown(&mut self.processes);
        }
        self.open_editors.retain(|(rack, _)| *rack != rack_index);
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
        bypass_slot: Option<usize>,
    ) -> Result<LaunchedWorker, String> {
        let candidates = self.resolve_rack_slots(rack)?;
        let first = candidates
            .iter()
            .flatten()
            .next()
            .ok_or("rack contains no available plug-ins")?;
        self.quarantine
            .ensure_launch_permitted(&first.fingerprint)
            .map_err(|error| error.to_string())?;
        let target = control_target(rack_index, bank_index, generation)?;
        let socket = self
            .app_support
            .join(format!("worker-{rack_index}-{generation}.sock"));
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
        let mut session = WorkerControlSession::launch(&mut self.processes, &launch)
            .map_err(|error| format!("could not launch rack worker: {error}"))?;
        let topology = WireRackTopology {
            slots: candidates
                .iter()
                .enumerate()
                .filter_map(|(slot, candidate)| {
                    let candidate = candidate.as_ref()?;
                    Some(sp_protocol::payload::PluginSlotConfiguration {
                        slot: u8::try_from(slot).expect("model limits slots to u8"),
                        input_channels: candidate.input_channels,
                        output_channels: candidate.output_channels,
                        event_input_active: candidate.event_input_active,
                        bundle_path: candidate.bundle.display().to_string(),
                        class_id: Some(candidate.class_id.clone()),
                    })
                })
                .collect(),
        };
        let payload = topology.encode().map_err(|error| error.to_string())?;
        let response = session
            .client_mut()
            .request(ControlOperation::RebuildRack, None, &payload)
            .map_err(|error| format!("rack rebuild request failed: {error}"))?;
        if response.status() != ControlResponseStatus::Ok {
            let _ = session.shutdown(&mut self.processes);
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
                restore_worker_state(&mut session, slot, state).map_err(|error| {
                    format!("could not restore slot {} state: {error}", slot + 1)
                })?;
            }
            for (slot, model_slot) in rack.slots.iter().enumerate() {
                if candidates.get(slot).is_none_or(Option::is_none) {
                    continue;
                }
                for (parameter_id, value) in &model_slot.parameters.values {
                    let parameter_id = parameter_id.0.parse::<u32>().map_err(|_| {
                        format!("saved parameter ID `{}` is not a VST3 u32", parameter_id.0)
                    })?;
                    write_worker_parameter(&mut session, slot, parameter_id, f64::from(value.0))?;
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
        if let Err(error) = restore {
            let _ = session.shutdown(&mut self.processes);
            return Err(error);
        }
        if let Some(slot) = bypass_slot {
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

    pub(crate) fn capture_plugin_states(
        &mut self,
        model: &sp_model::Session,
    ) -> Result<Vec<CapturedPluginState>, String> {
        let mut captures = Vec::new();
        for (&rack_index, worker) in &mut self.workers {
            let Some(session) = worker.session.as_mut() else {
                continue;
            };
            let rack = model
                .racks
                .get(rack_index)
                .ok_or("running worker has no matching session rack")?;
            for (slot, model_slot) in rack.slots.iter().enumerate() {
                if worker.loaded_slots.get(slot) != Some(&true) {
                    continue;
                }
                let slot_id = slot_identity(slot)?;
                request_ok(
                    session,
                    ControlOperation::DeactivateSlot,
                    Some(slot_id),
                    &[],
                )?;
                let capture =
                    request_ok(session, ControlOperation::CaptureState, Some(slot_id), &[]);
                let reactivate =
                    request_ok(session, ControlOperation::ActivateSlot, Some(slot_id), &[]);
                let response = capture?;
                reactivate?;
                let state = StateRestore::decode(response.payload())
                    .map_err(|error| format!("worker returned invalid state: {error}"))?;
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

    pub(crate) fn generic_parameters(
        &mut self,
        rack_index: usize,
        slot: usize,
    ) -> Result<Vec<GenericParameter>, String> {
        let worker = self
            .workers
            .get_mut(&rack_index)
            .ok_or("rack worker is not running")?;
        let parameters = worker
            .parameters
            .get(slot)
            .ok_or("slot index exceeds loaded rack topology")?
            .clone();
        let session = worker
            .session
            .as_mut()
            .ok_or("rack worker is currently recovering")?;
        parameters
            .into_iter()
            .map(|parameter| {
                let metadata = worker_parameter_metadata(session, slot, parameter.id)?;
                Ok(GenericParameter {
                    id: parameter.id,
                    name: metadata.name,
                    unit: metadata.unit,
                    formatted: metadata.formatted,
                    normalized: metadata.normalized,
                    default_normalized: metadata.default_normalized,
                    read_only: parameter.read_only,
                    discrete: metadata.step_count > 0,
                    step_count: metadata.step_count,
                    automatable: parameter.automatable,
                })
            })
            .collect()
    }

    pub(crate) fn write_parameter(
        &mut self,
        rack_index: usize,
        slot: usize,
        parameter_id: u32,
        normalized: f64,
    ) -> Result<String, String> {
        let worker = self
            .workers
            .get_mut(&rack_index)
            .ok_or("rack worker is not running")?;
        let session = worker
            .session
            .as_mut()
            .ok_or("rack worker is currently recovering")?;
        write_worker_parameter(session, slot, parameter_id, normalized)?;
        let formatted = worker_parameter_metadata(session, slot, parameter_id)?.formatted;
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
        Ok(formatted)
    }

    pub(crate) fn open_native_editor(
        &mut self,
        rack_index: usize,
        slot: usize,
    ) -> Result<(), String> {
        let worker = self
            .workers
            .get_mut(&rack_index)
            .ok_or("rack worker is not running")?;
        let session = worker
            .session
            .as_mut()
            .ok_or("rack worker is currently recovering")?;
        let key = (rack_index, slot);
        let slot = slot_identity(slot)?;
        let operation = if self.open_editors.contains(&key) {
            ControlOperation::CloseNativeEditor
        } else {
            ControlOperation::OpenNativeEditor
        };
        let response = session
            .client_mut()
            .request(operation, Some(slot), &[])
            .map_err(|error| format!("native editor is unavailable: {error}"))?;
        if response.status() == ControlResponseStatus::Ok {
            if operation == ControlOperation::OpenNativeEditor {
                self.open_editors.insert(key);
            } else {
                self.open_editors.remove(&key);
            }
            Ok(())
        } else {
            Err(format!(
                "native editor is unavailable: {:?}",
                response.status()
            ))
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
        self.workers.get(&rack_index).is_some_and(|worker| {
            !worker.recovery_failed && worker.recovery.state() != RackRecoveryState::Idle
        })
    }

    pub(crate) fn rack_latency_samples(&self, rack_index: usize) -> Option<u32> {
        self.workers
            .get(&rack_index)
            .map(|worker| worker.latency_samples)
    }

    pub(crate) fn rack_restart_requested(&self, rack_index: usize) -> bool {
        self.workers
            .get(&rack_index)
            .is_some_and(|worker| worker.restart_requested)
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
            worker.rack = rack.clone();
        }
    }

    /// Opens callback-owned mappings for every retained rack worker before `CoreAudio` starts.
    /// The worker and callback have separate mappings of the same POSIX objects.
    pub(crate) fn audio_bank_mappings(&self) -> Result<Vec<AudioBankMapping>, String> {
        self.workers
            .iter()
            .map(|(&rack_index, worker)| {
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
                    Arc::clone(&worker.recovery),
                ))
            })
            .collect()
    }

    fn recover_worker(&mut self, rack_index: usize) {
        let Some(worker) = self.workers.get_mut(&rack_index) else {
            return;
        };
        if !worker.recovery.request_quiesce() {
            return;
        }
        self.record_worker_fault(rack_index);
    }

    fn record_worker_fault(&mut self, rack_index: usize) {
        let Some(worker) = self.workers.get_mut(&rack_index) else {
            return;
        };
        if worker.recovery_recorded {
            return;
        }
        worker.recovery_recorded = true;
        worker.suspected_slot = worker.session.as_mut().and_then(|session| {
            session
                .client_mut()
                .request(ControlOperation::QuerySlotAttribution, None, &[])
                .ok()
                .filter(|response| response.status() == ControlResponseStatus::Ok)
                .and_then(|response| response.payload().try_into().ok().map(u64::from_le_bytes))
                .and_then(|slot| usize::try_from(slot.checked_sub(1)?).ok())
                .filter(|&slot| slot < worker.rack.slots.len())
        });
        let fallback_fingerprint = worker.fingerprint.clone();
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
            .unwrap_or(fallback_fingerprint);
        self.open_editors.retain(|(rack, _)| *rack != rack_index);
        let _ = self
            .quarantine
            .record_failure(fingerprint, PluginFailureKind::WorkerHang);
        self.note(format!(
            "rack {rack_index} entered dry fallback while its worker is replaced"
        ));
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
                RackRecoveryState::Quiescent => self.launch_replacement(rack_index),
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

    fn launch_replacement(&mut self, rack_index: usize) {
        let Some(mut worker) = self.workers.remove(&rack_index) else {
            return;
        };
        if worker.recovery_failed {
            self.workers.insert(rack_index, worker);
            return;
        }
        if let Some(session) = worker.session.take() {
            let _ = session.shutdown(&mut self.processes);
        }
        let replacement_index = worker.active_bank_index ^ 1;
        let generation = next_bank_generation(&worker.banks);
        let reset = reset_reaped_region(&mut worker.banks[replacement_index], generation);
        let launched = reset
            .map_err(|error| format!("cannot reset replacement bank: {error}"))
            .and_then(|()| {
                self.launch_worker(
                    rack_index,
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
                worker.parameters = launched.parameters;
                worker.loaded_slots = launched.loaded_slots;
                worker.active_bank_index = replacement_index;
                worker.suspected_slot = None;
                if worker
                    .recovery
                    .publish_replacement(replacement_index, generation)
                {
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
            worker.recovery_failed = true;
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
                worker.recovery_failed = true;
                self.note(format!(
                    "rack {rack_index} retired-bank handoff was rejected"
                ));
            }
            Err(error) => {
                worker.recovery_failed = true;
                self.note(format!(
                    "rack {rack_index} could not reset retired bank: {error}"
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
    let payload = StateRestore {
        component: state.component.clone(),
        controller: state.controller.clone(),
    }
    .encode()
    .map_err(|error| error.to_string())?;
    request_ok(
        session,
        ControlOperation::RestoreState,
        Some(slot),
        &payload,
    )?;
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

fn worker_parameter_metadata(
    session: &mut WorkerControlSession,
    slot: usize,
    parameter_id: u32,
) -> Result<WireParameterMetadata, String> {
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
    WireParameterMetadata::decode(response.payload()).map_err(|error| error.to_string())
}

impl Drop for ProductRuntime {
    fn drop(&mut self) {
        for (_, mut worker) in std::mem::take(&mut self.workers) {
            if let Some(session) = worker.session.take() {
                let _ = session.shutdown(&mut self.processes);
            }
        }
    }
}

impl HelperBinaries {
    fn resolve() -> Result<Self, String> {
        let directory = std::env::var_os("SUPERPOSITION_HELPERS_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::current_exe()
                    .ok()
                    .and_then(|exe| exe.parent().map(|parent| parent.join("../Helpers")))
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
    let session_root = default_session_root();
    let controller = SessionController::open(&session_root)
        .unwrap_or_else(|_| SessionController::empty(&session_root));
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
