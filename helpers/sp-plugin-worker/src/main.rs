//! Isolated plug-in-host helper entry point.

mod control_thread;
#[cfg(feature = "sdk")]
mod state_transfer;
mod worker_runtime;

use std::{
    cmp, io,
    os::unix::{fs::FileTypeExt, net::UnixListener},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

#[cfg(feature = "sdk")]
use sp_protocol::{
    WORKER_PHASE_CONTROL, WORKER_PHASE_SCANNING, WORKER_PHASE_STOPPING, WORKER_PHASE_WAITING,
    control::{
        BankIdentity, ControlErrorCode, ControlErrorRecord, ControlOperation, ControlRequest,
        ControlResponse, ControlResponseStatus, ControlTarget, RackIdentity, SlotIdentity,
    },
    payload::{
        Bypass, ControlPayloadCodec, EditorGeometry, EditorPosition, EditorPreviewDescriptor,
        EditorPreviewRequest, HealthReport, ParameterId, ParameterIds, ParameterMetadata,
        ParameterValues, ParameterWrite, PluginSlotConfiguration as WirePluginSlotConfiguration,
        RackTopology, RestartReport, SlotOrder, StateChunkRequest, StateChunkWrite, StateRestore,
        StateTransferDescriptor, StateTransferId, StateTransferLengths,
    },
};
use sp_shared_memory::{
    BLOCK_SLOT_COUNT, BlockRequest, BlockSlot, BlockTicket, MAX_PLUGINS_PER_RACK,
    ParameterFeedbackBank, ProtocolError, ProtocolHeader, SharedBank, SlotState,
};
use sp_shared_memory_macos::{MonotonicClock, SharedMemoryRegion};
use sp_vst3::{Vst3BundlePath, adapter::Vst3ClassSelection};
#[cfg(feature = "sdk")]
use std::collections::BTreeMap;
#[cfg(feature = "sdk")]
use std::sync::mpsc::{
    Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError, sync_channel,
};

#[cfg(feature = "sdk")]
use crate::{
    control_thread::{
        ControlThread, ProcessingControlEndpoint, ProcessingControlHandler, control_mailbox,
    },
    state_transfer::StateTransfers,
    worker_runtime::{
        PlanarBlock, PluginFacade, PluginProcessRequest, PluginRuntimeError,
        PluginSlotConfiguration, PluginState, PluginTopology, RackProcessor,
    },
};

#[cfg(feature = "sdk")]
use sp_vst3::{
    adapter::{
        BoundedMidiEvents, BoundedOutputChanges, BoundedParameterChanges, MainBusLayout,
        MidiMessage, PlanarAudioInput, PlanarAudioOutput, PluginProcessBlock, ProcessingFormat,
        RackPluginAdapter, TimedMidiMessage, TimedParameterChange, Vst3StateStreams,
    },
    sdk::HostSdkRackAdapter,
};

#[cfg(feature = "sdk")]
use sp_vst3::sdk::SdkPlugin;

const WORKER_ID_OPTION: &str = "--worker-id";
const BUNDLE_OPTION: &str = "--bundle";
/// Fixed readiness record emitted only after the initial shared-memory heartbeat publication.
const WORKER_READY_LINE: &str = "ready";
const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(10);

/// Standalone one-bank worker used by `cargo xtask host-checker`.
///
/// It serves timed block requests from one shared-memory bank and has no control socket.
#[derive(Clone, Debug, Eq, PartialEq)]
struct StandaloneWorkerConfiguration {
    bank_name: String,
    worker_id: u32,
    /// Optional VST3 bundle. When set (and `sdk` is enabled), blocks are processed
    /// through the plug-in instead of a pass-through copy.
    bundle: Option<Vst3BundlePath>,
}

/// Production worker configuration. Unlike standalone mode, this is entered only by the
/// explicit `--worker` selector and always has a concrete VST3 class and control endpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ProductionWorkerConfiguration {
    bank_name: String,
    worker_id: u32,
    rack_index: u8,
    rack_generation: u64,
    bank_index: u8,
    bank_generation: u64,
    control_socket: PathBuf,
    selection: Vst3ClassSelection,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum WorkerStartup {
    Standalone(StandaloneWorkerConfiguration),
    Production(ProductionWorkerConfiguration),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HeartbeatPublisher {
    clock: MonotonicClock,
    interval_ticks: u64,
    last_published_tick: u64,
}

impl HeartbeatPublisher {
    fn new(clock: MonotonicClock) -> Self {
        Self {
            clock,
            interval_ticks: clock.duration_to_ticks(HEARTBEAT_INTERVAL),
            last_published_tick: 0,
        }
    }

    fn publish_now(&mut self, header: &ProtocolHeader) -> Result<(), String> {
        let tick = self.clock.now_ticks();
        header
            .publish_worker_heartbeat(tick)
            .map_err(|error| format!("could not publish worker heartbeat: {error}"))?;
        self.last_published_tick = tick;
        Ok(())
    }

    fn publish_if_due(&mut self, header: &ProtocolHeader) -> Result<(), String> {
        let tick = self.clock.now_ticks();
        if heartbeat_due(self.last_published_tick, tick, self.interval_ticks) {
            header
                .publish_worker_heartbeat(tick)
                .map_err(|error| format!("could not publish worker heartbeat: {error}"))?;
            self.last_published_tick = tick;
        }
        Ok(())
    }
}

fn main() {
    let startup = match parse_worker_startup(std::env::args().skip(1)) {
        Ok(startup) => startup,
        Err(error) => exit_with_error(&error),
    };
    let Some(startup) = startup else {
        println!(
            "usage: sp-plugin-worker --worker --bank <shared-memory-bank> --worker-id <id> --rack-index <0..7> --rack-generation <generation> --bank-index <0..1> --bank-generation <generation> --control-socket <absolute-path> --bundle <path> --class-id <id>"
        );
        return;
    };
    let result = match startup {
        WorkerStartup::Standalone(configuration) => run_standalone_worker(&configuration),
        WorkerStartup::Production(configuration) => run_production_worker(configuration),
    };
    if let Err(error) = result {
        exit_with_error(&error);
    }
}

fn parse_worker_startup<I, S>(arguments: I) -> Result<Option<WorkerStartup>, String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let arguments: Vec<String> = arguments.into_iter().map(Into::into).collect();
    match arguments.first().map(String::as_str) {
        None => Ok(None),
        Some("--worker") => parse_production_worker_arguments(&arguments[1..])
            .map(WorkerStartup::Production)
            .map(Some),
        Some(_) => parse_standalone_arguments(&arguments)
            .map(WorkerStartup::Standalone)
            .map(Some),
    }
}

fn exit_with_error(error: &str) -> ! {
    eprintln!("sp-plugin-worker: {error}");
    std::process::exit(1);
}

fn parse_standalone_arguments(
    arguments: &[String],
) -> Result<StandaloneWorkerConfiguration, String> {
    if !arguments.len().is_multiple_of(2) {
        return Err("standalone worker options must be option/value pairs".to_owned());
    }
    let mut bank_name = None;
    let mut worker_id = None;
    let mut bundle = None;
    for pair in arguments.chunks_exact(2) {
        let option = &pair[0];
        let value = &pair[1];
        let duplicate = || format!("duplicate standalone worker option `{option}`");
        match option.as_str() {
            "--bank" => {
                if bank_name.replace(value.clone()).is_some() || value.is_empty() {
                    return Err(duplicate());
                }
            }
            WORKER_ID_OPTION => {
                if worker_id.replace(parse_worker_id(value)?).is_some() {
                    return Err(duplicate());
                }
            }
            BUNDLE_OPTION => {
                if bundle.replace(Vst3BundlePath::new(value.clone())).is_some() {
                    return Err(duplicate());
                }
            }
            _ => return Err(format!("unknown argument `{option}`")),
        }
    }
    Ok(StandaloneWorkerConfiguration {
        bank_name: bank_name.ok_or("standalone worker requires --bank")?,
        worker_id: worker_id.ok_or_else(|| format!("missing required `{WORKER_ID_OPTION}`"))?,
        bundle,
    })
}

#[allow(
    clippy::too_many_lines,
    reason = "flat option/value parsing reads clearest as one sequence"
)]
fn parse_production_worker_arguments(
    arguments: &[String],
) -> Result<ProductionWorkerConfiguration, String> {
    if !arguments.len().is_multiple_of(2) {
        return Err("production worker options must be option/value pairs".to_owned());
    }
    let mut bank_name = None;
    let mut worker_id = None;
    let mut rack_index = None;
    let mut rack_generation = None;
    let mut bank_index = None;
    let mut bank_generation = None;
    let mut control_socket = None;
    let mut bundle = None;
    let mut class_id = None;
    for pair in arguments.chunks_exact(2) {
        let option = &pair[0];
        let value = &pair[1];
        let duplicate = |name: &str| format!("duplicate production worker option `{name}`");
        match option.as_str() {
            "--bank" => {
                if bank_name.replace(value.clone()).is_some() || value.is_empty() {
                    return Err(duplicate(option));
                }
            }
            WORKER_ID_OPTION => {
                if worker_id.is_some() {
                    return Err(duplicate(option));
                }
                worker_id = Some(parse_worker_id(value)?);
            }
            "--rack-index" => {
                if rack_index.is_some() {
                    return Err(duplicate(option));
                }
                rack_index = Some(
                    value
                        .parse::<u8>()
                        .ok()
                        .filter(|index| *index < 8)
                        .ok_or("rack index must be an unsigned integer in 0..7")?,
                );
            }
            "--rack-generation" => {
                if rack_generation.is_some() {
                    return Err(duplicate(option));
                }
                rack_generation = Some(parse_nonzero_generation(value, "rack generation")?);
            }
            "--bank-index" => {
                if bank_index.is_some() {
                    return Err(duplicate(option));
                }
                bank_index = Some(
                    value
                        .parse::<u8>()
                        .ok()
                        .filter(|index| *index < 2)
                        .ok_or("bank index must be an unsigned integer in 0..1")?,
                );
            }
            "--bank-generation" => {
                if bank_generation.is_some() {
                    return Err(duplicate(option));
                }
                bank_generation = Some(parse_nonzero_generation(value, "bank generation")?);
            }
            "--control-socket" => {
                if control_socket.is_some() {
                    return Err(duplicate(option));
                }
                let path = PathBuf::from(value);
                if !path.is_absolute() {
                    return Err("worker control socket must be an absolute path".to_owned());
                }
                control_socket = Some(path);
            }
            BUNDLE_OPTION => {
                if bundle.replace(Vst3BundlePath::new(value.clone())).is_some() || value.is_empty()
                {
                    return Err(duplicate(option));
                }
            }
            "--class-id" => {
                if class_id.replace(value.clone()).is_some() || value.is_empty() {
                    return Err(duplicate(option));
                }
            }
            _ => return Err(format!("unknown production worker option `{option}`")),
        }
    }
    let bundle = bundle.ok_or("production worker requires --bundle")?;
    Ok(ProductionWorkerConfiguration {
        bank_name: bank_name.ok_or("production worker requires --bank")?,
        worker_id: worker_id.ok_or("production worker requires --worker-id")?,
        rack_index: rack_index.ok_or("production worker requires --rack-index")?,
        rack_generation: rack_generation.ok_or("production worker requires --rack-generation")?,
        bank_index: bank_index.ok_or("production worker requires --bank-index")?,
        bank_generation: bank_generation.ok_or("production worker requires --bank-generation")?,
        control_socket: control_socket.ok_or("production worker requires --control-socket")?,
        selection: Vst3ClassSelection::new(
            bundle,
            class_id.ok_or("production worker requires --class-id")?,
        ),
    })
}

fn parse_nonzero_generation(value: &str, label: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .ok()
        .filter(|generation| *generation != 0)
        .ok_or_else(|| format!("{label} must be a nonzero unsigned integer"))
}

fn parse_worker_id(value: &str) -> Result<u32, String> {
    let worker_id = value
        .parse::<u32>()
        .map_err(|_| "worker ID must be an unsigned integer")?;
    if worker_id == 0 || worker_id >= u32::MAX - 2 {
        return Err("worker ID must be nonzero and outside the protocol-reserved range".to_owned());
    }
    Ok(worker_id)
}

fn run_standalone_worker(configuration: &StandaloneWorkerConfiguration) -> Result<(), String> {
    let clock = MonotonicClock::new().map_err(|error| error.to_string())?;
    let mut heartbeat = HeartbeatPublisher::new(clock);
    let mut region =
        SharedMemoryRegion::open(&configuration.bank_name).map_err(|error| error.to_string())?;
    let mut plugin = load_optional_plugin(configuration)?;
    heartbeat.publish_now(&region.bank().header)?;
    publish_readiness(io::stdout().lock())?;
    let shutdown_requested = spawn_host_shutdown_watchdog();

    while !shutdown_requested.load(Ordering::Acquire) {
        heartbeat.publish_if_due(&region.bank().header)?;
        let mut processed_request = false;
        for slot_index in 0..BLOCK_SLOT_COUNT {
            let bank = region.bank_mut();
            let header = &bank.header;
            let Some(slot) = bank.slots.get_mut(slot_index) else {
                continue;
            };
            if process_available_request(
                slot,
                header,
                configuration,
                clock,
                &mut heartbeat,
                plugin.as_mut(),
            )? {
                processed_request = true;
            }
        }
        if !processed_request {
            // Standalone mode has no control-socket wakeup, so poll with a CPU relaxation hint
            // rather than yielding to the general scheduler between requests.
            std::hint::spin_loop();
        }
    }
    Ok(())
}

#[cfg(feature = "sdk")]
fn load_optional_plugin(
    configuration: &StandaloneWorkerConfiguration,
) -> Result<Option<SdkPlugin>, String> {
    let Some(bundle) = configuration.bundle.as_ref() else {
        return Ok(None);
    };
    SdkPlugin::load(bundle).map(Some).map_err(|error| {
        format!(
            "could not load VST3 bundle {}: {error}",
            bundle.as_path().display()
        )
    })
}

#[cfg(not(feature = "sdk"))]
fn load_optional_plugin(
    configuration: &StandaloneWorkerConfiguration,
) -> Result<Option<()>, String> {
    if configuration.bundle.is_some() {
        return Err("worker built without the sdk feature cannot honor --bundle".to_owned());
    }
    Ok(None)
}

fn publish_readiness(mut output: impl io::Write) -> Result<(), String> {
    writeln!(output, "{WORKER_READY_LINE}")
        .map_err(|error| format!("could not write readiness: {error}"))?;
    output
        .flush()
        .map_err(|error| format!("could not flush readiness: {error}"))
}

fn spawn_host_shutdown_watchdog() -> Arc<AtomicBool> {
    let shutdown_requested = Arc::new(AtomicBool::new(false));
    let watchdog_flag = Arc::clone(&shutdown_requested);
    thread::spawn(move || {
        let _ = wait_for_host_shutdown(io::stdin().lock());
        watchdog_flag.store(true, Ordering::Release);
    });
    shutdown_requested
}

fn wait_for_host_shutdown(mut input: impl io::Read) -> io::Result<()> {
    let mut command = [0_u8; 1];
    let _ = input.read(&mut command)?;
    Ok(())
}

fn process_available_request(
    slot: &mut BlockSlot,
    header: &ProtocolHeader,
    configuration: &StandaloneWorkerConfiguration,
    clock: MonotonicClock,
    heartbeat: &mut HeartbeatPublisher,
    #[cfg(feature = "sdk")] plugin: Option<&mut SdkPlugin>,
    #[cfg(not(feature = "sdk"))] plugin: Option<&mut ()>,
) -> Result<bool, String> {
    let Some(observed_ticket) = requested_ticket(slot)? else {
        return Ok(false);
    };
    let ticket = match slot.claim_ticket_for_processing_at(
        configuration.worker_id,
        observed_ticket,
        clock.now_ticks(),
    ) {
        Ok(ticket) => ticket,
        Err(ProtocolError::UnexpectedState | ProtocolError::Owned | ProtocolError::StaleTicket) => {
            return Ok(false);
        }
        Err(error) => return Err(format!("invalid shared-memory request: {error}")),
    };

    process_slot_audio(slot, plugin)?;
    slot.publish_completion_at(configuration.worker_id, ticket, clock.now_ticks())
        .map_err(|error| format!("could not publish completion: {error}"))?;
    // A completed block is a useful liveness boundary even when its period is shorter than the
    // ordinary heartbeat interval.
    heartbeat.publish_now(header)?;
    Ok(true)
}

#[cfg(feature = "sdk")]
fn process_slot_audio(slot: &mut BlockSlot, plugin: Option<&mut SdkPlugin>) -> Result<(), String> {
    if let Some(plugin) = plugin {
        process_with_plugin(slot, plugin)
            .map_err(|error| format!("could not process VST3 audio: {error}"))
    } else {
        copy_bounded_audio(slot)
            .map_err(|error| format!("could not copy bounded no-op audio: {error}"))
    }
}

#[cfg(not(feature = "sdk"))]
fn process_slot_audio(slot: &mut BlockSlot, _plugin: Option<&mut ()>) -> Result<(), String> {
    copy_bounded_audio(slot).map_err(|error| format!("could not copy bounded no-op audio: {error}"))
}

#[cfg(feature = "sdk")]
fn process_with_plugin(slot: &mut BlockSlot, plugin: &mut SdkPlugin) -> Result<(), String> {
    let request = BlockRequest {
        frame_count: slot.metadata.frame_count,
        input_channel_count: slot.metadata.input_channel_count,
        output_channel_count: slot.metadata.output_channel_count,
        midi_event_count: slot.metadata.midi_event_count,
        event_count: slot.metadata.event_count,
        flags: slot.metadata.flags,
        sidechain_slots: slot.metadata.sidechain_slots,
    };
    if !request.is_valid() {
        return Err(ProtocolError::InvalidRequest.to_string());
    }
    let frame_count = usize::try_from(request.frame_count)
        .map_err(|_| ProtocolError::InvalidRequest.to_string())?;
    let input_channel_count = usize::try_from(request.input_channel_count)
        .map_err(|_| ProtocolError::InvalidRequest.to_string())?;
    let output_channel_count = usize::try_from(request.output_channel_count)
        .map_err(|_| ProtocolError::InvalidRequest.to_string())?;

    // Disjoint field borrows keep this path allocation-free for mono/stereo.
    let input_audio = &slot.input_audio;
    let output_audio = &mut slot.output_audio;
    match (input_channel_count, output_channel_count) {
        (1, 1) => {
            let input: [&[f32]; 1] = [&input_audio[0][..frame_count]];
            let mut output: [&mut [f32]; 1] = [&mut output_audio[0][..frame_count]];
            plugin
                .process_planar(&input, &mut output, frame_count)
                .map_err(|error| error.to_string())?;
        }
        (1, 2) => {
            let input: [&[f32]; 1] = [&input_audio[0][..frame_count]];
            let (left, right) = output_audio.split_at_mut(1);
            let mut output: [&mut [f32]; 2] =
                [&mut left[0][..frame_count], &mut right[0][..frame_count]];
            plugin
                .process_planar(&input, &mut output, frame_count)
                .map_err(|error| error.to_string())?;
        }
        (2, 1) => {
            let input: [&[f32]; 2] = [
                &input_audio[0][..frame_count],
                &input_audio[1][..frame_count],
            ];
            let mut output: [&mut [f32]; 1] = [&mut output_audio[0][..frame_count]];
            plugin
                .process_planar(&input, &mut output, frame_count)
                .map_err(|error| error.to_string())?;
        }
        (2, 2) => {
            let input: [&[f32]; 2] = [
                &input_audio[0][..frame_count],
                &input_audio[1][..frame_count],
            ];
            let (left, right) = output_audio.split_at_mut(1);
            let mut output: [&mut [f32]; 2] =
                [&mut left[0][..frame_count], &mut right[0][..frame_count]];
            plugin
                .process_planar(&input, &mut output, frame_count)
                .map_err(|error| error.to_string())?;
        }
        _ => return Err(ProtocolError::InvalidRequest.to_string()),
    }
    Ok(())
}

fn requested_ticket(slot: &BlockSlot) -> Result<Option<BlockTicket>, String> {
    match slot.metadata.state() {
        Ok(SlotState::Requested) => {
            let ticket = BlockTicket {
                generation: slot.metadata.request_generation.load(Ordering::Acquire),
                sequence: slot.metadata.request_sequence.load(Ordering::Acquire),
            };
            if ticket.is_valid() {
                Ok(Some(ticket))
            } else {
                Err("requested slot contains an invalid ticket".to_owned())
            }
        }
        Ok(
            SlotState::Free | SlotState::Processing | SlotState::Complete | SlotState::Abandoned,
        ) => Ok(None),
        Err(error) => Err(format!("invalid shared-memory request: {error}")),
    }
}

fn copy_bounded_audio(slot: &mut BlockSlot) -> Result<(), ProtocolError> {
    let request = BlockRequest {
        frame_count: slot.metadata.frame_count,
        input_channel_count: slot.metadata.input_channel_count,
        output_channel_count: slot.metadata.output_channel_count,
        midi_event_count: slot.metadata.midi_event_count,
        event_count: slot.metadata.event_count,
        flags: slot.metadata.flags,
        sidechain_slots: slot.metadata.sidechain_slots,
    };
    if !request.is_valid() {
        return Err(ProtocolError::InvalidRequest);
    }

    let frame_count =
        usize::try_from(request.frame_count).map_err(|_| ProtocolError::InvalidRequest)?;
    let input_channel_count =
        usize::try_from(request.input_channel_count).map_err(|_| ProtocolError::InvalidRequest)?;
    let output_channel_count =
        usize::try_from(request.output_channel_count).map_err(|_| ProtocolError::InvalidRequest)?;
    let copied_channel_count = cmp::min(input_channel_count, output_channel_count);
    for channel in 0..copied_channel_count {
        let input = &slot.input_audio[channel][..frame_count];
        slot.output_audio[channel][..frame_count].copy_from_slice(input);
    }
    for channel in copied_channel_count..output_channel_count {
        slot.output_audio[channel][..frame_count].fill(0.0);
    }
    Ok(())
}

const fn heartbeat_due(last_tick: u64, now_tick: u64, interval_ticks: u64) -> bool {
    last_tick == 0 || now_tick.saturating_sub(last_tick) >= interval_ticks
}

const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(feature = "sdk")]
const EDITOR_POLL_INTERVAL: Duration = Duration::from_millis(8);
#[cfg(feature = "sdk")]
const EDITOR_RUN_LOOP_INTERVAL: Duration = Duration::from_millis(2);
/// `AppKit` removes a closed window only on a later run-loop pass, so events keep pumping this
/// long after an editor closes, even when no other editor is open.
#[cfg(all(feature = "sdk", target_os = "macos"))]
const EDITOR_CLOSE_PUMP: Duration = Duration::from_millis(500);
/// An editor is pictured this long after it opens or a parameter changes, once it has painted.
#[cfg(all(feature = "sdk", target_os = "macos"))]
const EDITOR_PREVIEW_SETTLE: Duration = Duration::from_millis(500);
/// While parameters keep changing, an open editor is pictured at most this often.
#[cfg(all(feature = "sdk", target_os = "macos"))]
const EDITOR_PREVIEW_INTERVAL: Duration = Duration::from_secs(2);
/// A loading thread without a loop pass for this long is held by plug-in code, such as a modal
/// dialog or menu. Picture polls are refused then instead of waiting past the host's deadline.
#[cfg(feature = "sdk")]
const EDITOR_THREAD_STALL: Duration = Duration::from_millis(250);

#[cfg(feature = "sdk")]
struct MainThreadRequest {
    runtime: Box<ProductionRuntime>,
    request: ControlRequest,
}

#[cfg(feature = "sdk")]
struct MainThreadResponse {
    runtime: Box<ProductionRuntime>,
    response: ControlResponse,
}

/// A chain change the loading thread prepares and the processing thread applies between
/// blocks. Plug-in loading, activation, editor windows, and teardown stay off the processing
/// thread, so the rest of the chain keeps its deadline.
#[cfg(feature = "sdk")]
enum ChainEdit {
    /// An activated plug-in for an empty slot. It warms up before it is heard.
    Insert(Box<InsertedPlugin>),
    Remove(usize),
    /// Slot `i` receives the plug-in previously at `from[i]`.
    Reorder([usize; MAX_PLUGINS_PER_RACK]),
}

#[cfg(feature = "sdk")]
struct InsertedPlugin {
    configuration: WirePluginSlotConfiguration,
    facade: Vst3Facade,
}

#[cfg(feature = "sdk")]
enum LoadingReply {
    /// The loading thread answered the request itself.
    Response(ControlResponse),
    /// The processing thread applies `edit`, then answers `request`.
    Apply {
        request: ControlRequest,
        edit: ChainEdit,
    },
}

/// Live captures use the upper half of the transfer ID space, so chunk and release requests
/// can be routed to the loading thread without consulting the processing thread's state.
#[cfg(feature = "sdk")]
const LIVE_CAPTURE_TRANSFER_BASE: u64 = 1 << 63;

#[cfg(feature = "sdk")]
struct EditorHost {
    // Loading-thread captures run beside the DSP instead of taking the runtime away from it.
    state_transfers: StateTransfers,
    services: BTreeMap<usize, sp_vst3::sdk::MainThreadService>,
    feedback_loss_counts: BTreeMap<usize, u64>,
    container_loss_counts: BTreeMap<usize, u64>,
    editors: BTreeMap<usize, sp_vst3::sdk::SdkEditorHandle>,
    #[cfg(target_os = "macos")]
    windows: BTreeMap<usize, OpenEditor>,
    /// Newest editor picture per slot. It outlives the window, so the host can still fetch the
    /// picture taken as the editor closed.
    #[cfg(target_os = "macos")]
    previews: BTreeMap<usize, EditorPreview>,
    #[cfg(target_os = "macos")]
    last_preview_sequence: u64,
    /// Keeps the event pump running until a closed window has left the screen.
    #[cfg(target_os = "macos")]
    pump_until: Option<Instant>,
}

/// An open editor window and when it is next pictured.
#[cfg(all(feature = "sdk", target_os = "macos"))]
struct OpenEditor {
    window: sp_vst3::editor_window::MacOsEditorWindow,
    /// When to picture the editor next; `None` until a parameter changes.
    preview_due: Option<Instant>,
    pictured_at: Option<Instant>,
}

#[cfg(all(feature = "sdk", target_os = "macos"))]
impl OpenEditor {
    /// Schedules a picture once the editor has repainted, and no sooner than
    /// [`EDITOR_PREVIEW_INTERVAL`] after the last one.
    fn parameter_changed(&mut self, now: Instant) {
        if self.preview_due.is_none() {
            let earliest = self
                .pictured_at
                .map_or(now, |at| at + EDITOR_PREVIEW_INTERVAL);
            self.preview_due = Some(earliest.max(now + EDITOR_PREVIEW_SETTLE));
        }
    }
}

/// A PNG picture of a slot's editor, numbered in capture order across the worker.
#[cfg(all(feature = "sdk", target_os = "macos"))]
struct EditorPreview {
    sequence: u64,
    captured_at_unix_ms: u64,
    png: Vec<u8>,
}

#[cfg(feature = "sdk")]
impl Default for EditorHost {
    fn default() -> Self {
        Self {
            state_transfers: StateTransfers::starting_at(LIVE_CAPTURE_TRANSFER_BASE),
            services: BTreeMap::new(),
            feedback_loss_counts: BTreeMap::new(),
            container_loss_counts: BTreeMap::new(),
            editors: BTreeMap::new(),
            #[cfg(target_os = "macos")]
            windows: BTreeMap::new(),
            #[cfg(target_os = "macos")]
            previews: BTreeMap::new(),
            #[cfg(target_os = "macos")]
            last_preview_sequence: 0,
            #[cfg(target_os = "macos")]
            pump_until: None,
        }
    }
}

#[cfg(feature = "sdk")]
impl EditorHost {
    fn register_services(&mut self, rack: &RackProcessor<Vst3Facade>) {
        self.services = rack
            .plugins()
            .filter_map(|(slot, plugin)| {
                plugin
                    .adapter
                    .main_thread_service()
                    .map(|service| (slot, service))
            })
            .collect();
        self.editors = rack
            .plugins()
            .filter_map(|(slot, plugin)| {
                plugin
                    .adapter
                    .main_thread_editor_handle()
                    .map(|editor| (slot, editor))
            })
            .collect();
    }

    fn handle_editor_request(&mut self, request: &ControlRequest) -> ControlResponse {
        if is_live_read_operation(request.operation()) {
            let result = self.handle_live_read(request);
            return ProductionRuntime::response(request, result);
        }
        #[cfg(not(target_os = "macos"))]
        {
            return ProductionRuntime::unsupported(request, "native editors require macOS");
        }

        #[cfg(target_os = "macos")]
        {
            let result = match request.operation() {
                ControlOperation::OpenNativeEditor => ProductionRuntime::slot_index(request)
                    .and_then(|slot| {
                        if let Some(open) = self.windows.get(&slot) {
                            open.window.focus();
                            return Ok(Vec::new());
                        }
                        let position = (!request.payload().is_empty())
                            .then(|| EditorPosition::decode(request.payload()))
                            .transpose()
                            .map_err(|error| error.to_string())?;
                        let editor = self
                            .editors
                            .get_mut(&slot)
                            .ok_or("native editor slot is not loaded")?;
                        let window = sp_vst3::editor_window::MacOsEditorWindow::new(
                            "Superposition Plug-in Editor",
                        )?;
                        let parent_view = window.content_view()?;
                        let size = editor
                            .open_editor(&parent_view)
                            .map_err(|error| error.to_string())?;
                        window.resize(size.width, size.height);
                        if let Some(position) = position {
                            window.set_top_left(f64::from(position.left), f64::from(position.top));
                        }
                        window.focus();
                        self.windows.insert(
                            slot,
                            OpenEditor {
                                window,
                                preview_due: Some(Instant::now() + EDITOR_PREVIEW_SETTLE),
                                pictured_at: None,
                            },
                        );
                        Ok(Vec::new())
                    }),
                ControlOperation::CloseNativeEditor => ProductionRuntime::slot_index(request)
                    .and_then(|slot| {
                        let window = &self
                            .windows
                            .get(&slot)
                            .ok_or("native editor window is not open")?
                            .window;
                        window.request_close();
                        if !window.take_close_request() {
                            return Err("native editor window did not accept close".to_owned());
                        }
                        self.close_editor(slot)?;
                        Ok(Vec::new())
                    }),
                ControlOperation::FocusNativeEditor => ProductionRuntime::slot_index(request)
                    .and_then(|slot| {
                        self.windows
                            .get(&slot)
                            .ok_or("native editor window is not open")?
                            .window
                            .focus();
                        Ok(Vec::new())
                    }),
                ControlOperation::ResizeNativeEditor => ProductionRuntime::slot_index(request)
                    .and_then(|slot| {
                        let geometry = EditorGeometry::decode(request.payload())
                            .map_err(|error| error.to_string())?;
                        let editor = self
                            .editors
                            .get_mut(&slot)
                            .ok_or("native editor slot is not loaded")?;
                        let window = &self
                            .windows
                            .get(&slot)
                            .ok_or("native editor window is not open")?
                            .window;
                        let accepted = editor
                            .resize_editor(sp_vst3::sdk::Vst3EditorSize {
                                width: geometry.width,
                                height: geometry.height,
                            })
                            .map_err(|error| error.to_string())?;
                        window.resize(accepted.width, accepted.height);
                        Ok(Vec::new())
                    }),
                ControlOperation::CaptureEditorPreview => ProductionRuntime::slot_index(request)
                    .and_then(|slot| self.describe_preview(slot, request.payload())),
                _ => Err("request is not a native-editor operation".to_owned()),
            };
            ProductionRuntime::response(request, result)
        }
    }

    /// Loads, closes, or rekeys loading-thread resources for a chain edit. The processing thread
    /// has already checked the edit against the rack.
    fn prepare_chain_edit(
        &mut self,
        request: &ControlRequest,
        feedback: &ParameterFeedbackBank,
    ) -> Result<ChainEdit, String> {
        self.state_transfers.clear();
        match request.operation() {
            ControlOperation::LoadPlugin => {
                let configuration = WirePluginSlotConfiguration::decode(request.payload())
                    .map_err(|error| error.to_string())?;
                let slot = ProductionRuntime::request_configuration_slot(request, &configuration)?;
                let mut facade = ProductionRuntime::load_facade(&configuration)?;
                facade.set_active(true).map_err(|error| error.to_string())?;
                self.forget_slot(slot);
                if let Some(service) = facade.adapter.main_thread_service() {
                    self.services.insert(slot, service);
                }
                if let Some(editor) = facade.adapter.main_thread_editor_handle() {
                    self.editors.insert(slot, editor);
                }
                let _ = feedback.reset_slot(slot);
                Ok(ChainEdit::Insert(Box::new(InsertedPlugin {
                    configuration,
                    facade,
                })))
            }
            ControlOperation::UnloadSlot => {
                let slot = ProductionRuntime::slot_index(request)?;
                #[cfg(target_os = "macos")]
                if self.windows.contains_key(&slot) {
                    self.close_editor(slot)?;
                }
                self.forget_slot(slot);
                let _ = feedback.reset_slot(slot);
                Ok(ChainEdit::Remove(slot))
            }
            ControlOperation::ReorderRack => {
                let order =
                    SlotOrder::decode(request.payload()).map_err(|error| error.to_string())?;
                let from = slot_permutation(&order);
                permute_slot_keys(&mut self.services, &from);
                permute_slot_keys(&mut self.editors, &from);
                permute_slot_keys(&mut self.feedback_loss_counts, &from);
                permute_slot_keys(&mut self.container_loss_counts, &from);
                #[cfg(target_os = "macos")]
                {
                    permute_slot_keys(&mut self.windows, &from);
                    permute_slot_keys(&mut self.previews, &from);
                }
                for (slot, &source) in from.iter().enumerate() {
                    if slot != source {
                        let _ = feedback.reset_slot(slot);
                    }
                }
                Ok(ChainEdit::Reorder(from))
            }
            _ => Err("request is not a chain edit".to_owned()),
        }
    }

    fn forget_slot(&mut self, slot: usize) {
        self.services.remove(&slot);
        self.editors.remove(&slot);
        self.feedback_loss_counts.remove(&slot);
        self.container_loss_counts.remove(&slot);
        #[cfg(target_os = "macos")]
        self.previews.remove(&slot);
    }

    /// Serves state and parameter reads on the loading thread while the rack keeps processing.
    fn handle_live_read(&mut self, request: &ControlRequest) -> Result<Vec<u8>, String> {
        let slot = ProductionRuntime::slot_index(request)?;
        let editor = self
            .editors
            .get(&slot)
            .ok_or("plug-in slot is not loaded")?;
        match request.operation() {
            ControlOperation::BeginStateCapture => {
                // Apply processor-originated values to the controller before it is serialized.
                if let Some(service) = self.services.get(&slot) {
                    let _ = service.pump_foreground();
                }
                let state = editor.capture_state().map_err(|error| error.to_string())?;
                let descriptor = self.state_transfers.begin_capture(
                    slot,
                    PluginState {
                        component: state.component,
                        controller: state.controller,
                    },
                )?;
                StateTransferDescriptor {
                    id: descriptor.id,
                    component_len: descriptor.component_len,
                    controller_len: descriptor.controller_len,
                }
                .encode()
                .map_err(|error| error.to_string())
            }
            ControlOperation::ReadStateChunk => {
                let chunk = StateChunkRequest::decode(request.payload())
                    .map_err(|error| error.to_string())?;
                self.state_transfers.read_chunk(
                    slot,
                    chunk.id,
                    chunk.stream,
                    chunk.offset,
                    chunk.length,
                )
            }
            ControlOperation::ReleaseStateTransfer => {
                let transfer = StateTransferId::decode(request.payload())
                    .map_err(|error| error.to_string())?;
                self.state_transfers.release(slot, transfer.id)?;
                Ok(Vec::new())
            }
            ControlOperation::ReadParameters => {
                let ids =
                    ParameterIds::decode(request.payload()).map_err(|error| error.to_string())?;
                let parameters = ids
                    .parameters
                    .into_iter()
                    .map(|id| {
                        let parameter_id = u32::try_from(id.value)
                            .map_err(|_| "parameter ID exceeds VST3 u32 range".to_owned())?;
                        let normalized = editor
                            .read_parameter(parameter_id)
                            .map_err(|error| error.to_string())?;
                        Ok(ParameterWrite { id, normalized })
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                ParameterValues { parameters }
                    .encode()
                    .map_err(|error| error.to_string())
            }
            _ => Err("request is not a live read operation".to_owned()),
        }
    }

    /// Describes `slot`'s newest picture and stages its PNG as a live-capture transfer when the
    /// host's known sequence differs. It answers from stored pictures and never pictures the
    /// editor itself.
    #[cfg(target_os = "macos")]
    fn describe_preview(&mut self, slot: usize, payload: &[u8]) -> Result<Vec<u8>, String> {
        let known = EditorPreviewRequest::decode(payload)
            .map_err(|error| error.to_string())?
            .known_sequence;
        let editor_open = self.windows.contains_key(&slot);
        let descriptor = match self.previews.get(&slot) {
            None => EditorPreviewDescriptor {
                sequence: 0,
                captured_at_unix_ms: 0,
                png_len: 0,
                editor_open,
                transfer_id: None,
            },
            Some(preview) => EditorPreviewDescriptor {
                sequence: preview.sequence,
                captured_at_unix_ms: preview.captured_at_unix_ms,
                png_len: u32::try_from(preview.png.len())
                    .map_err(|_| "editor preview is too large")?,
                editor_open,
                transfer_id: (preview.sequence != known)
                    .then(|| {
                        self.state_transfers.begin_capture(
                            slot,
                            PluginState {
                                component: preview.png.clone(),
                                controller: Vec::new(),
                            },
                        )
                    })
                    .transpose()?
                    .map(|transfer| transfer.id),
            },
        };
        descriptor.encode().map_err(|error| error.to_string())
    }

    /// Pictures `slot`'s open editor and keeps the picture as the slot's newest preview.
    #[cfg(target_os = "macos")]
    fn capture_preview(&mut self, slot: usize) {
        let Some(open) = self.windows.get_mut(&slot) else {
            return;
        };
        open.preview_due = None;
        open.pictured_at = Some(Instant::now());
        match open.window.capture_preview_png() {
            Ok(png) => {
                self.last_preview_sequence += 1;
                let captured_at_unix_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |elapsed| {
                        u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
                    });
                self.previews.insert(
                    slot,
                    EditorPreview {
                        sequence: self.last_preview_sequence,
                        captured_at_unix_ms,
                        png,
                    },
                );
            }
            Err(error) => eprintln!("sp-plugin-worker: could not picture editor: {error}"),
        }
    }

    /// Pictures the editor one last time, then detaches its view and closes its window.
    #[cfg(target_os = "macos")]
    fn close_editor(&mut self, slot: usize) -> Result<(), String> {
        self.log_service_stats("editor-close");
        self.capture_preview(slot);
        let window = &self
            .windows
            .get(&slot)
            .ok_or("native editor window is not open")?
            .window;
        let detach_result = self
            .editors
            .get_mut(&slot)
            .ok_or_else(|| "native editor slot is not loaded".to_owned())
            .and_then(|editor| editor.close_editor().map_err(|error| error.to_string()));
        window.close();
        self.windows.remove(&slot);
        self.pump_until = Some(Instant::now() + EDITOR_CLOSE_PUMP);
        detach_result
    }

    /// Whether `AppKit` events need pumping: an editor is open or one closed recently.
    #[cfg(target_os = "macos")]
    fn needs_event_pump(&self) -> bool {
        !self.windows.is_empty() || self.pump_until.is_some_and(|until| Instant::now() < until)
    }

    fn log_service_stats(&self, reason: &str) {
        use std::io::Write as _;

        let Some(path) = std::env::var_os("SUPERPOSITION_EDITOR_DIAGNOSTICS") else {
            return;
        };
        let Ok(mut output) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        else {
            return;
        };
        for (slot, service) in &self.services {
            let _ = writeln!(
                output,
                "visualization: slot={} reason={reason} stats={:?}",
                slot + 1,
                service.stats()
            );
        }
    }
}

#[allow(
    clippy::too_many_lines,
    clippy::needless_pass_by_value,
    reason = "one-shot startup sequence that owns its configuration for thread handoff"
)]
fn run_production_worker(configuration: ProductionWorkerConfiguration) -> Result<(), String> {
    #[cfg(feature = "sdk")]
    {
        if !sp_shared_memory_macos::request_wake_supported() {
            return Err("live plug-in workers require macOS 14.4 or later".to_owned());
        }
        let listener = bind_control_listener(&configuration.control_socket)?;
        let target = ControlTarget::new(
            RackIdentity::new(configuration.rack_index, configuration.rack_generation)
                .map_err(|error| error.to_string())?,
            BankIdentity::new(configuration.bank_index, configuration.bank_generation)
                .map_err(|error| error.to_string())?,
        );
        let region = SharedMemoryRegion::open(&configuration.bank_name)
            .map_err(|error| error.to_string())?;
        if region.generation() != configuration.bank_generation {
            return Err(format!(
                "worker bank generation {} does not match mapped bank generation {}",
                configuration.bank_generation,
                region.generation()
            ));
        }
        let clock = MonotonicClock::new().map_err(|error| error.to_string())?;
        let mut heartbeat = HeartbeatPublisher::new(clock);
        // The initial plug-in has no sidechain. A rebuild that asks for one reloads it.
        let facade = Vst3Facade::load(&configuration.selection, false)?;
        let initial_plugin = WirePluginSlotConfiguration {
            slot: 0,
            input_channels: facade.layout.input_channels,
            output_channels: facade.layout.output_channels,
            event_input_active: facade.layout.event_input_active,
            sidechain_active: false,
            bundle_path: configuration
                .selection
                .bundle
                .as_path()
                .display()
                .to_string(),
            class_id: Some(configuration.selection.class_id.clone()),
        };
        let mut runtime = Box::new(ProductionRuntime::new(
            configuration.worker_id,
            facade,
            initial_plugin,
        )?);
        heartbeat.publish_now(&region.bank().header)?;
        let (mut socket_endpoint, processing_endpoint) = control_mailbox();
        socket_endpoint
            .set_request_wake(&configuration.bank_name)
            .map_err(|error| error.to_string())?;
        let control_thread =
            ControlThread::spawn(listener, target, socket_endpoint, CONTROL_TIMEOUT)
                .map_err(|error| error.to_string())?;
        // Product workers use the authenticated control shutdown request. Unlike the standalone
        // worker, EOF on inherited stdin must not terminate a rack immediately when the
        // supervisor deliberately launches it with null stdin.
        let shutdown_requested = Arc::new(AtomicBool::new(false));
        runtime.shutdown_requested = Arc::clone(&shutdown_requested);
        let loading_pass = Arc::clone(&runtime.loading_pass);
        let processing_shutdown = Arc::clone(&shutdown_requested);
        let (main_request_tx, main_request_rx) = sync_channel(1);
        let (main_response_tx, main_response_rx) = sync_channel(1);
        let (editor_request_tx, editor_request_rx) = sync_channel(1);
        let (editor_response_tx, editor_response_rx) = sync_channel(1);
        // Plug-ins removed from the chain return here for teardown on the loading thread.
        let (retired_tx, retired_rx) = sync_channel::<Vst3Facade>(MAX_PLUGINS_PER_RACK);
        let mut editor_host = EditorHost::default();
        editor_host.register_services(&runtime.rack);
        // This second mapping belongs to the loading/main thread. The processing thread keeps
        // its own mapping, and neither audio blocks nor control RPCs carry editor feedback.
        let feedback_region = SharedMemoryRegion::open(&configuration.bank_name)
            .map_err(|error| format!("could not open parameter feedback mapping: {error}"))?;
        feedback_region
            .bank()
            .feedback
            .reset_slot(0)
            .ok_or("could not initialize first parameter feedback slot")?;
        publish_worker_telemetry(region.bank(), &mut runtime);
        let processing = thread::Builder::new()
            .name("sp-worker-processing".to_owned())
            .spawn(move || {
                let mut runtime = Some(runtime);
                let result = run_production_processing_thread(
                    region,
                    clock,
                    heartbeat,
                    &mut runtime,
                    processing_endpoint,
                    main_request_tx,
                    main_response_rx,
                    editor_request_tx,
                    editor_response_rx,
                    &retired_tx,
                    &processing_shutdown,
                );
                processing_shutdown.store(true, Ordering::Release);
                (result, runtime)
            })
            .map_err(|error| format!("could not start worker processing thread: {error}"))?;

        while !shutdown_requested.load(Ordering::Acquire) && !control_thread.is_stopped() {
            loading_pass.store(clock.now_ticks(), Ordering::Release);
            if let Ok(request) = editor_request_rx.try_recv() {
                let reply = if is_chain_edit(request.operation()) {
                    match editor_host.prepare_chain_edit(&request, &feedback_region.bank().feedback)
                    {
                        Ok(edit) => LoadingReply::Apply { request, edit },
                        Err(error) => LoadingReply::Response(ProductionRuntime::response(
                            &request,
                            Err(error),
                        )),
                    }
                } else {
                    LoadingReply::Response(editor_host.handle_editor_request(&request))
                };
                if editor_response_tx.send(reply).is_err() {
                    break;
                }
            }
            let mut removed = false;
            while let Ok(mut facade) = retired_rx.try_recv() {
                if let Err(error) = facade.set_active(false) {
                    eprintln!("sp-plugin-worker: could not deactivate removed plug-in: {error}");
                }
                removed = true;
            }
            // Teardown can outlast the close pump that removes the plug-in's editor window.
            #[cfg(target_os = "macos")]
            if removed {
                editor_host.pump_until = Some(Instant::now() + EDITOR_CLOSE_PUMP);
            }
            #[cfg(target_os = "macos")]
            let pumping = editor_host.needs_event_pump();
            #[cfg(not(target_os = "macos"))]
            let pumping = false;
            let next_request = if pumping {
                match main_request_rx.try_recv() {
                    Ok(task) => Some(task),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => break,
                }
            } else {
                match main_request_rx.recv_timeout(EDITOR_POLL_INTERVAL) {
                    Ok(task) => Some(task),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            };
            if let Some(mut task) = next_request {
                let operation = task.request.operation();
                let response = task
                    .runtime
                    .handle_main_thread_control(task.request, &mut editor_host);
                if response.status() == ControlResponseStatus::Ok
                    && operation == ControlOperation::RebuildRack
                {
                    for slot in 0..MAX_PLUGINS_PER_RACK {
                        let _ = feedback_region.bank().feedback.reset_slot(slot);
                    }
                    editor_host.feedback_loss_counts.clear();
                    editor_host.container_loss_counts.clear();
                    #[cfg(target_os = "macos")]
                    editor_host.previews.clear();
                }
                editor_host.register_services(&task.runtime.rack);
                if main_response_tx
                    .send(MainThreadResponse {
                        runtime: task.runtime,
                        response,
                    })
                    .is_err()
                {
                    break;
                }
            }
            for (&slot, service) in &editor_host.services {
                let _ = service.pump_foreground();
                let values = service.take_parameter_values(128);
                #[cfg(target_os = "macos")]
                if !values.is_empty()
                    && let Some(open) = editor_host.windows.get_mut(&slot)
                {
                    open.parameter_changed(Instant::now());
                }
                for (parameter_id, normalized) in values {
                    let _ = feedback_region
                        .bank()
                        .feedback
                        .publish(slot, parameter_id, normalized);
                }
                let lost = service.parameter_feedback_loss_count();
                let previous = editor_host.feedback_loss_counts.entry(slot).or_default();
                let feedback_delta = lost.saturating_sub(*previous);
                *previous = lost;
                let container_lost = service.parameter_container_loss_count();
                let previous = editor_host.container_loss_counts.entry(slot).or_default();
                let container_delta = container_lost.saturating_sub(*previous);
                *previous = container_lost;
                feedback_region
                    .bank()
                    .feedback
                    .report_overflow(slot, feedback_delta.saturating_add(container_delta));
            }
            #[cfg(target_os = "macos")]
            {
                let close_requests: Vec<usize> = editor_host
                    .windows
                    .iter()
                    .filter_map(|(&slot, open)| open.window.take_close_request().then_some(slot))
                    .collect();
                for slot in close_requests {
                    if let Err(error) = editor_host.close_editor(slot) {
                        eprintln!("sp-plugin-worker: could not close editor: {error}");
                    }
                }
                for (&slot, open) in &editor_host.windows {
                    let Some(service) = editor_host.services.get(&slot) else {
                        continue;
                    };
                    if let Some((width, height)) = service.take_editor_resize_request()
                        && let Ok(size) = sp_vst3::sdk::Vst3EditorSize::from_sdk(width, height)
                    {
                        open.window.resize(size.width, size.height);
                    }
                }
                // One due picture per pass keeps each pump interval short.
                let now = Instant::now();
                if let Some(slot) = editor_host.windows.iter().find_map(|(&slot, open)| {
                    open.preview_due
                        .is_some_and(|due| due <= now)
                        .then_some(slot)
                }) {
                    editor_host.capture_preview(slot);
                }
                if editor_host.needs_event_pump() {
                    sp_vst3::editor_window::pump_events(EDITOR_RUN_LOOP_INTERVAL);
                }
            }
        }
        editor_host.log_service_stats("shutdown");
        shutdown_requested.store(true, Ordering::Release);
        // Unblock a pending ownership handoff before joining the processing thread.
        drop(main_request_rx);
        drop(main_response_tx);
        let control_result = control_thread.shutdown().map_err(|error| error.to_string());
        let (processing_result, runtime) = processing
            .join()
            .map_err(|_| "worker processing thread panicked".to_owned())?;
        #[cfg(target_os = "macos")]
        for slot in editor_host.windows.keys().copied().collect::<Vec<_>>() {
            if let Err(error) = editor_host.close_editor(slot) {
                eprintln!("sp-plugin-worker: could not detach editor on shutdown: {error}");
            }
        }
        // VST3 controller teardown belongs on its loading thread, after native views detach.
        drop(runtime);
        let _ = std::fs::remove_file(&configuration.control_socket);
        control_result?;
        processing_result
    }
    #[cfg(not(feature = "sdk"))]
    {
        let _ = configuration;
        Err("worker built without the sdk feature cannot run production mode".to_owned())
    }
}

#[cfg(feature = "sdk")]
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::needless_pass_by_value,
    reason = "the processing thread owns its channel endpoints and ordered scheduling transitions"
)]
fn run_production_processing_thread(
    mut region: SharedMemoryRegion,
    clock: MonotonicClock,
    mut heartbeat: HeartbeatPublisher,
    runtime: &mut Option<Box<ProductionRuntime>>,
    mut processing_endpoint: ProcessingControlEndpoint,
    main_request: SyncSender<MainThreadRequest>,
    main_response: Receiver<MainThreadResponse>,
    editor_request: SyncSender<ControlRequest>,
    editor_response: Receiver<LoadingReply>,
    retired: &SyncSender<Vst3Facade>,
    shutdown_requested: &AtomicBool,
) -> Result<(), String> {
    let mut audio_policy = sp_shared_memory_macos::AudioThreadPolicy::new()
        .map_err(|error| format!("could not initialize worker scheduling: {error}"))?;
    let mut last_audio = None;
    while !shutdown_requested.load(Ordering::Acquire) {
        if let Ok(reply) = editor_response.try_recv() {
            let response = match reply {
                LoadingReply::Response(response) => response,
                LoadingReply::Apply { request, edit } => {
                    let result = runtime
                        .as_mut()
                        .expect("runtime is present between commands")
                        .apply_chain_edit(edit, retired);
                    ProductionRuntime::response(&request, result.map(|()| Vec::new()))
                }
            };
            processing_endpoint
                .try_reply(response)
                .map_err(|_| "worker control reply mailbox is full".to_owned())?;
        }
        if last_audio.is_some_and(|(idle_deadline, _)| clock.now_ticks() >= idle_deadline) {
            audio_policy.leave().map_err(|error| error.to_string())?;
            last_audio = None;
        }
        region
            .bank()
            .header
            .worker_loop_tick
            .store(clock.now_ticks(), Ordering::Release);
        // Snapshot before scanning: a publication during the scan invalidates the wait.
        let wake_sequence = region.request_wake_sequence();
        region
            .bank()
            .header
            .worker_wait_sequence
            .store(wake_sequence, Ordering::Release);
        region
            .bank()
            .header
            .worker_phase
            .store(WORKER_PHASE_SCANNING, Ordering::Release);
        let mut observed_request = false;
        for slot_index in 0..BLOCK_SLOT_COUNT {
            let bank = region.bank_mut();
            let header = &bank.header;
            let Some(slot) = bank.slots.get_mut(slot_index) else {
                continue;
            };
            let runtime = runtime
                .as_mut()
                .expect("runtime is returned after editor command");
            match process_production_request(
                slot,
                header,
                runtime.worker_id,
                clock,
                &mut heartbeat,
                &mut runtime.rack,
                &mut audio_policy,
            )? {
                RequestProgress::Idle => {}
                RequestProgress::Retry => observed_request = true,
                RequestProgress::Processed(frames) => {
                    observed_request = true;
                    let idle_ticks = clock.duration_to_ticks(Duration::from_nanos(
                        u64::from(frames) * 2_000_000_000 / 48_000,
                    ));
                    last_audio = Some((clock.now_ticks().saturating_add(idle_ticks), frames));
                }
            }
        }
        region
            .bank()
            .header
            .worker_phase
            .store(WORKER_PHASE_CONTROL, Ordering::Release);
        if let Some(runtime) = runtime.as_mut() {
            publish_worker_telemetry(region.bank(), runtime);
        }
        if processing_endpoint.has_pending() {
            drain_production_commands(
                &mut processing_endpoint,
                runtime,
                &main_request,
                &main_response,
                &editor_request,
                &mut audio_policy,
                clock,
            )?;
            if let Some(runtime) = runtime.as_mut() {
                publish_worker_telemetry(region.bank(), runtime);
            }
            // Recheck published audio before taking another control command. A burst of
            // controller requests must not keep the rack away from its next block.
            observed_request |= processing_endpoint.has_pending();
        }
        heartbeat.publish_if_due(&region.bank().header)?;
        if let Some((idle_deadline, frames)) = last_audio
            && clock.now_ticks() < idle_deadline
        {
            audio_policy
                .enter(frames, 48_000)
                .map_err(|error| error.to_string())?;
        }
        if !observed_request {
            let wait_timeout = last_audio.map_or(EDITOR_POLL_INTERVAL, |(idle_deadline, _)| {
                clock
                    .ticks_to_duration(idle_deadline.saturating_sub(clock.now_ticks()))
                    .min(EDITOR_POLL_INTERVAL)
            });
            if wait_timeout.is_zero() {
                continue;
            }
            // The finite wait also services control, editor requests, and heartbeat when
            // the device is stopped. Only workers wait; the device callback only wakes.
            region
                .bank()
                .header
                .worker_phase
                .store(WORKER_PHASE_WAITING, Ordering::Release);
            region
                .wait_for_request(wake_sequence, wait_timeout)
                .map_err(|error| format!("worker request wait failed: {error}"))?;
            region
                .bank()
                .header
                .worker_phase
                .store(WORKER_PHASE_SCANNING, Ordering::Release);
        }
    }
    region
        .bank()
        .header
        .worker_phase
        .store(WORKER_PHASE_STOPPING, Ordering::Release);
    Ok(())
}

#[cfg(feature = "sdk")]
fn drain_production_commands(
    endpoint: &mut ProcessingControlEndpoint,
    runtime: &mut Option<Box<ProductionRuntime>>,
    main_request: &SyncSender<MainThreadRequest>,
    main_response: &Receiver<MainThreadResponse>,
    editor_request: &SyncSender<ControlRequest>,
    audio_policy: &mut sp_shared_memory_macos::AudioThreadPolicy,
    clock: MonotonicClock,
) -> Result<(), String> {
    let Some(request) = endpoint.try_receive() else {
        return Ok(());
    };
    if is_chain_edit(request.operation())
        && let Err(error) = runtime
            .as_ref()
            .expect("runtime has one owner")
            .check_chain_edit(&request)
    {
        let response = ProductionRuntime::response(&request, Err(error));
        endpoint
            .try_reply(response)
            .map_err(|_| "worker control reply mailbox is full".to_owned())?;
        return Ok(());
    }
    if request.operation() == ControlOperation::CaptureEditorPreview
        && clock.now_ticks().saturating_sub(
            runtime
                .as_ref()
                .expect("runtime has one owner")
                .loading_pass
                .load(Ordering::Acquire),
        ) > clock.duration_to_ticks(EDITOR_THREAD_STALL)
    {
        let response = ProductionRuntime::response(
            &request,
            Err("worker editor thread is busy; poll again later".to_owned()),
        );
        endpoint
            .try_reply(response)
            .map_err(|_| "worker control reply mailbox is full".to_owned())?;
        return Ok(());
    }
    if routes_to_loading_thread(&request) {
        if let Err(error) = editor_request.try_send(request) {
            let request = match error {
                TrySendError::Full(request) | TrySendError::Disconnected(request) => request,
            };
            let response = ProductionRuntime::response(
                &request,
                Err("worker main-thread editor queue is unavailable".to_owned()),
            );
            endpoint
                .try_reply(response)
                .map_err(|_| "worker control reply mailbox is full".to_owned())?;
        }
        return Ok(());
    }
    audio_policy.leave().map_err(|error| error.to_string())?;
    let response = if is_main_thread_operation(request.operation()) {
        if let Err(error) = main_request.send(MainThreadRequest {
            runtime: runtime.take().expect("runtime has one owner"),
            request,
        }) {
            *runtime = Some(error.0.runtime);
            return Err("worker main-thread editor owner stopped".to_owned());
        }
        let returned = main_response
            .recv()
            .map_err(|_| "worker main-thread editor owner stopped".to_owned())?;
        *runtime = Some(returned.runtime);
        returned.response
    } else {
        runtime
            .as_mut()
            .expect("runtime has one owner")
            .handle_control(request)
    };
    let _ = endpoint.try_reply(response);
    Ok(())
}

#[cfg(feature = "sdk")]
const fn is_editor_operation(operation: ControlOperation) -> bool {
    matches!(
        operation,
        ControlOperation::OpenNativeEditor
            | ControlOperation::CloseNativeEditor
            | ControlOperation::FocusNativeEditor
            | ControlOperation::ResizeNativeEditor
            | ControlOperation::CaptureEditorPreview
    )
}

/// Reads that the loading thread serves without pausing the processing thread.
#[cfg(feature = "sdk")]
const fn is_live_read_operation(operation: ControlOperation) -> bool {
    matches!(
        operation,
        ControlOperation::BeginStateCapture
            | ControlOperation::ReadStateChunk
            | ControlOperation::ReleaseStateTransfer
            | ControlOperation::ReadParameters
    )
}

/// Plug-in insert, removal, and reorder while the rack keeps processing.
#[cfg(feature = "sdk")]
const fn is_chain_edit(operation: ControlOperation) -> bool {
    matches!(
        operation,
        ControlOperation::LoadPlugin | ControlOperation::UnloadSlot | ControlOperation::ReorderRack
    )
}

/// Chooses the loading-thread queue for editor work, live reads, and chain edits. Chunk and
/// release requests carry a transfer ID; only IDs from the live-capture space belong to the
/// loading thread.
#[cfg(feature = "sdk")]
fn routes_to_loading_thread(request: &ControlRequest) -> bool {
    let transfer_id = match request.operation() {
        ControlOperation::ReadStateChunk => StateChunkRequest::decode(request.payload())
            .map(|chunk| chunk.id)
            .ok(),
        ControlOperation::ReleaseStateTransfer => StateTransferId::decode(request.payload())
            .map(|transfer| transfer.id)
            .ok(),
        operation => {
            return is_editor_operation(operation)
                || is_live_read_operation(operation)
                || is_chain_edit(operation);
        }
    };
    transfer_id.is_some_and(|id| id >= LIVE_CAPTURE_TRANSFER_BASE)
}

/// Slot `i` receives the plug-in previously at `from[i]`: each `current` slot takes the plug-in
/// from the matching `order` slot, and unlisted slots stay put.
#[cfg(feature = "sdk")]
fn slot_permutation(order: &SlotOrder) -> [usize; MAX_PLUGINS_PER_RACK] {
    let mut from = std::array::from_fn(|slot| slot);
    for (&slot, &source) in order.current.iter().zip(&order.order) {
        from[usize::from(slot)] = usize::from(source);
    }
    from
}

#[cfg(feature = "sdk")]
fn permute_slot_keys<V>(map: &mut BTreeMap<usize, V>, from: &[usize; MAX_PLUGINS_PER_RACK]) {
    let mut previous = std::mem::take(map);
    for (slot, source) in from.iter().enumerate() {
        if let Some(value) = previous.remove(source) {
            map.insert(slot, value);
        }
    }
}

#[cfg(feature = "sdk")]
const fn is_main_thread_operation(operation: ControlOperation) -> bool {
    matches!(
        operation,
        ControlOperation::RebuildRack
            | ControlOperation::ActivateSlot
            | ControlOperation::DeactivateSlot
            | ControlOperation::ParameterMetadata
            | ControlOperation::ReadParameter
            | ControlOperation::CaptureState
            | ControlOperation::RestoreState
            | ControlOperation::BeginStateRestore
            | ControlOperation::WriteStateChunk
            | ControlOperation::CommitStateRestore
            | ControlOperation::ReleaseStateTransfer
            | ControlOperation::Shutdown
    )
}

fn bind_control_listener(path: &std::path::Path) -> Result<UnixListener, String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            std::fs::remove_file(path)
                .map_err(|error| format!("could not replace stale control socket: {error}"))?;
        }
        Ok(_) => return Err("worker control path exists and is not a socket".to_owned()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("could not inspect control socket path: {error}")),
    }
    UnixListener::bind(path)
        .map_err(|error| format!("could not bind worker control socket: {error}"))
}

#[cfg(feature = "sdk")]
enum RequestProgress {
    Idle,
    Retry,
    Processed(u32),
}

#[cfg(feature = "sdk")]
#[allow(
    clippy::too_many_arguments,
    reason = "one processing call owns the claimed audio block and its scheduling policy"
)]
fn process_production_request(
    slot: &mut BlockSlot,
    header: &ProtocolHeader,
    worker_id: u32,
    clock: MonotonicClock,
    heartbeat: &mut HeartbeatPublisher,
    rack: &mut RackProcessor<Vst3Facade>,
    audio_policy: &mut sp_shared_memory_macos::AudioThreadPolicy,
) -> Result<RequestProgress, String> {
    let Some(observed_ticket) = requested_ticket(slot)? else {
        return Ok(RequestProgress::Idle);
    };
    let ticket =
        match slot.claim_ticket_for_processing_at(worker_id, observed_ticket, clock.now_ticks()) {
            Ok(ticket) => ticket,
            Err(
                ProtocolError::UnexpectedState | ProtocolError::Owned | ProtocolError::StaleTicket,
            ) => {
                // A publication wake is not repeated after transient owner contention.
                // Rescan instead of sleeping while the observed request may remain pending.
                return Ok(RequestProgress::Retry);
            }
            Err(error) => return Err(format!("invalid shared-memory request: {error}")),
        };
    let frames = slot.metadata.frame_count;
    audio_policy
        .enter(frames, 48_000)
        .map_err(|error| error.to_string())?;
    rack.process_block(slot)
        .map_err(|error| format!("could not process VST3 rack: {error}"))?;
    slot.publish_completion_at(worker_id, ticket, clock.now_ticks())
        .map_err(|error| format!("could not publish completion: {error}"))?;
    heartbeat.publish_now(header)?;
    Ok(RequestProgress::Processed(frames))
}

#[cfg(feature = "sdk")]
fn publish_worker_telemetry(bank: &SharedBank, runtime: &mut ProductionRuntime) {
    bank.header
        .worker_latency_samples
        .store(runtime.rack.latency_samples(), Ordering::Release);
    for (slot, flags) in runtime
        .rack
        .take_slot_restart_flags()
        .into_iter()
        .enumerate()
    {
        if flags != 0 {
            let _ = bank.feedback.publish_restart(slot, flags);
            runtime.restart_flags_seen |= flags;
            bank.header
                .worker_restart_requested
                .fetch_or(flags, Ordering::AcqRel);
        }
    }
}

#[cfg(feature = "sdk")]
struct Vst3Facade {
    adapter: HostSdkRackAdapter,
    topology: PluginTopology,
    layout: MainBusLayout,
    latency_samples: u32,
    output_changes: BoundedOutputChanges<512>,
}

#[cfg(feature = "sdk")]
impl Vst3Facade {
    /// Loads the plug-in with its default main buses and, when `sidechain` is set, its first
    /// auxiliary input negotiated to stereo and active.
    fn load(selection: &Vst3ClassSelection, sidechain: bool) -> Result<Self, String> {
        let format = ProcessingFormat::new(48_000.0, 256).map_err(|error| error.to_string())?;
        let mut adapter = HostSdkRackAdapter::new();
        adapter.set_editor_gesture_feedback(false);
        adapter
            .select_module_class(selection, format)
            .map_err(|error| error.to_string())?;
        adapter
            .initialize_component()
            .map_err(|error| error.to_string())?;
        adapter
            .initialize_controller()
            .map_err(|error| error.to_string())?;
        adapter
            .connect_component_controller()
            .map_err(|error| error.to_string())?;
        let buses = adapter
            .discover_buses()
            .map_err(|error| error.to_string())?;
        let input_channels = buses
            .audio_inputs
            .first()
            .map_or(0, |bus| bus.channel_count);
        let output_channels = buses
            .audio_outputs
            .first()
            .map_or(0, |bus| bus.channel_count);
        let main = MainBusLayout::new(
            input_channels,
            output_channels,
            !buses.event_inputs.is_empty(),
        )
        .map_err(|error| error.to_string())?;
        buses
            .validate_fixed(main)
            .map_err(|error| error.to_string())?;
        let layout = MainBusLayout {
            sidechain_input: sidechain,
            ..main
        };
        adapter
            .negotiate_main_buses(layout)
            .map_err(|error| error.to_string())?;
        if sidechain {
            adapter
                .discover_buses()
                .map_err(|error| error.to_string())?
                .validate_fixed(layout)
                .map_err(|error| error.to_string())?;
        }
        adapter
            .activate_main_buses(layout)
            .map_err(|error| error.to_string())?;
        let latency_samples = adapter
            .latency_samples()
            .map_err(|error| error.to_string())?;
        Ok(Self {
            adapter,
            topology: PluginTopology {
                input_channels: u32::from(layout.input_channels),
                output_channels: u32::from(layout.output_channels),
            },
            layout,
            latency_samples,
            output_changes: BoundedOutputChanges::default(),
        })
    }
}

#[cfg(feature = "sdk")]
impl PluginFacade for Vst3Facade {
    fn process(
        &mut self,
        input: &PlanarBlock,
        output: &mut PlanarBlock,
        request: PluginProcessRequest<'_>,
    ) -> Result<(), PluginRuntimeError> {
        let empty = &[];
        let input_channels = [
            if request.input_channels > 0 {
                &input.channels()[0][..request.frame_count]
            } else {
                empty
            },
            if request.input_channels > 1 {
                &input.channels()[1][..request.frame_count]
            } else {
                empty
            },
        ];
        let (left, right) = output.channels_mut().split_at_mut(1);
        let output_channels = [
            &mut left[0][..request.frame_count],
            &mut right[0][..request.frame_count],
        ];
        let mut midi = BoundedMidiEvents::default();
        for event in request.midi_events {
            if let Some(message) = translate_midi_event(*event) {
                let _ = midi.push_preserving_note_off(TimedMidiMessage {
                    message,
                    sample_offset: u16::try_from(event.frame_offset).unwrap_or(u16::MAX),
                });
            }
        }
        let mut parameters = BoundedParameterChanges::default();
        for event in request.automation_events.iter().filter(|event| {
            event.event_type == sp_protocol::BLOCK_EVENT_PARAMETER
                && (event.flags == 0
                    || usize::try_from(event.flags).ok() == Some(request.slot_index + 1))
        }) {
            let _ = parameters.push_coalescing(TimedParameterChange {
                parameter_id: event.key,
                normalized: f64::from(event.value),
                sample_offset: u16::try_from(event.frame_offset).unwrap_or(u16::MAX),
            });
        }
        self.output_changes.clear();
        self.adapter
            .process(PluginProcessBlock {
                frames: request.frame_count,
                input: PlanarAudioInput::new(
                    input_channels,
                    u8::try_from(request.input_channels)
                        .map_err(|_| PluginRuntimeError::InvalidRequest)?,
                ),
                output: PlanarAudioOutput::new(
                    output_channels,
                    u8::try_from(request.output_channels)
                        .map_err(|_| PluginRuntimeError::InvalidRequest)?,
                ),
                sidechain: request.sidechain.map(|[left, right]| {
                    [&left[..request.frame_count], &right[..request.frame_count]]
                }),
                midi: &midi,
                parameter_changes: &parameters,
                output_changes: &mut self.output_changes,
            })
            .map_err(|error| PluginRuntimeError::Plugin(error.to_string()))?;
        Ok(())
    }

    fn set_active(&mut self, active: bool) -> Result<(), PluginRuntimeError> {
        if active {
            self.adapter.start_processing()
        } else {
            self.adapter.stop_processing()
        }
        .map_err(|error| PluginRuntimeError::Plugin(error.to_string()))?;
        if active {
            self.latency_samples = self
                .adapter
                .latency_samples()
                .map_err(|error| PluginRuntimeError::Plugin(error.to_string()))?;
        }
        Ok(())
    }

    fn set_parameter(
        &mut self,
        parameter_id: u32,
        normalized: f64,
    ) -> Result<(), PluginRuntimeError> {
        self.adapter
            .set_parameter(parameter_id, normalized)
            .map_err(|error| PluginRuntimeError::Plugin(error.to_string()))
    }

    fn parameter_metadata(
        &mut self,
        parameter_id: u32,
    ) -> Result<ParameterMetadata, PluginRuntimeError> {
        let parameter = self
            .adapter
            .parameter_metadata(parameter_id)
            .map_err(|error| PluginRuntimeError::Plugin(error.to_string()))?;
        let formatted = self
            .adapter
            .format_parameter(parameter.id, parameter.normalized)
            .unwrap_or_else(|_| format!("{:.3}", parameter.normalized));
        Ok(ParameterMetadata {
            id: ParameterId {
                value: u64::from(parameter.id),
            },
            normalized: parameter.normalized,
            default_normalized: parameter.default_normalized,
            step_count: parameter.step_count,
            flags: parameter.flags.0,
            name: parameter.title,
            unit: parameter.unit,
            formatted,
        })
    }

    fn read_parameter(&mut self, parameter_id: u32) -> Result<f64, PluginRuntimeError> {
        self.adapter
            .read_parameter(parameter_id)
            .map_err(|error| PluginRuntimeError::Plugin(error.to_string()))
    }

    fn capture_state(&mut self) -> Result<PluginState, PluginRuntimeError> {
        self.adapter
            .capture_state_streams()
            .map(|state| PluginState {
                component: state.component,
                controller: state.controller,
            })
            .map_err(|error| PluginRuntimeError::Plugin(error.to_string()))
    }

    fn restore_state(&mut self, state: &PluginState) -> Result<(), PluginRuntimeError> {
        self.adapter
            .restore_state_streams(&Vst3StateStreams {
                component: state.component.clone(),
                controller: state.controller.clone(),
            })
            .map_err(|error| PluginRuntimeError::Plugin(error.to_string()))?;
        let buses = self
            .adapter
            .discover_buses()
            .map_err(|error| PluginRuntimeError::Plugin(error.to_string()))?;
        buses
            .validate_fixed(self.layout)
            .map_err(|error| PluginRuntimeError::Plugin(error.to_string()))
    }

    fn latency_samples(&self) -> u32 {
        self.latency_samples
    }

    fn take_restart_flags(&mut self) -> u32 {
        self.adapter.take_plugin_restart_flags().unwrap_or_default()
    }
}

#[cfg(feature = "sdk")]
fn translate_midi_event(event: sp_shared_memory::MidiEvent) -> Option<MidiMessage> {
    if event.data_length == 0 || event.data_length > 3 || event.port != 0 {
        return None;
    }
    let status = event.data[0];
    let channel = status & 0x0f;
    match status & 0xf0 {
        0x80 if event.data_length == 3 => Some(MidiMessage::NoteOff {
            channel,
            note: event.data[1],
            velocity: event.data[2],
        }),
        0x90 if event.data_length == 3 && event.data[2] == 0 => Some(MidiMessage::NoteOff {
            channel,
            note: event.data[1],
            velocity: 0,
        }),
        0x90 if event.data_length == 3 => Some(MidiMessage::NoteOn {
            channel,
            note: event.data[1],
            velocity: event.data[2],
        }),
        0xb0 if event.data_length == 3 => Some(MidiMessage::ControlChange {
            channel,
            controller: event.data[1],
            value: event.data[2],
        }),
        0xc0 if event.data_length == 2 => Some(MidiMessage::ProgramChange {
            channel,
            program: event.data[1],
        }),
        0xd0 if event.data_length == 2 => Some(MidiMessage::ChannelPressure {
            channel,
            pressure: event.data[1],
        }),
        0xe0 if event.data_length == 3 => Some(MidiMessage::PitchBend {
            channel,
            value: u16::from(event.data[1]) | (u16::from(event.data[2]) << 7),
        }),
        _ => None,
    }
}

#[cfg(feature = "sdk")]
struct ProductionRuntime {
    worker_id: u32,
    rack: RackProcessor<Vst3Facade>,
    loaded: [Option<WirePluginSlotConfiguration>; 8],
    rack_committed: bool,
    state_transfers: StateTransfers,
    shutdown_requested: Arc<AtomicBool>,
    /// When the loading thread last began a loop pass, in monotonic ticks.
    loading_pass: Arc<std::sync::atomic::AtomicU64>,
    // Legacy health RPCs report whether this worker has ever observed a restart. The host's
    // per-slot cursor, not these RPCs, acknowledges pending maintenance in shared memory.
    restart_flags_seen: u32,
}

#[cfg(feature = "sdk")]
impl ProductionRuntime {
    fn new(
        worker_id: u32,
        facade: Vst3Facade,
        configuration: WirePluginSlotConfiguration,
    ) -> Result<Self, String> {
        let mut rack = RackProcessor::new();
        let topology = facade.topology;
        let sidechain = facade.layout.sidechain_input;
        let slot = usize::from(configuration.slot);
        rack.replace_slot(
            slot,
            facade,
            PluginSlotConfiguration {
                topology,
                bypassed: false,
                active: false,
                sidechain,
            },
        )
        .map_err(|error| error.to_string())?;
        rack.set_active(slot, true)
            .map_err(|error| error.to_string())?;
        let mut loaded = std::array::from_fn(|_| None);
        loaded[slot] = Some(configuration);
        Ok(Self {
            worker_id,
            rack,
            loaded,
            rack_committed: false,
            state_transfers: StateTransfers::default(),
            shutdown_requested: Arc::new(AtomicBool::new(false)),
            loading_pass: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            restart_flags_seen: 0,
        })
    }

    fn slot_index(request: &ControlRequest) -> Result<usize, String> {
        let identity = request
            .slot()
            .ok_or("request did not identify a plugin slot")?;
        let index = usize::try_from(identity.get().saturating_sub(1))
            .map_err(|_| "invalid slot identity")?;
        (index < 8)
            .then_some(index)
            .ok_or("slot identity is outside alpha rack capacity".to_owned())
    }

    fn inactive_slot(&self, request: &ControlRequest) -> Result<usize, String> {
        let slot = Self::slot_index(request)?;
        let configuration = self
            .rack
            .slot_configuration(slot)
            .ok_or("state transfer slot is not loaded")?;
        if configuration.active {
            return Err("state transfer requires an inactive plug-in slot".to_owned());
        }
        Ok(slot)
    }

    fn request_configuration_slot(
        request: &ControlRequest,
        configuration: &WirePluginSlotConfiguration,
    ) -> Result<usize, String> {
        let request_slot = Self::slot_index(request)?;
        if request_slot != usize::from(configuration.slot) {
            return Err("plugin payload slot does not match control slot identity".to_owned());
        }
        Ok(request_slot)
    }

    fn load_facade(configuration: &WirePluginSlotConfiguration) -> Result<Vst3Facade, String> {
        let class_id = configuration
            .class_id
            .as_deref()
            .filter(|class_id| !class_id.is_empty())
            .ok_or("plug-in load requires a scanner-selected class ID")?;
        Vst3Facade::load(
            &Vst3ClassSelection::new(Vst3BundlePath::new(&configuration.bundle_path), class_id),
            configuration.sidechain_active,
        )
    }

    /// Checks a chain edit against the rack before the loading thread prepares it.
    fn check_chain_edit(&self, request: &ControlRequest) -> Result<(), String> {
        match request.operation() {
            ControlOperation::LoadPlugin => {
                let configuration = WirePluginSlotConfiguration::decode(request.payload())
                    .map_err(|error| error.to_string())?;
                let slot = Self::request_configuration_slot(request, &configuration)?;
                if self.rack.slot_configuration(slot).is_some() {
                    return Err("plug-in insert requires an empty slot".to_owned());
                }
                Ok(())
            }
            ControlOperation::UnloadSlot => {
                let slot = Self::slot_index(request)?;
                self.rack
                    .slot_configuration(slot)
                    .map(|_| ())
                    .ok_or_else(|| "plug-in slot is not loaded".to_owned())
            }
            ControlOperation::ReorderRack => SlotOrder::decode(request.payload())
                .map(|_| ())
                .map_err(|error| error.to_string()),
            _ => Err("request is not a chain edit".to_owned()),
        }
    }

    /// Applies a prepared chain edit between blocks. Removed plug-ins go to the loading thread
    /// for teardown; if its queue is full they drop here instead.
    fn apply_chain_edit(
        &mut self,
        edit: ChainEdit,
        retired: &SyncSender<Vst3Facade>,
    ) -> Result<(), String> {
        self.state_transfers.clear();
        match edit {
            ChainEdit::Insert(inserted) => {
                let InsertedPlugin {
                    configuration,
                    facade,
                } = *inserted;
                let slot = usize::from(configuration.slot);
                let topology = facade.topology;
                let sidechain = facade.layout.sidechain_input;
                self.rack
                    .replace_slot(
                        slot,
                        facade,
                        PluginSlotConfiguration {
                            topology,
                            bypassed: false,
                            active: true,
                            sidechain,
                        },
                    )
                    .map_err(|error| error.to_string())?;
                self.rack
                    .start_warmup(slot)
                    .map_err(|error| error.to_string())?;
                self.loaded[slot] = Some(configuration);
            }
            ChainEdit::Remove(slot) => {
                if let Some(facade) = self
                    .rack
                    .remove_slot(slot)
                    .map_err(|error| error.to_string())?
                {
                    let _ = retired.try_send(facade);
                }
                self.loaded[slot] = None;
            }
            ChainEdit::Reorder(from) => {
                self.rack
                    .reorder(&from)
                    .map_err(|error| error.to_string())?;
                let mut previous =
                    std::mem::replace(&mut self.loaded, std::array::from_fn(|_| None));
                for (slot, &source) in from.iter().enumerate() {
                    self.loaded[slot] = previous[source].take().map(|mut configuration| {
                        configuration.slot = u8::try_from(slot).expect("rack slot fits u8");
                        configuration
                    });
                }
            }
        }
        Ok(())
    }

    fn rebuild_rack(&mut self, topology: RackTopology) -> Result<Vec<u8>, String> {
        let mut requested: [Option<WirePluginSlotConfiguration>; 8] = std::array::from_fn(|_| None);
        for configuration in topology.slots {
            let slot = usize::from(configuration.slot);
            requested[slot] = Some(configuration);
        }

        #[allow(
            clippy::needless_range_loop,
            reason = "the loop takes entries out of `requested` while comparing against `loaded`"
        )]
        for slot in 0..8 {
            if self.loaded[slot] == requested[slot] {
                continue;
            }

            let prior_state = if self.rack.slot_configuration(slot).is_some() {
                self.rack
                    .set_active(slot, false)
                    .map_err(|error| error.to_string())?;
                let state = self
                    .rack_committed
                    .then(|| self.rack.capture_state(slot))
                    .transpose()
                    .map_err(|error| error.to_string())?;
                let _ = self
                    .rack
                    .remove_slot(slot)
                    .map_err(|error| error.to_string())?;
                state
            } else {
                None
            };
            self.loaded[slot] = None;

            let Some(configuration) = requested[slot].take() else {
                continue;
            };
            let facade = Self::load_facade(&configuration)?;
            let plugin_topology = facade.topology;
            let sidechain = facade.layout.sidechain_input;
            self.rack
                .replace_slot(
                    slot,
                    facade,
                    PluginSlotConfiguration {
                        topology: plugin_topology,
                        bypassed: false,
                        active: false,
                        sidechain,
                    },
                )
                .map_err(|error| error.to_string())?;
            if let Some(state) = prior_state.as_ref() {
                self.rack
                    .restore_state(slot, state)
                    .map_err(|error| error.to_string())?;
            }
            self.rack
                .set_active(slot, true)
                .map_err(|error| error.to_string())?;
            self.loaded[slot] = Some(configuration);
        }
        self.rack_committed = true;
        Ok(Vec::new())
    }

    fn handle_main_thread_control(
        &mut self,
        request: ControlRequest,
        host: &mut EditorHost,
    ) -> ControlResponse {
        if matches!(
            request.operation(),
            ControlOperation::RebuildRack | ControlOperation::Shutdown
        ) {
            host.state_transfers.clear();
        }

        #[cfg(not(target_os = "macos"))]
        {
            return self.handle_control(request);
        }

        #[cfg(target_os = "macos")]
        {
            if matches!(
                request.operation(),
                ControlOperation::RebuildRack | ControlOperation::Shutdown
            ) {
                for slot in host.windows.keys().copied().collect::<Vec<_>>() {
                    if let Err(error) = host.close_editor(slot) {
                        return Self::response(&request, Err(error));
                    }
                }
            }
            self.handle_control(request)
        }
    }

    fn unsupported(request: &ControlRequest, message: &str) -> ControlResponse {
        ControlResponse::error(
            request.request_id(),
            request.target(),
            request.slot(),
            ControlResponseStatus::Unsupported,
            ControlErrorRecord::new(ControlErrorCode::UNSUPPORTED, message)
                .expect("bounded unsupported error"),
        )
        .expect("bounded unsupported response")
    }

    fn response(request: &ControlRequest, result: Result<Vec<u8>, String>) -> ControlResponse {
        match result {
            Ok(payload) => ControlResponse::success(
                request.request_id(),
                request.target(),
                request.slot(),
                &payload,
            ),
            Err(message) => {
                let unsupported =
                    message.contains("unsupported") || message.contains("does not expose");
                ControlResponse::error(
                    request.request_id(),
                    request.target(),
                    request.slot(),
                    if unsupported {
                        ControlResponseStatus::Unsupported
                    } else {
                        ControlResponseStatus::Failed
                    },
                    ControlErrorRecord::new(
                        if unsupported {
                            ControlErrorCode::UNSUPPORTED
                        } else {
                            ControlErrorCode::OPERATION_FAILED
                        },
                        &message,
                    )
                    .expect("bounded control error"),
                )
            }
        }
        .unwrap_or_else(|_| {
            ControlResponse::error(
                request.request_id(),
                request.target(),
                request.slot(),
                ControlResponseStatus::Failed,
                ControlErrorRecord::new(
                    ControlErrorCode::INTERNAL,
                    "worker could not encode control response",
                )
                .expect("static error"),
            )
            .expect("static error response")
        })
    }
}

#[cfg(feature = "sdk")]
impl ProcessingControlHandler for ProductionRuntime {
    #[allow(
        clippy::too_many_lines,
        reason = "flat dispatch over every control operation reads clearest in one match"
    )]
    fn handle_control(&mut self, request: ControlRequest) -> ControlResponse {
        if matches!(
            request.operation(),
            ControlOperation::RebuildRack | ControlOperation::Shutdown
        ) {
            self.state_transfers.clear();
        }
        let result = match request.operation() {
            ControlOperation::PreloadPlugin => {
                return Self::unsupported(&request, "LoadPlugin loads and inserts in one step");
            }
            ControlOperation::RebuildRack => RackTopology::decode(request.payload())
                .map_err(|error| error.to_string())
                .and_then(|topology| self.rebuild_rack(topology)),
            ControlOperation::ActivateSlot => Self::slot_index(&request)
                .and_then(|slot| {
                    if self.state_transfers.holds_slot(slot) {
                        return Err("state transfer must finish before activation".to_owned());
                    }
                    self.rack
                        .set_active(slot, true)
                        .map_err(|error| error.to_string())
                })
                .map(|()| Vec::new()),
            ControlOperation::DeactivateSlot => Self::slot_index(&request)
                .and_then(|slot| {
                    self.rack
                        .set_active(slot, false)
                        .map_err(|error| error.to_string())
                })
                .map(|()| Vec::new()),
            ControlOperation::SetSlotBypass => Self::slot_index(&request).and_then(|slot| {
                Bypass::decode(request.payload())
                    .map_err(|error| error.to_string())
                    .and_then(|bypass| {
                        self.rack
                            .set_bypassed(slot, bypass.enabled)
                            .map_err(|error| error.to_string())
                    })
                    .map(|()| Vec::new())
            }),
            ControlOperation::SetRackBypass => Bypass::decode(request.payload())
                .map(|bypass| {
                    self.rack.set_rack_bypassed(bypass.enabled);
                    Vec::new()
                })
                .map_err(|error| error.to_string()),
            ControlOperation::ParameterMetadata => Self::slot_index(&request).and_then(|slot| {
                ParameterId::decode(request.payload())
                    .map_err(|error| error.to_string())
                    .and_then(|id| {
                        u32::try_from(id.value)
                            .map_err(|_| "parameter ID exceeds VST3 u32 range".to_owned())
                            .and_then(|id| {
                                self.rack
                                    .parameter_metadata(slot, id)
                                    .map_err(|error| error.to_string())
                            })
                            .and_then(|metadata| {
                                metadata.encode().map_err(|error| error.to_string())
                            })
                    })
            }),
            ControlOperation::ReadParameter => Self::slot_index(&request).and_then(|slot| {
                ParameterId::decode(request.payload())
                    .map_err(|error| error.to_string())
                    .and_then(|id| {
                        u32::try_from(id.value)
                            .map_err(|_| "parameter ID exceeds VST3 u32 range".to_owned())
                            .and_then(|id| {
                                self.rack
                                    .read_parameter(slot, id)
                                    .map_err(|error| error.to_string())
                            })
                            .and_then(|normalized| {
                                ParameterWrite { id, normalized }
                                    .encode()
                                    .map_err(|error| error.to_string())
                            })
                    })
            }),
            ControlOperation::BeginParameterGesture | ControlOperation::EndParameterGesture => {
                return Self::unsupported(
                    &request,
                    "vst3-host does not expose host-initiated IComponentHandler beginEdit/endEdit",
                );
            }
            ControlOperation::WriteParameter => Self::slot_index(&request).and_then(|slot| {
                ParameterWrite::decode(request.payload())
                    .map_err(|error| error.to_string())
                    .and_then(|write| {
                        u32::try_from(write.id.value)
                            .map_err(|_| "parameter ID exceeds VST3 u32 range".to_owned())
                            .and_then(|id| {
                                self.rack
                                    .set_parameter(slot, id, write.normalized)
                                    .map_err(|error| error.to_string())
                            })
                            .map(|()| Vec::new())
                    })
            }),
            ControlOperation::CaptureState => Self::slot_index(&request)
                .and_then(|slot| {
                    self.rack
                        .capture_state(slot)
                        .map_err(|error| error.to_string())
                })
                .and_then(|state| {
                    StateRestore {
                        component: state.component,
                        controller: state.controller,
                    }
                    .encode()
                    .map_err(|error| error.to_string())
                }),
            ControlOperation::RestoreState => Self::slot_index(&request).and_then(|slot| {
                StateRestore::decode(request.payload())
                    .map_err(|error| error.to_string())
                    .and_then(|state| {
                        self.rack
                            .restore_state(
                                slot,
                                &PluginState {
                                    component: state.component,
                                    controller: state.controller,
                                },
                            )
                            .map_err(|error| error.to_string())
                    })
                    .map(|()| Vec::new())
            }),
            ControlOperation::BeginStateRestore => self.inactive_slot(&request).and_then(|slot| {
                let lengths = StateTransferLengths::decode(request.payload())
                    .map_err(|error| error.to_string())?;
                let descriptor = self.state_transfers.begin_restore(
                    slot,
                    lengths.component_len,
                    lengths.controller_len,
                )?;
                StateTransferDescriptor {
                    id: descriptor.id,
                    component_len: descriptor.component_len,
                    controller_len: descriptor.controller_len,
                }
                .encode()
                .map_err(|error| error.to_string())
            }),
            ControlOperation::WriteStateChunk => self.inactive_slot(&request).and_then(|slot| {
                let chunk = StateChunkWrite::decode(request.payload())
                    .map_err(|error| error.to_string())?;
                self.state_transfers.write_chunk(
                    slot,
                    chunk.id,
                    chunk.stream,
                    chunk.offset,
                    &chunk.data,
                )?;
                Ok(Vec::new())
            }),
            ControlOperation::CommitStateRestore => self.inactive_slot(&request).and_then(|slot| {
                let transfer = StateTransferId::decode(request.payload())
                    .map_err(|error| error.to_string())?;
                let state = self.state_transfers.commit_restore(slot, transfer.id)?;
                self.rack
                    .restore_state(slot, &state)
                    .map_err(|error| error.to_string())?;
                Ok(Vec::new())
            }),
            ControlOperation::ReleaseStateTransfer => {
                self.inactive_slot(&request).and_then(|slot| {
                    let transfer = StateTransferId::decode(request.payload())
                        .map_err(|error| error.to_string())?;
                    self.state_transfers.release(slot, transfer.id)?;
                    Ok(Vec::new())
                })
            }
            ControlOperation::OpenNativeEditor
            | ControlOperation::CloseNativeEditor
            | ControlOperation::FocusNativeEditor
            | ControlOperation::ResizeNativeEditor
            | ControlOperation::CaptureEditorPreview
            | ControlOperation::BeginStateCapture
            | ControlOperation::ReadStateChunk
            | ControlOperation::ReadParameters
            | ControlOperation::LoadPlugin
            | ControlOperation::UnloadSlot
            | ControlOperation::ReorderRack => {
                return Self::unsupported(&request, "loading-thread operation missed its routing");
            }
            ControlOperation::NotifyLatency => Ok(u64::from(self.rack.latency_samples())
                .to_le_bytes()
                .to_vec()),
            ControlOperation::NotifyRestart => RestartReport {
                requested: self.restart_flags_seen != 0,
                reason: 0,
            }
            .encode()
            .map_err(|error| error.to_string()),
            ControlOperation::QueryHealth => HealthReport {
                online: true,
                current_slot: self
                    .rack
                    .current_slot()
                    .and_then(|slot| SlotIdentity::new(u64::try_from(slot + 1).ok()?).ok()),
                latency_samples: self.rack.latency_samples(),
                restart_requested: self.restart_flags_seen != 0,
            }
            .encode()
            .map_err(|error| error.to_string()),
            ControlOperation::QuerySlotAttribution => Ok(self
                .rack
                .current_slot()
                .map_or(0, |slot| u64::try_from(slot + 1).unwrap_or(0))
                .to_le_bytes()
                .to_vec()),
            ControlOperation::Shutdown => {
                self.shutdown_requested.store(true, Ordering::Release);
                Ok(Vec::new())
            }
        };
        Self::response(&request, result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sp_shared_memory::{MAX_FRAMES, SlotState};

    const BANK_NAME: &str = "/superposition-test";

    #[test]
    fn no_arguments_print_usage_mode() {
        assert_eq!(parse_worker_startup(Vec::<String>::new()).unwrap(), None);
    }

    #[cfg(feature = "sdk")]
    #[test]
    fn editor_operations_use_independent_main_thread_routing() {
        assert!(is_editor_operation(ControlOperation::OpenNativeEditor));
        assert!(is_editor_operation(ControlOperation::CloseNativeEditor));
        assert!(is_editor_operation(ControlOperation::FocusNativeEditor));
        assert!(is_editor_operation(ControlOperation::ResizeNativeEditor));
        assert!(is_editor_operation(ControlOperation::CaptureEditorPreview));
        assert!(!is_main_thread_operation(
            ControlOperation::OpenNativeEditor
        ));
        assert!(!is_editor_operation(ControlOperation::ReadParameter));
        assert!(is_main_thread_operation(ControlOperation::ReadParameter));
        assert!(!is_main_thread_operation(ControlOperation::ReadParameters));
        assert!(is_live_read_operation(ControlOperation::ReadParameters));
        assert!(is_live_read_operation(ControlOperation::BeginStateCapture));
        assert!(is_main_thread_operation(ControlOperation::CaptureState));
        assert!(!is_main_thread_operation(ControlOperation::WriteParameter));
        assert!(!is_main_thread_operation(ControlOperation::QueryHealth));
        assert!(is_main_thread_operation(ControlOperation::Shutdown));
    }

    #[test]
    fn production_parser_requires_explicit_rack_and_bank_identity() {
        let arguments = [
            "--worker",
            "--bank",
            BANK_NAME,
            WORKER_ID_OPTION,
            "7",
            "--rack-index",
            "3",
            "--rack-generation",
            "11",
            "--bank-index",
            "1",
            "--bank-generation",
            "17",
            "--control-socket",
            "/tmp/superposition-worker.sock",
            BUNDLE_OPTION,
            "Example.vst3",
            "--class-id",
            "example-class",
        ];
        let startup = parse_worker_startup(arguments)
            .expect("production arguments parse")
            .expect("startup");
        let WorkerStartup::Production(configuration) = startup else {
            panic!("expected production worker startup");
        };
        assert_eq!(configuration.rack_index, 3);
        assert_eq!(configuration.rack_generation, 11);
        assert_eq!(configuration.bank_index, 1);
        assert_eq!(configuration.bank_generation, 17);
        assert_eq!(configuration.worker_id, 7);

        assert!(
            parse_worker_startup([
                "--worker",
                "--bank",
                BANK_NAME,
                WORKER_ID_OPTION,
                "7",
                "--rack-index",
                "3",
                "--rack-generation",
                "11",
                "--bank-generation",
                "17",
                "--control-socket",
                "/tmp/superposition-worker.sock",
                BUNDLE_OPTION,
                "Example.vst3",
                "--class-id",
                "example-class",
            ])
            .is_err()
        );
    }

    fn standalone(arguments: &[&str]) -> Result<StandaloneWorkerConfiguration, String> {
        match parse_worker_startup(arguments.iter().copied())? {
            Some(WorkerStartup::Standalone(configuration)) => Ok(configuration),
            other => panic!("expected standalone worker startup, got {other:?}"),
        }
    }

    #[test]
    fn standalone_arguments_parse_bank_worker_and_optional_bundle() {
        let configuration = standalone(&[
            "--bank",
            BANK_NAME,
            WORKER_ID_OPTION,
            "7",
            BUNDLE_OPTION,
            "Example.vst3",
        ])
        .expect("standalone arguments parse");
        assert_eq!(configuration.bank_name, BANK_NAME);
        assert_eq!(configuration.worker_id, 7);
        assert_eq!(
            configuration.bundle,
            Some(Vst3BundlePath::new("Example.vst3"))
        );

        let without_bundle =
            standalone(&["--bank", BANK_NAME, WORKER_ID_OPTION, "7"]).expect("bundle is optional");
        assert_eq!(without_bundle.bundle, None);
    }

    #[test]
    fn readiness_is_newline_terminated_and_flush_failures_propagate() {
        let mut output = Vec::new();
        publish_readiness(&mut output).unwrap();
        assert_eq!(output, format!("{WORKER_READY_LINE}\n").as_bytes());

        let error = publish_readiness(FlushFailure).unwrap_err();
        assert!(error.contains("could not flush readiness"));
    }

    #[test]
    fn host_watchdog_accepts_eof_or_an_explicit_shutdown_byte() {
        wait_for_host_shutdown(io::empty()).unwrap();
        wait_for_host_shutdown(io::Cursor::new([1_u8])).unwrap();
    }

    struct FlushFailure;

    impl io::Write for FlushFailure {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("synthetic flush failure"))
        }
    }

    #[test]
    fn standalone_parser_rejects_missing_duplicate_unknown_and_reserved_worker_ids() {
        assert!(standalone(&["--bank", BANK_NAME]).is_err());
        assert!(standalone(&["--bank", BANK_NAME, WORKER_ID_OPTION]).is_err());
        assert!(
            standalone(&[
                "--bank",
                BANK_NAME,
                WORKER_ID_OPTION,
                "1",
                WORKER_ID_OPTION,
                "2",
            ])
            .is_err()
        );
        assert!(
            standalone(&[
                "--bank",
                BANK_NAME,
                WORKER_ID_OPTION,
                "1",
                "--fault-mode",
                "none"
            ])
            .is_err()
        );
        for worker_id in ["0", "4294967293", "4294967294", "4294967295"] {
            assert!(standalone(&["--bank", BANK_NAME, WORKER_ID_OPTION, worker_id]).is_err());
        }
    }

    #[test]
    fn bounded_no_op_copy_copies_common_channels_and_zeros_output_only_channels() {
        let mut slot = BlockSlot::new();
        slot.input_audio[0][0] = 0.25;
        slot.input_audio[0][1] = -0.5;
        slot.input_audio[0][2] = 99.0;
        slot.output_audio = [[-1.0; MAX_FRAMES]; 2];
        let ticket = BlockTicket {
            generation: 1,
            sequence: 1,
        };
        slot.publish_request(
            ticket,
            BlockRequest {
                frame_count: 2,
                input_channel_count: 1,
                output_channel_count: 2,
                midi_event_count: 0,
                event_count: 0,
                flags: 0,
                sidechain_slots: 0,
            },
        )
        .unwrap();
        slot.claim_for_processing(7).unwrap();

        copy_bounded_audio(&mut slot).unwrap();

        assert!((slot.output_audio[0][0] - 0.25).abs() < f32::EPSILON);
        assert!((slot.output_audio[0][1] + 0.5).abs() < f32::EPSILON);
        assert!((slot.output_audio[0][2] + 1.0).abs() < f32::EPSILON);
        assert!(slot.output_audio[1][0].abs() < f32::EPSILON);
        assert!(slot.output_audio[1][1].abs() < f32::EPSILON);
        assert!((slot.output_audio[1][2] + 1.0).abs() < f32::EPSILON);
        assert_eq!(slot.metadata.state(), Ok(SlotState::Processing));
    }

    #[test]
    fn bounded_no_op_copy_rejects_corrupt_counts() {
        let mut slot = BlockSlot::new();
        slot.metadata.frame_count = u32::try_from(MAX_FRAMES).unwrap() + 1;
        assert_eq!(
            copy_bounded_audio(&mut slot),
            Err(ProtocolError::InvalidRequest)
        );
    }

    #[test]
    fn heartbeat_cadence_uses_saturating_exact_interval_comparison() {
        assert!(heartbeat_due(0, 1, 10));
        assert!(!heartbeat_due(100, 109, 10));
        assert!(heartbeat_due(100, 110, 10));
        assert!(!heartbeat_due(u64::MAX, 1, 10));
    }

    #[test]
    fn normal_timed_processing_copies_audio_and_publishes_ordered_timing() {
        let clock = MonotonicClock::new().unwrap();
        let header = ProtocolHeader::new(1, 1);
        let mut heartbeat = HeartbeatPublisher::new(clock);
        let mut slot = timed_requested_slot(clock, 1);
        slot.input_audio[0][0] = 0.75;
        let configuration = StandaloneWorkerConfiguration {
            bank_name: BANK_NAME.to_owned(),
            worker_id: 7,
            bundle: None,
        };

        assert!(
            process_available_request(
                &mut slot,
                &header,
                &configuration,
                clock,
                &mut heartbeat,
                None,
            )
            .unwrap()
        );

        let snapshot = slot.completion_snapshot().unwrap().unwrap();
        assert!(snapshot.timing.is_valid());
        assert!(snapshot.timing.request_published_tick > 0);
        assert!((slot.output_audio[0][0] - 0.75).abs() < f32::EPSILON);
        slot.consume_completion(snapshot.ticket).unwrap();
    }

    fn timed_requested_slot(clock: MonotonicClock, sequence: u64) -> BlockSlot {
        let mut slot = BlockSlot::new();
        slot.publish_request_at(
            BlockTicket {
                generation: 1,
                sequence,
            },
            BlockRequest {
                frame_count: 2,
                input_channel_count: 1,
                output_channel_count: 1,
                midi_event_count: 0,
                event_count: 0,
                flags: 0,
                sidechain_slots: 0,
            },
            clock.now_ticks(),
        )
        .unwrap();
        slot
    }
}
