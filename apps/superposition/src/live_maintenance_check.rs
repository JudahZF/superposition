//! Opt-in worker-maintenance check using real plug-ins and synthetic audio blocks.

use std::{
    env, fs,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use sp_audio_io_macos::{RackAutomationEvents, RackSharedMemoryDispatcher};
use sp_engine::RackAudioSource;
use sp_model::{ParameterId, PluginInstanceId, RackId};
use sp_session::{CapturedPluginState, SessionController};
use sp_shared_memory_macos::RackRecoveryState;
use sp_supervisor::{PluginCatalog, Scanner};

use crate::ProductRuntime;

const PRO_Q_4_CLASS_ID: &str = "ED57BD725C60467EA64DD2F400758B6F";
const OUTPUT_LEVEL_ID: u32 = 556;
const FRAMES: usize = 64;
const MAX_PHASE_TIME: Duration = Duration::from_secs(15);
const MAX_POLL_TIME: Duration = Duration::from_millis(200);

fn required_env(name: &str) -> String {
    env::var(name).unwrap_or_else(|_| panic!("{name} must be set for maintenance check"))
}

fn approved_temp_path(path: &Path) -> PathBuf {
    let canonical = fs::canonicalize(path).expect("approved fixture path exists");
    assert!(
        canonical.starts_with("/private/tmp") || canonical.starts_with("/tmp"),
        "maintenance fixtures must stay under /tmp"
    );
    canonical
}

fn process_id(product: &ProductRuntime, rack: usize) -> u64 {
    product.workers[&rack]
        .session
        .as_ref()
        .expect("rack worker has a control session")
        .process_id()
}

fn bank_identity(product: &ProductRuntime, rack: usize) -> (usize, u64) {
    let worker = &product.workers[&rack];
    let index = worker.active_bank_index;
    (index, worker.banks[index].generation())
}

fn drive_block(dispatcher: &mut RackSharedMemoryDispatcher) {
    let mut inputs: Box<[[f32; sp_engine::MAX_MIX_FRAMES * 2]; sp_model::MAX_RACKS]> =
        vec![[0.0; sp_engine::MAX_MIX_FRAMES * 2]; sp_model::MAX_RACKS]
            .into_boxed_slice()
            .try_into()
            .expect("one input per rack");
    for rack in inputs.iter_mut().take(2) {
        for frame in rack.chunks_exact_mut(2).take(FRAMES) {
            frame.copy_from_slice(&[0.1, -0.1]);
        }
    }
    let automation = std::array::from_fn(|_| RackAutomationEvents::new());
    dispatcher.process_block_with_rack_inputs(
        &inputs,
        None,
        &[],
        &automation,
        FRAMES,
        Duration::from_millis(20),
    );
}

fn wet_peak(dispatcher: &RackSharedMemoryDispatcher, rack: usize) -> f32 {
    match dispatcher.sources()[rack] {
        RackAudioSource::Wet(samples) => samples
            .iter()
            .take(FRAMES * 2)
            .map(|sample| sample.abs())
            .fold(0.0_f32, f32::max),
        RackAudioSource::None => 0.0,
    }
}

fn retain_state(directory: &Path, label: &str, state: &CapturedPluginState) {
    fs::write(
        directory.join(format!("{label}-component.bin")),
        &state.component,
    )
    .expect("retain component state evidence");
    fs::write(
        directory.join(format!("{label}-controller.bin")),
        &state.controller,
    )
    .expect("retain controller state evidence");
}

#[test]
#[ignore = "launches approved Pro-Q workers; no GUI or audio device is opened"]
#[allow(
    clippy::too_many_lines,
    reason = "one opt-in real-worker lifecycle keeps setup, transitions, and evidence together"
)]
fn planned_maintenance_preserves_state_and_other_rack() {
    assert_eq!(required_env("SUPERPOSITION_MAINTENANCE_CHECK"), "1");
    let session_path = approved_temp_path(Path::new(&required_env(
        "SUPERPOSITION_MAINTENANCE_SESSION",
    )));
    let artifact_root = approved_temp_path(Path::new(&required_env(
        "SUPERPOSITION_MAINTENANCE_ARTIFACT_ROOT",
    )));
    let helpers = PathBuf::from(required_env("SUPERPOSITION_HELPERS_DIR"));
    assert!(helpers.is_absolute() && helpers.is_dir());
    for name in ["sp-plugin-worker", "sp-plugin-scanner"] {
        assert!(helpers.join(name).is_file(), "missing helper {name}");
    }
    let bundle = PathBuf::from(required_env("SUPERPOSITION_MAINTENANCE_PLUGIN_BUNDLE"));
    assert!(bundle.is_absolute() && bundle.is_dir());
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    // Keep room for the worker socket name within macOS's Unix socket path limit.
    let isolated = artifact_root.join(format!("m{unique:x}"));
    fs::create_dir(&isolated).expect("new isolated artifact directory");
    let app_support = isolated.join("s");
    fs::create_dir(&app_support).expect("isolated application support");
    let catalog = PluginCatalog::open(app_support.join("plugin-catalog.json")).expect("catalog");
    let mut scanner = Scanner::with_default_timeout(helpers.join("sp-plugin-scanner"), catalog);
    let scan = scanner.scan(&bundle).expect("scan approved Pro-Q bundle");
    assert!(scan.is_supported(), "Pro-Q scan must be supported");

    let controller = SessionController::open(&session_path).expect("open approved fixture");
    let mut model = controller.document().model.clone();
    assert_eq!(model.racks.len(), 1, "fixture must have one Pro-Q rack");
    assert_eq!(
        model.racks[0].slots.len(),
        1,
        "fixture must have one Pro-Q slot"
    );
    let original = &model.racks[0].slots[0];
    assert_eq!(original.plugin.identity.vendor, "FabFilter");
    assert_eq!(original.plugin.identity.name, "Pro-Q 4");
    assert_eq!(original.plugin.identity.unique_id, PRO_Q_4_CLASS_ID);
    assert_eq!(original.plugin.fingerprint.digest, scan.fingerprint.digest);
    let saved = controller
        .load_plugin_state(&original.id.0)
        .expect("load fixture state")
        .map(|(component, controller, metadata)| CapturedPluginState {
            instance_id: original.id.0.clone(),
            component,
            controller,
            metadata,
        });
    let mut second = model.racks[0].clone();
    second.id = RackId("maintenance-untouched".to_owned());
    second.name = "Untouched Pro-Q rack".to_owned();
    second.slots[0].id = PluginInstanceId("maintenance-untouched-slot".to_owned());
    let second_state = saved.clone().map(|mut state| {
        state.instance_id = second.slots[0].id.0.clone();
        state
    });
    model.racks.push(second);
    let mut requested_slot = model.racks[0].slots[0].clone();
    requested_slot.id = PluginInstanceId("maintenance-requested-slot".to_owned());
    let requested_state = saved.clone().map(|mut state| {
        state.instance_id = requested_slot.id.0.clone();
        state
    });
    model.racks[0].slots.push(requested_slot);
    model
        .validate_for_alpha()
        .expect("two-rack, three-slot fixture is valid");

    let mut product = ProductRuntime::open(app_support).expect("open isolated product");
    product
        .load_rack(0, &model.racks[0], &[saved.clone(), requested_state])
        .expect("load two-slot target Pro-Q rack");
    product
        .load_rack(1, &model.racks[1], &[second_state])
        .expect("load unaffected Pro-Q rack");
    let mappings = product.audio_bank_mappings().expect("map both rack banks");
    let mut dispatcher = RackSharedMemoryDispatcher::with_rack_banks(mappings)
        .expect("one synthetic dispatcher owns both rack selectors");

    product
        .write_parameter(0, 0, OUTPUT_LEVEL_ID, 0.5)
        .expect("set target slot 1 Output Level");
    product
        .write_parameter(0, 1, OUTPUT_LEVEL_ID, 0.475)
        .expect("set target slot 2 Output Level");
    product
        .write_parameter(1, 0, OUTPUT_LEVEL_ID, 0.45)
        .expect("set unaffected Output Level");
    let pre_audio_states = product
        .capture_rack_states(0)
        .expect("capture both edited target slot states");
    assert_eq!(
        pre_audio_states.len(),
        2,
        "both Pro-Q slots must capture state"
    );
    retain_state(&isolated, "before-audio", &pre_audio_states[1]);
    let untouched_pid = process_id(&product, 1);
    let untouched_identity = bank_identity(&product, 1);
    let initial_quarantine = product.quarantined_count();
    let mut max_poll = Duration::ZERO;
    let mut failures = Vec::new();

    let mut warmup_wet = [0.0_f32; 2];
    for _ in 0..8 {
        drive_block(&mut dispatcher);
        for (rack, peak) in warmup_wet.iter_mut().enumerate() {
            *peak = (*peak).max(wet_peak(&dispatcher, rack));
        }
        product.poll();
    }
    let initial_target = dispatcher.telemetry(0).expect("target telemetry").completed;
    let initial_other = dispatcher.telemetry(1).expect("other telemetry").completed;
    if initial_target == 0 || initial_other == 0 {
        failures.push("both real workers must complete synthetic warmup blocks".to_owned());
    }
    if warmup_wet.contains(&0.0) {
        failures.push("both real workers must render nonzero wet audio".to_owned());
    }
    println!(
        "maintenance_warmup: target_completed={initial_target} other_completed={initial_other} wet_peaks={warmup_wet:?} restart_flags={} maintenance_pending={} diagnostics={:?}",
        product.rack_restart_flags(0),
        product.maintenance_pending(),
        product.diagnostics()
    );
    if product.maintenance_pending()
        || product.workers[&0].recovery.state() != RackRecoveryState::Idle
    {
        failures.push("Pro-Q requested maintenance before test injection".to_owned());
    }

    // The processor must receive queued parameter edits before its opaque state is the baseline.
    let fresh_states = product
        .capture_rack_states(0)
        .expect("capture target state after synthetic processing");
    assert_eq!(fresh_states.len(), 2, "both Pro-Q slots must capture state");
    assert!(
        fresh_states[0].component != fresh_states[1].component
            || fresh_states[0].controller != fresh_states[1].controller,
        "distinct Output Levels must produce distinguishable opaque states"
    );
    retain_state(&isolated, "baseline", &fresh_states[1]);

    for flags in [1_u32, 2, 8] {
        let before = bank_identity(&product, 0);
        let target_before = dispatcher.telemetry(0).expect("target telemetry").completed;
        let other_before = dispatcher.telemetry(1).expect("other telemetry").completed;
        let bank = &product.workers[&0].banks[before.0];
        if !bank.bank().feedback.publish_restart(1, flags) {
            failures.push(format!("feedback rejected restart flags 0x{flags:X}"));
            break;
        }
        let started = Instant::now();
        let mut completed = false;
        let mut saw_requested_slot = false;
        let mut saw_wrong_slot = false;
        while started.elapsed() < MAX_PHASE_TIME {
            drive_block(&mut dispatcher);
            let poll_started = Instant::now();
            product.poll();
            max_poll = max_poll.max(poll_started.elapsed());
            let current = bank_identity(&product, 0);
            let worker = &product.workers[&0];
            if let Some(maintenance) = &worker.maintenance {
                saw_requested_slot |= maintenance
                    .requests
                    .iter()
                    .any(|request| request.slot_index == 1 && request.flags & flags != 0);
                saw_wrong_slot |= maintenance
                    .requests
                    .iter()
                    .any(|request| request.slot_index == 0 && request.flags & flags != 0);
            }
            if current.0 != before.0
                && current.1 > before.1
                && worker.recovery.state() == RackRecoveryState::Idle
                && worker.maintenance.is_none()
            {
                completed = true;
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        let after = bank_identity(&product, 0);
        let other_after = dispatcher.telemetry(1).expect("other telemetry").completed;
        println!(
            "maintenance: flags=0x{flags:X} before={before:?} after={after:?} elapsed={:?} other_completed={other_before}->{other_after} max_poll={max_poll:?} target={:?} other={:?}",
            started.elapsed(),
            dispatcher.telemetry(0),
            dispatcher.telemetry(1)
        );
        if !completed {
            failures.push(format!("flags 0x{flags:X} did not complete a bank handoff"));
            break;
        }
        if !saw_requested_slot || saw_wrong_slot {
            failures.push(format!(
                "restart flags 0x{flags:X} were attributed to the wrong Pro-Q slot"
            ));
        }
        if other_after <= other_before || process_id(&product, 1) != untouched_pid {
            failures.push(format!(
                "unaffected rack stopped or relaunched for flags 0x{flags:X}"
            ));
        }
        if bank_identity(&product, 1) != untouched_identity {
            failures.push(format!(
                "unaffected rack changed bank for flags 0x{flags:X}"
            ));
        }
        let mut target_wet = 0.0_f32;
        for _ in 0..4 {
            drive_block(&mut dispatcher);
            target_wet = target_wet.max(wet_peak(&dispatcher, 0));
            product.poll();
        }
        if dispatcher.telemetry(0).expect("target telemetry").completed <= target_before {
            failures.push(format!(
                "target rack stopped dispatching for flags 0x{flags:X}"
            ));
        }
        if target_wet == 0.0 {
            failures.push(format!(
                "target rack rendered no wet audio after flags 0x{flags:X}"
            ));
        }
        let worker = &product.workers[&0];
        let saved_state = worker.saved_states[1].as_ref();
        if let Some(state) = saved_state {
            retain_state(&isolated, &format!("captured-{flags}"), state);
        }
        if saved_state.is_none_or(|state| {
            state.component != fresh_states[1].component
                || state.controller != fresh_states[1].controller
        }) {
            failures.push(format!(
                "requested slot 2 opaque state was not retained for flags 0x{flags:X}"
            ));
        }
        if worker.rack.slots[1]
            .parameters
            .values
            .get(&ParameterId(OUTPUT_LEVEL_ID.to_string()))
            .is_none_or(|value| (value.get() - 0.475).abs() > 1e-5)
        {
            failures.push(format!(
                "requested slot 2 parameter was not retained for flags 0x{flags:X}"
            ));
        }
        if worker.rack.slots[0]
            .parameters
            .values
            .get(&ParameterId(OUTPUT_LEVEL_ID.to_string()))
            .is_none_or(|value| (value.get() - 0.5).abs() > 1e-5)
        {
            failures.push(format!(
                "target slot 1 parameter changed for flags 0x{flags:X}"
            ));
        }
        let target = product.workers.get_mut(&0).expect("target worker");
        let bank_target = target
            .session
            .as_mut()
            .expect("target control session")
            .client_mut()
            .target()
            .bank();
        if usize::from(bank_target.index()) != after.0 || bank_target.generation() != after.1 {
            failures.push(format!(
                "control bank identity is stale for flags 0x{flags:X}"
            ));
        }
        match product.read_worker_parameter(0, 1, OUTPUT_LEVEL_ID) {
            Ok(value) if (value - 0.475).abs() < 1e-5 => {}
            Ok(_) => failures.push(format!(
                "replacement worker did not restore requested slot 2 Output Level for flags 0x{flags:X}"
            )),
            Err(error) => failures.push(format!(
                "replacement worker could not read requested slot 2 Output Level for flags 0x{flags:X}: {error}"
            )),
        }
        match product.read_worker_parameter(0, 0, OUTPUT_LEVEL_ID) {
            Ok(value) if (value - 0.5).abs() < 1e-5 => {}
            Ok(_) => failures.push(format!(
                "target slot 1 Output Level changed for flags 0x{flags:X}"
            )),
            Err(error) => failures.push(format!(
                "target slot 1 could not read Output Level for flags 0x{flags:X}: {error}"
            )),
        }
        match product.read_worker_parameter(1, 0, OUTPUT_LEVEL_ID) {
            Ok(value) if (value - 0.45).abs() < 1e-5 => {}
            Ok(_) => failures.push(format!(
                "unaffected rack Output Level changed for flags 0x{flags:X}"
            )),
            Err(error) => failures.push(format!(
                "unaffected rack could not read Output Level for flags 0x{flags:X}: {error}"
            )),
        }
        match product.capture_rack_states(0) {
            Ok(states)
                if states.get(1).is_some_and(|state| {
                    state.component == fresh_states[1].component
                        && state.controller == fresh_states[1].controller
                }) => {}
            Ok(_) => failures.push(format!(
                "replacement worker did not restore requested slot 2 opaque state for flags 0x{flags:X}"
            )),
            Err(error) => failures.push(format!(
                "replacement worker could not capture opaque state for flags 0x{flags:X}: {error}"
            )),
        }
    }

    let target_healthy = product.worker_running(0)
        && !product.worker_recovering(0)
        && !product.worker_recovery_failed(0)
        && !product.planned_maintenance_failed(0);
    let other_healthy = product.worker_running(1)
        && !product.worker_recovering(1)
        && !product.worker_recovery_failed(1);
    let quarantine = product.quarantined_count();
    let fault_budget = product.workers[&0].restart_budget.failures;
    let other_fault_budget = product.workers[&1].restart_budget.failures;
    let maintenance_budget = product.workers[&0].maintenance_budget.failures;
    println!(
        "maintenance_final: target_healthy={target_healthy} other_healthy={other_healthy} quarantine={quarantine} fault_budget={fault_budget} other_fault_budget={other_fault_budget} maintenance_budget={maintenance_budget} max_poll={max_poll:?} diagnostics={:?}",
        product.diagnostics()
    );
    drop(dispatcher);
    drop(product);

    assert!(failures.is_empty(), "{}", failures.join("; "));
    assert!(
        target_healthy && other_healthy,
        "workers must remain healthy"
    );
    assert_eq!(
        quarantine, initial_quarantine,
        "planned maintenance is not a fault"
    );
    assert_eq!(
        fault_budget, 0,
        "planned maintenance must not use crash budget"
    );
    assert_eq!(
        other_fault_budget, 0,
        "other rack must not use crash budget"
    );
    assert!(
        (1..=3).contains(&maintenance_budget),
        "planned maintenance should use only its own budget"
    );
    assert!(max_poll < MAX_POLL_TIME, "UI poll blocked for {max_poll:?}");
}
