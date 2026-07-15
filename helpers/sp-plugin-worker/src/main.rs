//! Isolated plug-in-host helper entry point.

use std::{
    cmp, io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use sp_shared_memory::{
    BLOCK_SLOT_COUNT, BlockRequest, BlockSlot, BlockTicket, ProtocolError, ProtocolHeader,
    SlotState,
};
use sp_shared_memory_macos::{MonotonicClock, SharedMemoryRegion};
use sp_test_support::{
    FAULT_DELAY_MICROS_OPTION, FAULT_MODE_OPTION, FAULT_TRIGGER_SEQUENCE_OPTION,
    FaultConfiguration, FaultMode, WORK_DURATION_MICROS_OPTION, parse_fault_configuration,
};
use sp_vst3::Vst3BundlePath;

#[cfg(feature = "sdk")]
use sp_vst3::sdk::SdkPlugin;

const WORKER_ID_OPTION: &str = "--worker-id";
const BUNDLE_OPTION: &str = "--bundle";
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
    /// Optional VST3 bundle. When set (and `sdk` is enabled), blocks are processed
    /// through the plug-in instead of the feasibility pass-through copy.
    bundle: Option<Vst3BundlePath>,
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
    let configuration = match parse_startup_arguments(std::env::args().skip(1)) {
        Ok(configuration) => configuration,
        Err(error) => exit_with_error(&error),
    };
    let Some(configuration) = configuration else {
        println!(
            "usage: sp-plugin-worker --bank <shared-memory-bank> --worker-id <id> --bundle <path>"
        );
        return;
    };
    if let Err(error) = run_feasibility_worker(&configuration) {
        exit_with_error(&error);
    }
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
            FAULT_MODE_OPTION
            | FAULT_TRIGGER_SEQUENCE_OPTION
            | FAULT_DELAY_MICROS_OPTION
            | WORK_DURATION_MICROS_OPTION => {
                fault_arguments.push(option.clone());
                fault_arguments.push(value.clone());
            }
            _ => return Err(format!("unknown feasibility option `{option}`")),
        }
        index += 2;
    }

    let worker_id = worker_id.ok_or_else(|| format!("missing required `{WORKER_ID_OPTION}`"))?;
    let fault = parse_fault_configuration(fault_arguments).map_err(|error| error.to_string())?;
    Ok(FeasibilityWorkerConfiguration {
        bank_name,
        worker_id,
        timing_mode: TimingMode::Timed,
        fault,
        bundle,
    })
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
            thread::yield_now();
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
    writeln!(output, "ready").map_err(|error| format!("could not write readiness: {error}"))?;
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
    #[cfg(not(feature = "sdk"))] _plugin: Option<&mut ()>,
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
    if plan.mode == FaultMode::HangAfterClaim {
        hang_after_claim();
    }

    process_slot_audio(slot, plugin)?;
    sleep_with_heartbeat(plan.work_duration, header, heartbeat)?;
    sleep_with_heartbeat(plan.delay_before_completion, header, heartbeat)?;
    publish_completion(slot, configuration, ticket, plan.mode, clock)?;
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

fn hang_after_claim() -> ! {
    loop {
        thread::park();
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
        assert_eq!(output, b"ready\n");

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
