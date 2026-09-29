//! Off-UI control work for a quiescent rack. The app keeps child-process ownership.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
};

use sp_protocol::{
    control::ControlOperation,
    payload::{MAX_PARAMETER_BATCH_SIZE, ParameterId as WireParameterId},
};
use sp_session::{
    CapturedPluginState, PluginActivationMetadata, PluginActivationState, PluginStateMetadata,
};
use sp_shared_memory_macos::RackRecoverySignal;
use sp_supervisor::{WorkerControlLaunch, WorkerControlSession};

use crate::{
    LaunchedWorker, ResolvedSlot, configure_worker_session, request_ok, slot_identity,
    worker_parameter_values,
};

pub(super) struct Input {
    pub rack_index: usize,
    pub task_id: u64,
    pub generation: u64,
    pub recovery: Arc<RackRecoverySignal>,
    pub rack: sp_model::Rack,
    pub loaded_slots: Vec<bool>,
    pub parameters: Vec<Vec<sp_model::PluginParameterMetadata>>,
    pub candidates: Vec<Option<ResolvedSlot>>,
    pub launch: WorkerControlLaunch,
    pub cancel: Arc<AtomicBool>,
}

pub(super) struct Ready {
    pub launched: LaunchedWorker,
    pub rack: sp_model::Rack,
    pub states: Vec<Option<CapturedPluginState>>,
}

pub(super) enum Event {
    Spawn {
        rack_index: usize,
        task_id: u64,
        generation: u64,
        recovery: Arc<RackRecoverySignal>,
        launch: WorkerControlLaunch,
        reply: Sender<Result<u64, String>>,
    },
    Finished {
        rack_index: usize,
        task_id: u64,
        generation: u64,
        recovery: Arc<RackRecoverySignal>,
        old_session: WorkerControlSession,
        result: Box<Result<Ready, String>>,
    },
}

pub(super) fn start(
    input: Input,
    old_session: WorkerControlSession,
    events: Sender<Event>,
) -> Result<(), Box<(String, WorkerControlSession)>> {
    let (sender, receiver) = mpsc::channel();
    let spawn = thread::Builder::new()
        .name(format!("sp-rack-maintenance-{}", input.rack_index + 1))
        .spawn(move || run(input, &receiver, &events));
    if let Err(error) = spawn {
        return Err(Box::new((
            format!("could not start rack maintenance task: {error}"),
            old_session,
        )));
    }
    sender.send(old_session).map_err(|error| {
        Box::new((
            "rack maintenance task ended before receiving its worker".to_owned(),
            error.0,
        ))
    })
}

fn run(mut input: Input, receiver: &Receiver<WorkerControlSession>, events: &Sender<Event>) {
    let Ok(mut old_session) = receiver.recv() else {
        return;
    };
    let result = run_inner(&mut input, &mut old_session, events);
    let _ = events.send(Event::Finished {
        rack_index: input.rack_index,
        task_id: input.task_id,
        generation: input.generation,
        recovery: input.recovery,
        old_session,
        result: Box::new(result),
    });
}

fn run_inner(
    input: &mut Input,
    old_session: &mut WorkerControlSession,
    events: &Sender<Event>,
) -> Result<Ready, String> {
    cancelled(&input.cancel)?;
    let states =
        capture_inactive_states(old_session, &input.rack, &input.loaded_slots, &input.cancel)?;
    let rack = refresh_parameters(
        old_session,
        &input.rack,
        &input.loaded_slots,
        &input.parameters,
        &input.cancel,
    )?;
    cancelled(&input.cancel)?;
    let (reply, receiver) = mpsc::channel();
    events
        .send(Event::Spawn {
            rack_index: input.rack_index,
            task_id: input.task_id,
            generation: input.generation,
            recovery: Arc::clone(&input.recovery),
            launch: input.launch.clone(),
            reply,
        })
        .map_err(|_| "app stopped during worker launch".to_owned())?;
    let process_id = receiver
        .recv()
        .map_err(|_| "app stopped during worker launch".to_owned())??;
    cancelled(&input.cancel)?;
    let session = WorkerControlSession::connect_launched(process_id, &input.launch)
        .map_err(|error| format!("replacement worker connection failed: {error}"))?;
    let mut launched = configure_worker_session(session, &rack, &input.candidates, &states, None)?;
    // The worker publishes lifecycle flags between control requests. This request fences the
    // final activation before the app observes the replacement bank's startup flags.
    request_ok(
        &mut launched.session,
        ControlOperation::QueryHealth,
        None,
        &[],
    )?;
    cancelled(&input.cancel)?;
    Ok(Ready {
        launched,
        rack,
        states,
    })
}

fn cancelled(flag: &AtomicBool) -> Result<(), String> {
    if flag.load(Ordering::Acquire) {
        Err("rack maintenance was cancelled".to_owned())
    } else {
        Ok(())
    }
}

fn capture_inactive_states(
    session: &mut WorkerControlSession,
    rack: &sp_model::Rack,
    loaded_slots: &[bool],
    cancel: &AtomicBool,
) -> Result<Vec<Option<CapturedPluginState>>, String> {
    let mut states = vec![None; rack.slots.len()];
    for (slot_index, slot) in rack.slots.iter().enumerate() {
        if loaded_slots.get(slot_index) != Some(&true) {
            continue;
        }
        cancelled(cancel)?;
        let slot_id = slot_identity(slot_index)?;
        request_ok(
            session,
            ControlOperation::DeactivateSlot,
            Some(slot_id),
            &[],
        )
        .map_err(|error| format!("slot {} deactivation failed: {error}", slot_index + 1))?;
        let state = session
            .client_mut()
            .capture_state(slot_id)
            .map_err(|error| format!("slot {} state capture failed: {error}", slot_index + 1))?;
        let fingerprint = slot.plugin.fingerprint.digest.clone();
        states[slot_index] = Some(CapturedPluginState {
            instance_id: slot.id.0.clone(),
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
        });
    }
    Ok(states)
}

fn refresh_parameters(
    session: &mut WorkerControlSession,
    rack: &sp_model::Rack,
    loaded_slots: &[bool],
    parameters: &[Vec<sp_model::PluginParameterMetadata>],
    cancel: &AtomicBool,
) -> Result<sp_model::Rack, String> {
    let mut refreshed = rack.clone();
    for (slot_index, slot) in refreshed.slots.iter_mut().enumerate() {
        if loaded_slots.get(slot_index) != Some(&true) {
            continue;
        }
        let known = parameters
            .get(slot_index)
            .ok_or("loaded slot has no parameter metadata")?;
        for batch in known.chunks(MAX_PARAMETER_BATCH_SIZE) {
            cancelled(cancel)?;
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
                let id = u32::try_from(parameter.id.value)
                    .map_err(|_| "worker returned an invalid parameter ID")?;
                slot.parameters
                    .values
                    .insert(sp_model::ParameterId(id.to_string()), value);
            }
        }
    }
    Ok(refreshed)
}
