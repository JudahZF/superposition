//! Isolated plug-in-host helper entry point.

mod control_thread;
mod worker_runtime;

#[cfg(feature = "sdk")]
use std::ffi::c_void;
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
    control::{
        BankIdentity, ControlErrorCode, ControlErrorRecord, ControlOperation, ControlRequest,
        ControlResponse, ControlResponseStatus, ControlTarget, MAX_PENDING_CONTROL_REQUESTS,
        RackIdentity, SlotIdentity,
    },
    payload::{
        Bypass, ControlPayloadCodec, EditorGeometry, HealthReport, ParameterId, ParameterMetadata,
        ParameterWrite, PluginSlotConfiguration as WirePluginSlotConfiguration, RackTopology,
        RestartReport, SlotOrder, StateRestore,
    },
};
use sp_shared_memory::{
    BLOCK_SLOT_COUNT, BlockRequest, BlockSlot, BlockTicket, ProtocolError, ProtocolHeader,
    SlotState,
};
use sp_shared_memory_macos::{MonotonicClock, SharedMemoryRegion};
use sp_test_support::{
    COMPUTE_LOAD_MICROS_OPTION, COMPUTE_LOAD_MODE_OPTION, ComputeLoadConfiguration,
    ComputeLoadMode, FAULT_DELAY_MICROS_OPTION, FAULT_MODE_OPTION, FAULT_TRIGGER_SEQUENCE_OPTION,
    FaultConfiguration, FaultMode, SELF_CRASH_AFTER_CLAIM_MODE, WORK_DURATION_MICROS_OPTION,
    parse_compute_load_configuration, parse_fault_configuration,
};
use sp_vst3::{Vst3BundlePath, adapter::Vst3ClassSelection};
#[cfg(all(feature = "sdk", target_os = "macos"))]
use std::collections::BTreeMap;
#[cfg(feature = "sdk")]
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};

#[cfg(feature = "sdk")]
use crate::{
    control_thread::{
        ControlThread, ProcessingControlEndpoint, ProcessingControlHandler, control_mailbox,
    },
    worker_runtime::{
        EditorCommand, EditorSize, PlanarBlock, PluginFacade, PluginProcessRequest,
        PluginRuntimeError, PluginSlotConfiguration, PluginState, PluginTopology, RackProcessor,
    },
};

#[cfg(feature = "sdk")]
use sp_vst3::{
    adapter::{
        AdapterNotification, BoundedMidiEvents, BoundedOutputChanges, BoundedParameterChanges,
        MainBusLayout, MidiMessage, OutputChange, PlanarAudioInput, PlanarAudioOutput,
        PluginProcessBlock, ProcessingFormat, RackPluginAdapter, TimedMidiMessage,
        TimedParameterChange, Vst3StateStreams,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TimingMode {
    LegacyUntimed,
    Timed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FeasibilityWorkerConfiguration {
    bank_name: String,
    worker_id: u32,
    timing_mode: TimingMode,
    fault: FaultConfiguration,
    /// Abort immediately after claiming the exact configured fault request.
    self_crash_after_claim: bool,
    /// Bounded CPU work applied after audio processing and before completion publication.
    compute_load: ComputeLoadConfiguration,
    /// Optional VST3 bundle. When set (and `sdk` is enabled), blocks are processed
    /// through the plug-in instead of the feasibility pass-through copy.
    bundle: Option<Vst3BundlePath>,
}

/// Production worker configuration. Unlike Phase 1 feasibility mode, this is entered only by the
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
    Feasibility(FeasibilityWorkerConfiguration),
    Production(ProductionWorkerConfiguration),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RequestPlan {
    mode: FaultMode,
    delay_before_claim: Duration,
    delay_before_completion: Duration,
    work_duration: Duration,
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
        WorkerStartup::Feasibility(configuration) => run_feasibility_worker(&configuration),
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
    if arguments
        .first()
        .is_some_and(|argument| argument == "--worker")
    {
        return parse_production_worker_arguments(&arguments[1..])
            .map(WorkerStartup::Production)
            .map(Some);
    }
    parse_startup_arguments(arguments)
        .map(|configuration| configuration.map(WorkerStartup::Feasibility))
}

fn exit_with_error(error: &str) -> ! {
    eprintln!("sp-plugin-worker: {error}");
    std::process::exit(1);
}

fn parse_startup_arguments<I, S>(
    arguments: I,
) -> Result<Option<FeasibilityWorkerConfiguration>, String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut arguments = arguments.into_iter().map(Into::into);
    let Some(command) = arguments.next() else {
        return Ok(None);
    };
    if command != "--feasibility-bank" && command != "--bank" {
        return Err(format!("unknown argument `{command}`"));
    }
    let bank_name = arguments
        .next()
        .ok_or("missing shared-memory bank name after --feasibility-bank")?;
    let remaining: Vec<String> = arguments.collect();
    if command == "--bank" {
        return parse_explicit_feasibility_arguments(bank_name, &remaining).map(Some);
    }
    if remaining
        .first()
        .is_some_and(|value| !value.starts_with("--"))
    {
        return parse_legacy_feasibility_arguments(bank_name, &remaining).map(Some);
    }
    parse_explicit_feasibility_arguments(bank_name, &remaining).map(Some)
}

fn parse_legacy_feasibility_arguments(
    bank_name: String,
    arguments: &[String],
) -> Result<FeasibilityWorkerConfiguration, String> {
    let [worker_id] = arguments else {
        return Err("legacy feasibility mode requires exactly one worker ID".to_owned());
    };
    Ok(FeasibilityWorkerConfiguration {
        bank_name,
        worker_id: parse_worker_id(worker_id)?,
        timing_mode: TimingMode::LegacyUntimed,
        fault: FaultConfiguration::default(),
        self_crash_after_claim: false,
        compute_load: ComputeLoadConfiguration::default(),
        bundle: None,
    })
}

fn parse_explicit_feasibility_arguments(
    bank_name: String,
    arguments: &[String],
) -> Result<FeasibilityWorkerConfiguration, String> {
    let mut worker_id = None;
    let mut bundle = None;
    let mut fault_arguments = Vec::new();
    let mut compute_load_arguments = Vec::new();
    let mut fault_mode_seen = false;
    let mut self_crash_after_claim = false;
    let mut index = 0;
    while index < arguments.len() {
        let option = &arguments[index];
        let value = arguments
            .get(index + 1)
            .ok_or_else(|| format!("missing value after `{option}`"))?;
        match option.as_str() {
            WORKER_ID_OPTION => {
                if worker_id.is_some() {
                    return Err(format!("duplicate feasibility option `{WORKER_ID_OPTION}`"));
                }
                worker_id = Some(parse_worker_id(value)?);
            }
            BUNDLE_OPTION => {
                if bundle.is_some() {
                    return Err(format!("duplicate feasibility option `{BUNDLE_OPTION}`"));
                }
                bundle = Some(Vst3BundlePath::new(value.clone()));
            }
            FAULT_MODE_OPTION => {
                if fault_mode_seen {
                    return Err(format!(
                        "duplicate feasibility option `{FAULT_MODE_OPTION}`"
                    ));
                }
                fault_mode_seen = true;
                if value == SELF_CRASH_AFTER_CLAIM_MODE {
                    self_crash_after_claim = true;
                } else {
                    fault_arguments.push(option.clone());
                    fault_arguments.push(value.clone());
                }
            }
            FAULT_TRIGGER_SEQUENCE_OPTION
            | FAULT_DELAY_MICROS_OPTION
            | WORK_DURATION_MICROS_OPTION => {
                fault_arguments.push(option.clone());
                fault_arguments.push(value.clone());
            }
            COMPUTE_LOAD_MODE_OPTION | COMPUTE_LOAD_MICROS_OPTION => {
                compute_load_arguments.push(option.clone());
                compute_load_arguments.push(value.clone());
            }
            _ => return Err(format!("unknown feasibility option `{option}`")),
        }
        index += 2;
    }

    let worker_id = worker_id.ok_or_else(|| format!("missing required `{WORKER_ID_OPTION}`"))?;
    let fault = parse_fault_configuration(fault_arguments).map_err(|error| error.to_string())?;
    let compute_load = parse_compute_load_configuration(compute_load_arguments)
        .map_err(|error| error.to_string())?;
    Ok(FeasibilityWorkerConfiguration {
        bank_name,
        worker_id,
        timing_mode: TimingMode::Timed,
        fault,
        self_crash_after_claim,
        compute_load,
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

fn run_feasibility_worker(configuration: &FeasibilityWorkerConfiguration) -> Result<(), String> {
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
            // The feasibility data plane has no control-socket wakeup: yielding here hands the
            // no-op worker back to the general scheduler and was able to delay every concurrently
            // yielded rack past a 256-frame callback boundary. Stay on the dedicated processing
            // thread with a CPU relaxation hint instead. The resulting CPU/energy cost is recorded
            // by the attached harness rather than hidden by a sleep-paced synthetic run.
            std::hint::spin_loop();
        }
    }
    Ok(())
}

#[cfg(feature = "sdk")]
fn load_optional_plugin(
    configuration: &FeasibilityWorkerConfiguration,
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
    configuration: &FeasibilityWorkerConfiguration,
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
    configuration: &FeasibilityWorkerConfiguration,
    clock: MonotonicClock,
    heartbeat: &mut HeartbeatPublisher,
    #[cfg(feature = "sdk")] plugin: Option<&mut SdkPlugin>,
    #[cfg(not(feature = "sdk"))] plugin: Option<&mut ()>,
) -> Result<bool, String> {
    let Some(observed_ticket) = requested_ticket(slot)? else {
        return Ok(false);
    };
    let mut plan = request_plan(configuration.fault, observed_ticket.sequence);
    sleep_with_heartbeat(plan.delay_before_claim, header, heartbeat)?;

    let ticket = match configuration.timing_mode {
        TimingMode::LegacyUntimed => slot.claim_for_processing(configuration.worker_id),
        TimingMode::Timed => slot.claim_ticket_for_processing_at(
            configuration.worker_id,
            observed_ticket,
            clock.now_ticks(),
        ),
    };
    let ticket = match ticket {
        Ok(ticket) => ticket,
        Err(ProtocolError::UnexpectedState | ProtocolError::Owned | ProtocolError::StaleTicket) => {
            return Ok(false);
        }
        Err(error) => return Err(format!("invalid shared-memory request: {error}")),
    };
    plan = request_plan(configuration.fault, ticket.sequence);
    if configuration.self_crash_after_claim
        && ticket.sequence == configuration.fault.trigger_request_sequence
    {
        self_crash_after_claim();
    }
    if plan.mode == FaultMode::HangAfterClaim {
        hang_after_claim();
    }

    process_slot_audio(slot, plugin)?;
    run_calibrated_compute_load(configuration.compute_load, clock, header, heartbeat)?;
    sleep_with_heartbeat(plan.work_duration, header, heartbeat)?;
    sleep_with_heartbeat(plan.delay_before_completion, header, heartbeat)?;
    publish_completion(slot, configuration, ticket, plan.mode, clock)?;
    // A completed block is a useful liveness boundary even when its period is shorter than the
    // ordinary heartbeat interval. This gives the control monitor a progression sample before a
    // deliberately injected crash or hang on the following request.
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

fn request_plan(configuration: FaultConfiguration, request_sequence: u64) -> RequestPlan {
    let mode = configuration.selected_mode(request_sequence);
    RequestPlan {
        mode,
        delay_before_claim: if mode == FaultMode::DelayBeforeClaim {
            configuration.fault_delay
        } else {
            Duration::ZERO
        },
        delay_before_completion: if mode == FaultMode::LateCompletion {
            configuration.fault_delay
        } else {
            Duration::ZERO
        },
        work_duration: configuration.work_duration,
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

fn publish_completion(
    slot: &mut BlockSlot,
    configuration: &FeasibilityWorkerConfiguration,
    live_ticket: BlockTicket,
    fault_mode: FaultMode,
    clock: MonotonicClock,
) -> Result<(), String> {
    match configuration.timing_mode {
        TimingMode::LegacyUntimed => slot
            .publish_completion(configuration.worker_id, live_ticket)
            .map_err(|error| format!("could not publish completion: {error}")),
        TimingMode::Timed => {
            let completed_tick = clock.now_ticks();
            match fault_mode {
                FaultMode::MalformedCompletion | FaultMode::StaleCompletion => {
                    publish_injected_completion(
                        slot,
                        configuration.worker_id,
                        live_ticket,
                        fault_mode,
                        completed_tick,
                    )
                }
                FaultMode::None | FaultMode::DelayBeforeClaim | FaultMode::LateCompletion => slot
                    .publish_completion_at(configuration.worker_id, live_ticket, completed_tick)
                    .map_err(|error| format!("could not publish completion: {error}")),
                FaultMode::HangAfterClaim => unreachable!("hang mode never publishes completion"),
            }
        }
    }
}

#[cfg(any(test, feature = "feasibility-fault-injection"))]
fn publish_injected_completion(
    slot: &mut BlockSlot,
    worker_id: u32,
    live_ticket: BlockTicket,
    fault_mode: FaultMode,
    completed_tick: u64,
) -> Result<(), String> {
    let result = match fault_mode {
        FaultMode::MalformedCompletion => {
            slot.test_publish_malformed_completion_at(worker_id, live_ticket, completed_tick)
        }
        FaultMode::StaleCompletion => slot.test_publish_stale_completion_at(
            worker_id,
            live_ticket,
            mismatched_ticket(live_ticket),
            completed_tick,
        ),
        _ => {
            return Err(
                "requested an injected completion for a non-injection fault mode".to_owned(),
            );
        }
    };
    result.map_err(|error| format!("could not publish injected completion: {error}"))
}

#[cfg(not(any(test, feature = "feasibility-fault-injection")))]
fn publish_injected_completion(
    _slot: &mut BlockSlot,
    _worker_id: u32,
    _live_ticket: BlockTicket,
    fault_mode: FaultMode,
    _completed_tick: u64,
) -> Result<(), String> {
    Err(format!(
        "fault mode `{}` requires the non-default `feasibility-fault-injection` worker feature",
        fault_mode.as_str()
    ))
}

#[cfg(any(test, feature = "feasibility-fault-injection"))]
fn mismatched_ticket(ticket: BlockTicket) -> BlockTicket {
    BlockTicket {
        generation: ticket.generation,
        sequence: if ticket.sequence == u64::MAX {
            1
        } else {
            ticket.sequence + 1
        },
    }
}

/// Performs bounded CPU work against the shared monotonic clock.
///
/// This deliberately spins rather than sleeping so a Phase 1 run exercises the worker's actual
/// compute and scheduler contention path. The configured interval remains bounded and heartbeats
/// continue to advance while a long (but capped) load is active.
fn run_calibrated_compute_load(
    configuration: ComputeLoadConfiguration,
    clock: MonotonicClock,
    header: &ProtocolHeader,
    heartbeat: &mut HeartbeatPublisher,
) -> Result<(), String> {
    if configuration.mode == ComputeLoadMode::None {
        return Ok(());
    }

    let duration_ticks = clock.duration_to_ticks(configuration.duration).max(1);
    let started = clock.now_ticks();
    let deadline = started.saturating_add(duration_ticks);
    let heartbeat_ticks = clock.duration_to_ticks(HEARTBEAT_INTERVAL).max(1);
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    while clock.now_ticks() < deadline {
        let slice_deadline = clock
            .now_ticks()
            .saturating_add(heartbeat_ticks)
            .min(deadline);
        while clock.now_ticks() < slice_deadline {
            state = state
                .rotate_left(17)
                .wrapping_mul(0xbf58_476d_1ce4_e5b9)
                .wrapping_add(0x94d0_49bb_1331_11eb);
            std::hint::black_box(state);
        }
        heartbeat.publish_if_due(header)?;
    }
    let observed_ticks = clock.now_ticks().saturating_sub(started).max(1);
    header
        .record_worker_busy_ticks(duration_ticks, observed_ticks)
        .map_err(|error| format!("could not publish calibrated busy duration: {error}"))?;
    Ok(())
}

fn sleep_with_heartbeat(
    duration: Duration,
    header: &ProtocolHeader,
    heartbeat: &mut HeartbeatPublisher,
) -> Result<(), String> {
    let mut remaining = duration;
    while !remaining.is_zero() {
        let chunk = cmp::min(remaining, HEARTBEAT_INTERVAL);
        let started = Instant::now();
        thread::sleep(chunk);
        remaining = remaining.saturating_sub(started.elapsed());
        heartbeat.publish_if_due(header)?;
    }
    Ok(())
}

const fn heartbeat_due(last_tick: u64, now_tick: u64, interval_ticks: u64) -> bool {
    last_tick == 0 || now_tick.saturating_sub(last_tick) >= interval_ticks
}

fn self_crash_after_claim() -> ! {
    // Deliberate Phase 1 fault injection. `abort` avoids unwinding across any plug-in FFI.
    std::process::abort()
}

fn hang_after_claim() -> ! {
    loop {
        thread::park();
    }
}

const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(feature = "sdk")]
const EDITOR_POLL_INTERVAL: Duration = Duration::from_millis(8);

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

#[cfg(feature = "sdk")]
#[derive(Clone, Copy)]
struct EditorResizeRequest {
    slot: usize,
    size: EditorSize,
}

#[cfg(feature = "sdk")]
#[derive(Default)]
struct EditorHost {
    #[cfg(target_os = "macos")]
    windows: BTreeMap<usize, sp_vst3::editor_window::MacOsEditorWindow>,
}

#[allow(
    clippy::too_many_lines,
    clippy::needless_pass_by_value,
    reason = "one-shot startup sequence that owns its configuration for thread handoff"
)]
fn run_production_worker(configuration: ProductionWorkerConfiguration) -> Result<(), String> {
    #[cfg(feature = "sdk")]
    {
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
        let facade = Vst3Facade::load(&configuration.selection)?;
        let initial_plugin = WirePluginSlotConfiguration {
            slot: 0,
            input_channels: u8::try_from(facade.topology.input_channels)
                .map_err(|_| "plug-in input topology exceeds u8")?,
            output_channels: u8::try_from(facade.topology.output_channels)
                .map_err(|_| "plug-in output topology exceeds u8")?,
            event_input_active: facade.event_input_active,
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
        let (socket_endpoint, processing_endpoint) = control_mailbox();
        let control_thread =
            ControlThread::spawn(listener, target, socket_endpoint, CONTROL_TIMEOUT)
                .map_err(|error| error.to_string())?;
        // Product workers use the authenticated control shutdown request. Unlike the Phase 1
        // feasibility child, EOF on inherited stdin must not terminate a rack immediately when
        // the supervisor deliberately launches it with null stdin.
        let shutdown_requested = Arc::new(AtomicBool::new(false));
        runtime.shutdown_requested = Arc::clone(&shutdown_requested);
        let processing_shutdown = Arc::clone(&shutdown_requested);
        let (main_request_tx, main_request_rx) = sync_channel(1);
        let (main_response_tx, main_response_rx) = sync_channel(1);
        let (editor_resize_tx, editor_resize_rx) = sync_channel(8);
        #[cfg(not(target_os = "macos"))]
        let _ = &editor_resize_rx;
        let processing = thread::Builder::new()
            .name("sp-worker-processing".to_owned())
            .spawn(move || {
                let result = run_production_processing_thread(
                    region,
                    clock,
                    heartbeat,
                    runtime,
                    processing_endpoint,
                    main_request_tx,
                    main_response_rx,
                    editor_resize_tx,
                    &processing_shutdown,
                );
                processing_shutdown.store(true, Ordering::Release);
                result
            })
            .map_err(|error| format!("could not start worker processing thread: {error}"))?;

        let mut editor_host = EditorHost::default();
        while !shutdown_requested.load(Ordering::Acquire) {
            match main_request_rx.recv_timeout(EDITOR_POLL_INTERVAL) {
                Ok(mut task) => {
                    let response = task
                        .runtime
                        .handle_editor_control(task.request, &mut editor_host);
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
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            #[cfg(target_os = "macos")]
            {
                while let Ok(request) = editor_resize_rx.try_recv() {
                    if let Some(window) = editor_host.windows.get(&request.slot) {
                        window.resize(request.size.width, request.size.height);
                    }
                }
                sp_vst3::editor_window::pump_events();
            }
        }
        let control_result = control_thread.shutdown().map_err(|error| error.to_string());
        let processing_result = processing
            .join()
            .map_err(|_| "worker processing thread panicked".to_owned())?;
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
    clippy::needless_pass_by_value,
    reason = "the processing thread owns every channel endpoint it is handed"
)]
fn run_production_processing_thread(
    mut region: SharedMemoryRegion,
    clock: MonotonicClock,
    mut heartbeat: HeartbeatPublisher,
    runtime: Box<ProductionRuntime>,
    mut processing_endpoint: ProcessingControlEndpoint,
    main_request: SyncSender<MainThreadRequest>,
    main_response: Receiver<MainThreadResponse>,
    editor_resize: SyncSender<EditorResizeRequest>,
    shutdown_requested: &AtomicBool,
) -> Result<(), String> {
    let mut runtime = Some(runtime);
    let mut next_editor_poll = Instant::now();
    while !shutdown_requested.load(Ordering::Acquire) {
        drain_production_commands(
            &mut processing_endpoint,
            &mut runtime,
            &main_request,
            &main_response,
        )?;
        heartbeat.publish_if_due(&region.bank().header)?;
        if Instant::now() >= next_editor_poll
            && let Some(runtime) = runtime.as_mut()
        {
            for slot in 0..sp_shared_memory::MAX_PLUGINS_PER_RACK {
                if let Ok(Some(size)) = runtime.rack.take_editor_resize_request(slot) {
                    let _ = editor_resize.try_send(EditorResizeRequest { slot, size });
                }
            }
            next_editor_poll = Instant::now() + EDITOR_POLL_INTERVAL;
        }
        let mut processed_request = false;
        for slot_index in 0..BLOCK_SLOT_COUNT {
            let bank = region.bank_mut();
            let header = &bank.header;
            let Some(slot) = bank.slots.get_mut(slot_index) else {
                continue;
            };
            let runtime = runtime
                .as_mut()
                .expect("runtime is returned after editor command");
            if process_production_request(
                slot,
                header,
                runtime.worker_id,
                clock,
                &mut heartbeat,
                &mut runtime.rack,
            )? {
                processed_request = true;
            }
        }
        if !processed_request {
            std::hint::spin_loop();
        }
    }
    Ok(())
}

#[cfg(feature = "sdk")]
fn drain_production_commands(
    endpoint: &mut ProcessingControlEndpoint,
    runtime: &mut Option<Box<ProductionRuntime>>,
    main_request: &SyncSender<MainThreadRequest>,
    main_response: &Receiver<MainThreadResponse>,
) -> Result<(), String> {
    for _ in 0..MAX_PENDING_CONTROL_REQUESTS {
        let Some(request) = endpoint.try_receive() else {
            return Ok(());
        };
        let response = if is_main_thread_operation(request.operation()) {
            main_request
                .send(MainThreadRequest {
                    runtime: runtime.take().expect("runtime has one owner"),
                    request,
                })
                .map_err(|_| "worker main-thread editor owner stopped".to_owned())?;
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
    }
    Ok(())
}

#[cfg(feature = "sdk")]
const fn is_main_thread_operation(operation: ControlOperation) -> bool {
    let editor = matches!(
        operation,
        ControlOperation::OpenNativeEditor
            | ControlOperation::CloseNativeEditor
            | ControlOperation::FocusNativeEditor
            | ControlOperation::ResizeNativeEditor
    );
    editor
        || (cfg!(target_os = "macos")
            && matches!(
                operation,
                ControlOperation::LoadPlugin
                    | ControlOperation::UnloadSlot
                    | ControlOperation::RebuildRack
                    | ControlOperation::Shutdown
            ))
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
fn process_production_request(
    slot: &mut BlockSlot,
    header: &ProtocolHeader,
    worker_id: u32,
    clock: MonotonicClock,
    heartbeat: &mut HeartbeatPublisher,
    rack: &mut RackProcessor<Vst3Facade>,
) -> Result<bool, String> {
    let Some(observed_ticket) = requested_ticket(slot)? else {
        return Ok(false);
    };
    let ticket =
        match slot.claim_ticket_for_processing_at(worker_id, observed_ticket, clock.now_ticks()) {
            Ok(ticket) => ticket,
            Err(
                ProtocolError::UnexpectedState | ProtocolError::Owned | ProtocolError::StaleTicket,
            ) => {
                return Ok(false);
            }
            Err(error) => return Err(format!("invalid shared-memory request: {error}")),
        };
    rack.process_block(slot)
        .map_err(|error| format!("could not process VST3 rack: {error}"))?;
    slot.publish_completion_at(worker_id, ticket, clock.now_ticks())
        .map_err(|error| format!("could not publish completion: {error}"))?;
    heartbeat.publish_now(header)?;
    Ok(true)
}

#[cfg(feature = "sdk")]
struct Vst3Facade {
    adapter: HostSdkRackAdapter,
    topology: PluginTopology,
    event_input_active: bool,
    latency_samples: u32,
    output_changes: BoundedOutputChanges<512>,
}

#[cfg(feature = "sdk")]
impl Vst3Facade {
    fn load(selection: &Vst3ClassSelection) -> Result<Self, String> {
        let format = ProcessingFormat::new(48_000.0, 256).map_err(|error| error.to_string())?;
        let mut adapter = HostSdkRackAdapter::new();
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
        let layout = MainBusLayout::new(
            input_channels,
            output_channels,
            !buses.event_inputs.is_empty(),
        )
        .map_err(|error| error.to_string())?;
        buses
            .validate_fixed(layout)
            .map_err(|error| error.to_string())?;
        adapter
            .negotiate_main_buses(layout)
            .map_err(|error| error.to_string())?;
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
            event_input_active: layout.event_input_active,
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
                midi: &midi,
                parameter_changes: &parameters,
                output_changes: &mut self.output_changes,
            })
            .map_err(|error| PluginRuntimeError::Plugin(error.to_string()))?;
        self.latency_samples = self
            .adapter
            .latency_samples()
            .unwrap_or(self.latency_samples);
        Ok(())
    }

    fn set_active(&mut self, active: bool) -> Result<(), PluginRuntimeError> {
        if active {
            self.adapter.start_processing()
        } else {
            self.adapter.stop_processing()
        }
        .map_err(|error| PluginRuntimeError::Plugin(error.to_string()))
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
            .map_err(|error| PluginRuntimeError::Plugin(error.to_string()))
    }

    fn latency_samples(&self) -> u32 {
        self.latency_samples
    }

    fn take_restart_flags(&mut self) -> u32 {
        let mut output: BoundedOutputChanges<512> = BoundedOutputChanges::default();
        if self.adapter.drain_notifications(&mut output).is_err() {
            return 0;
        }
        output.iter().fold(0, |flags, change| match change {
            OutputChange::Notification(AdapterNotification::RestartRequested {
                flags: requested,
            }) => flags | requested,
            _ => flags,
        })
    }

    fn editor(&mut self, command: EditorCommand) -> Result<Option<EditorSize>, PluginRuntimeError> {
        let result = match command {
            EditorCommand::Open { parent_view } => self
                .adapter
                .open_editor(parent_view as *mut c_void)
                .map(|size| {
                    Some(EditorSize {
                        width: size.width,
                        height: size.height,
                    })
                }),
            EditorCommand::Focus => self.adapter.focus_editor(true).map(|()| None),
            EditorCommand::Resize { width, height } => self
                .adapter
                .resize_editor(sp_vst3::sdk::Vst3EditorSize { width, height })
                .map(|()| None),
            EditorCommand::Close => self.adapter.close_editor().map(|()| None),
        };
        result.map_err(|error| PluginRuntimeError::Plugin(error.to_string()))
    }

    fn take_editor_resize_request(&mut self) -> Result<Option<EditorSize>, PluginRuntimeError> {
        self.adapter
            .take_editor_resize_request()
            .map(|request| {
                request.map(|size| EditorSize {
                    width: size.width,
                    height: size.height,
                })
            })
            .map_err(|error| PluginRuntimeError::Plugin(error.to_string()))
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
    preloaded: [Option<PreloadedSlot>; 8],
    rack_committed: bool,
    shutdown_requested: Arc<AtomicBool>,
}

#[cfg(feature = "sdk")]
struct PreloadedSlot {
    configuration: WirePluginSlotConfiguration,
    facade: Vst3Facade,
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
        let slot = usize::from(configuration.slot);
        rack.replace_slot(
            slot,
            facade,
            PluginSlotConfiguration {
                topology,
                bypassed: false,
                active: false,
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
            preloaded: std::array::from_fn(|_| None),
            rack_committed: false,
            shutdown_requested: Arc::new(AtomicBool::new(false)),
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
        Vst3Facade::load(&Vst3ClassSelection::new(
            Vst3BundlePath::new(&configuration.bundle_path),
            class_id,
        ))
    }

    fn preload_plugin(
        &mut self,
        request: &ControlRequest,
        configuration: WirePluginSlotConfiguration,
    ) -> Result<Vec<u8>, String> {
        let slot = Self::request_configuration_slot(request, &configuration)?;
        let facade = Self::load_facade(&configuration)?;
        self.preloaded[slot] = Some(PreloadedSlot {
            configuration,
            facade,
        });
        Ok(Vec::new())
    }

    fn load_preloaded(
        &mut self,
        request: &ControlRequest,
        configuration: WirePluginSlotConfiguration,
    ) -> Result<Vec<u8>, String> {
        let slot = Self::request_configuration_slot(request, &configuration)?;
        if self
            .rack
            .slot_configuration(slot)
            .is_some_and(|existing| existing.active)
        {
            return Err("plug-in load requires the replaced slot to be inactive".to_owned());
        }
        let preloaded = self.preloaded[slot]
            .take()
            .ok_or("requested slot has not been preloaded")?;
        if preloaded.configuration != configuration {
            self.preloaded[slot] = Some(preloaded);
            return Err("load configuration does not match the preloaded plug-in".to_owned());
        }
        let topology = preloaded.facade.topology;
        self.rack
            .replace_slot(
                slot,
                preloaded.facade,
                PluginSlotConfiguration {
                    topology,
                    bypassed: false,
                    active: false,
                },
            )
            .map_err(|error| error.to_string())?;
        self.loaded[slot] = Some(configuration);
        Ok(Vec::new())
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
            self.preloaded[slot] = None;

            let Some(configuration) = requested[slot].take() else {
                continue;
            };
            let facade = Self::load_facade(&configuration)?;
            let plugin_topology = facade.topology;
            self.rack
                .replace_slot(
                    slot,
                    facade,
                    PluginSlotConfiguration {
                        topology: plugin_topology,
                        bypassed: false,
                        active: false,
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

    fn apply_slot_order(&mut self, mut order: SlotOrder) -> Result<(), String> {
        for destination in 0..order.order.len() {
            let source = order
                .current
                .iter()
                .position(|slot| *slot == order.order[destination])
                .ok_or("reorder payload omitted an occupied slot")?;
            if source != destination {
                self.rack
                    .reorder(source, destination)
                    .map_err(|error| error.to_string())?;
                self.loaded.swap(source, destination);
                order.current.swap(source, destination);
            }
        }
        for (slot, configuration) in self.loaded.iter_mut().enumerate() {
            if let Some(configuration) = configuration {
                configuration.slot = u8::try_from(slot).expect("rack slot fits u8");
            }
        }
        Ok(())
    }

    fn handle_editor_control(
        &mut self,
        request: ControlRequest,
        host: &mut EditorHost,
    ) -> ControlResponse {
        #[cfg(not(target_os = "macos"))]
        return Self::unsupported(&request, "native editors require macOS");

        #[cfg(target_os = "macos")]
        if matches!(
            request.operation(),
            ControlOperation::LoadPlugin
                | ControlOperation::UnloadSlot
                | ControlOperation::RebuildRack
                | ControlOperation::Shutdown
        ) {
            let slots = match request.operation() {
                ControlOperation::LoadPlugin | ControlOperation::UnloadSlot => {
                    Self::slot_index(&request).map_or_else(|_| Vec::new(), |slot| vec![slot])
                }
                _ => host.windows.keys().copied().collect(),
            };
            for slot in slots {
                let _ = self.rack.editor(slot, EditorCommand::Close);
                if let Some(window) = host.windows.remove(&slot) {
                    window.close();
                }
            }
            return self.handle_control(request);
        }

        #[cfg(target_os = "macos")]
        let result = match request.operation() {
            ControlOperation::OpenNativeEditor => Self::slot_index(&request).and_then(|slot| {
                if let Some(window) = host.windows.get(&slot) {
                    window.focus();
                    return Ok(Vec::new());
                }
                let window =
                    sp_vst3::editor_window::MacOsEditorWindow::new("Superposition Plug-in Editor")?;
                let parent_view = window.content_view()?;
                let size = self
                    .rack
                    .editor(slot, EditorCommand::Open { parent_view })
                    .map_err(|error| error.to_string())?;
                if let Some(size) = size {
                    window.resize(size.width, size.height);
                }
                window.focus();
                host.windows.insert(slot, window);
                Ok(Vec::new())
            }),
            ControlOperation::CloseNativeEditor => Self::slot_index(&request).and_then(|slot| {
                self.close_editor(host, slot)?;
                Ok(Vec::new())
            }),
            ControlOperation::FocusNativeEditor => Self::slot_index(&request).and_then(|slot| {
                self.rack
                    .editor(slot, EditorCommand::Focus)
                    .map_err(|error| error.to_string())?;
                host.windows
                    .get(&slot)
                    .ok_or("native editor window is not open")?
                    .focus();
                Ok(Vec::new())
            }),
            ControlOperation::ResizeNativeEditor => Self::slot_index(&request).and_then(|slot| {
                let geometry =
                    EditorGeometry::decode(request.payload()).map_err(|error| error.to_string())?;
                self.rack
                    .editor(
                        slot,
                        EditorCommand::Resize {
                            width: geometry.width,
                            height: geometry.height,
                        },
                    )
                    .map_err(|error| error.to_string())?;
                host.windows
                    .get(&slot)
                    .ok_or("native editor window is not open")?
                    .resize(geometry.width, geometry.height);
                Ok(Vec::new())
            }),
            _ => Err("request is not a native-editor operation".to_owned()),
        };
        Self::response(&request, result)
    }

    #[cfg(target_os = "macos")]
    fn close_editor(&mut self, host: &mut EditorHost, slot: usize) -> Result<(), String> {
        self.rack
            .editor(slot, EditorCommand::Close)
            .map_err(|error| error.to_string())?;
        let window = host
            .windows
            .remove(&slot)
            .ok_or("native editor window is not open")?;
        window.close();
        Ok(())
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
        let result = match request.operation() {
            ControlOperation::PreloadPlugin => {
                WirePluginSlotConfiguration::decode(request.payload())
                    .map_err(|error| error.to_string())
                    .and_then(|configuration| self.preload_plugin(&request, configuration))
            }
            ControlOperation::LoadPlugin => WirePluginSlotConfiguration::decode(request.payload())
                .map_err(|error| error.to_string())
                .and_then(|configuration| self.load_preloaded(&request, configuration)),
            ControlOperation::RebuildRack => RackTopology::decode(request.payload())
                .map_err(|error| error.to_string())
                .and_then(|topology| self.rebuild_rack(topology)),
            ControlOperation::ActivateSlot => Self::slot_index(&request)
                .and_then(|slot| {
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
            ControlOperation::UnloadSlot => Self::slot_index(&request)
                .and_then(|slot| {
                    if self
                        .rack
                        .slot_configuration(slot)
                        .is_some_and(|configuration| configuration.active)
                    {
                        return Err("plug-in unload requires an inactive slot".to_owned());
                    }
                    let removed = self
                        .rack
                        .remove_slot(slot)
                        .map_err(|error| error.to_string())?;
                    self.loaded[slot] = None;
                    Ok(removed)
                })
                .map(|_| Vec::new()),
            ControlOperation::ReorderRack => SlotOrder::decode(request.payload())
                .map_err(|error| error.to_string())
                .and_then(|order| self.apply_slot_order(order).map(|()| Vec::new())),
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
            ControlOperation::OpenNativeEditor
            | ControlOperation::CloseNativeEditor
            | ControlOperation::FocusNativeEditor
            | ControlOperation::ResizeNativeEditor => {
                return Self::unsupported(
                    &request,
                    "native editor operation missed main-thread routing",
                );
            }
            ControlOperation::NotifyLatency => Ok(u64::from(self.rack.latency_samples())
                .to_le_bytes()
                .to_vec()),
            ControlOperation::NotifyRestart => RestartReport {
                requested: self.rack.take_restart_flags() != 0,
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
                restart_requested: self.rack.take_restart_flags() != 0,
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
        assert_eq!(parse_startup_arguments(Vec::<String>::new()).unwrap(), None);
    }

    #[cfg(feature = "sdk")]
    #[test]
    fn editor_lifecycle_is_never_dispatched_on_the_processing_thread() {
        assert!(is_main_thread_operation(ControlOperation::OpenNativeEditor));
        assert!(is_main_thread_operation(
            ControlOperation::ResizeNativeEditor
        ));
        assert!(!is_main_thread_operation(ControlOperation::WriteParameter));
        assert_eq!(
            is_main_thread_operation(ControlOperation::Shutdown),
            cfg!(target_os = "macos")
        );
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

    #[test]
    fn product_arguments_require_a_bundle_and_use_timed_processing() {
        let configuration = parse_startup_arguments([
            "--bank",
            BANK_NAME,
            WORKER_ID_OPTION,
            "7",
            BUNDLE_OPTION,
            "Example.vst3",
        ])
        .expect("product arguments parse")
        .expect("configuration");

        assert_eq!(configuration.timing_mode, TimingMode::Timed);
        assert_eq!(
            configuration.bundle,
            Some(Vst3BundlePath::new("Example.vst3"))
        );
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
    fn legacy_feasibility_arguments_remain_untimed_and_fault_free() {
        let configuration = parse_startup_arguments(["--feasibility-bank", BANK_NAME, "7"])
            .unwrap()
            .unwrap();
        assert_eq!(configuration.bank_name, BANK_NAME);
        assert_eq!(configuration.worker_id, 7);
        assert_eq!(configuration.timing_mode, TimingMode::LegacyUntimed);
        assert_eq!(configuration.fault, FaultConfiguration::default());
    }

    #[test]
    fn explicit_feasibility_arguments_parse_timing_and_fault_options() {
        let configuration = parse_startup_arguments([
            "--feasibility-bank",
            BANK_NAME,
            WORKER_ID_OPTION,
            "9",
            FAULT_MODE_OPTION,
            "late-completion",
            FAULT_TRIGGER_SEQUENCE_OPTION,
            "12",
            FAULT_DELAY_MICROS_OPTION,
            "400",
            WORK_DURATION_MICROS_OPTION,
            "100",
            COMPUTE_LOAD_MODE_OPTION,
            "calibrated-cpu",
            COMPUTE_LOAD_MICROS_OPTION,
            "250",
        ])
        .unwrap()
        .unwrap();

        assert_eq!(configuration.worker_id, 9);
        assert_eq!(configuration.timing_mode, TimingMode::Timed);
        assert_eq!(configuration.fault.mode, FaultMode::LateCompletion);
        assert_eq!(configuration.fault.trigger_request_sequence, 12);
        assert_eq!(configuration.fault.fault_delay, Duration::from_micros(400));
        assert_eq!(
            configuration.fault.work_duration,
            Duration::from_micros(100)
        );
        assert_eq!(
            configuration.compute_load.mode,
            ComputeLoadMode::CalibratedCpu
        );
        assert_eq!(
            configuration.compute_load.duration,
            Duration::from_micros(250)
        );
    }

    #[test]
    fn worker_parser_accepts_exact_self_crash_fault_trigger() {
        let configuration = parse_startup_arguments([
            "--feasibility-bank",
            BANK_NAME,
            WORKER_ID_OPTION,
            "9",
            FAULT_MODE_OPTION,
            SELF_CRASH_AFTER_CLAIM_MODE,
            FAULT_TRIGGER_SEQUENCE_OPTION,
            "12",
        ])
        .unwrap()
        .unwrap();

        assert!(configuration.self_crash_after_claim);
        assert_eq!(configuration.fault.mode, FaultMode::None);
        assert_eq!(configuration.fault.trigger_request_sequence, 12);
        assert_eq!(configuration.fault.selected_mode(12), FaultMode::None);
    }

    #[test]
    fn feasibility_parser_rejects_missing_duplicate_and_reserved_worker_ids() {
        assert!(parse_startup_arguments(["--feasibility-bank", BANK_NAME]).is_err());
        assert!(
            parse_startup_arguments([
                "--feasibility-bank",
                BANK_NAME,
                WORKER_ID_OPTION,
                "1",
                WORKER_ID_OPTION,
                "2",
            ])
            .is_err()
        );
        for worker_id in ["0", "4294967293", "4294967294", "4294967295"] {
            assert!(
                parse_startup_arguments([
                    "--feasibility-bank",
                    BANK_NAME,
                    WORKER_ID_OPTION,
                    worker_id,
                ])
                .is_err()
            );
        }
    }

    #[test]
    fn request_plan_selects_each_fault_only_at_the_exact_trigger() {
        for mode in [
            FaultMode::None,
            FaultMode::HangAfterClaim,
            FaultMode::DelayBeforeClaim,
            FaultMode::LateCompletion,
            FaultMode::MalformedCompletion,
            FaultMode::StaleCompletion,
        ] {
            let configuration = FaultConfiguration {
                mode,
                trigger_request_sequence: 5,
                fault_delay: if matches!(
                    mode,
                    FaultMode::DelayBeforeClaim | FaultMode::LateCompletion
                ) {
                    Duration::from_micros(10)
                } else {
                    Duration::ZERO
                },
                work_duration: Duration::from_micros(3),
            };
            assert_eq!(request_plan(configuration, 4).mode, FaultMode::None);
            assert_eq!(request_plan(configuration, 5).mode, mode);
            assert_eq!(request_plan(configuration, 6).mode, FaultMode::None);
        }
    }

    #[test]
    fn request_plan_places_delays_at_the_named_transition() {
        let before_claim = request_plan(
            FaultConfiguration {
                mode: FaultMode::DelayBeforeClaim,
                trigger_request_sequence: 1,
                fault_delay: Duration::from_micros(20),
                work_duration: Duration::from_micros(3),
            },
            1,
        );
        assert_eq!(before_claim.delay_before_claim, Duration::from_micros(20));
        assert_eq!(before_claim.delay_before_completion, Duration::ZERO);

        let late = request_plan(
            FaultConfiguration {
                mode: FaultMode::LateCompletion,
                trigger_request_sequence: 1,
                fault_delay: Duration::from_micros(20),
                work_duration: Duration::from_micros(3),
            },
            1,
        );
        assert_eq!(late.delay_before_claim, Duration::ZERO);
        assert_eq!(late.delay_before_completion, Duration::from_micros(20));
        assert_eq!(late.work_duration, Duration::from_micros(3));
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
        let configuration = timed_configuration(FaultMode::None, 1);

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

    #[test]
    fn malformed_fault_uses_the_deliberate_protocol_hook() {
        let clock = MonotonicClock::new().unwrap();
        let header = ProtocolHeader::new(1, 1);
        let mut heartbeat = HeartbeatPublisher::new(clock);
        let mut slot = timed_requested_slot(clock, 2);
        let configuration = timed_configuration(FaultMode::MalformedCompletion, 2);
        let ticket = BlockTicket {
            generation: 1,
            sequence: 2,
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
        assert_eq!(
            slot.consume_completion(ticket),
            Err(ProtocolError::MalformedCompletion)
        );
        assert_eq!(slot.metadata.state(), Ok(SlotState::Complete));
    }

    #[test]
    fn stale_fault_uses_a_valid_mismatched_completion_ticket() {
        let clock = MonotonicClock::new().unwrap();
        let header = ProtocolHeader::new(1, 1);
        let mut heartbeat = HeartbeatPublisher::new(clock);
        let mut slot = timed_requested_slot(clock, 3);
        let configuration = timed_configuration(FaultMode::StaleCompletion, 3);
        let live_ticket = BlockTicket {
            generation: 1,
            sequence: 3,
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
        assert!(snapshot.ticket.is_valid());
        assert_ne!(snapshot.ticket, live_ticket);
        assert_eq!(
            slot.consume_completion(live_ticket),
            Err(ProtocolError::StaleCompletion)
        );
        assert_eq!(slot.metadata.state(), Ok(SlotState::Complete));
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
            },
            clock.now_ticks(),
        )
        .unwrap();
        slot
    }

    fn timed_configuration(mode: FaultMode, sequence: u64) -> FeasibilityWorkerConfiguration {
        FeasibilityWorkerConfiguration {
            bank_name: BANK_NAME.to_owned(),
            worker_id: 7,
            timing_mode: TimingMode::Timed,
            fault: FaultConfiguration {
                mode,
                trigger_request_sequence: sequence,
                fault_delay: Duration::ZERO,
                work_duration: Duration::ZERO,
            },
            self_crash_after_claim: false,
            compute_load: ComputeLoadConfiguration::default(),
            bundle: None,
        }
    }

    #[test]
    fn mismatched_ticket_stays_valid_at_sequence_wrap() {
        let ticket = BlockTicket {
            generation: 8,
            sequence: u64::MAX,
        };
        let mismatched = mismatched_ticket(ticket);
        assert!(mismatched.is_valid());
        assert_ne!(mismatched, ticket);
        assert_eq!(mismatched.sequence, 1);
    }
}
