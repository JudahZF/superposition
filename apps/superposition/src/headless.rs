//! Command-line host for a saved session and explicitly selected `CoreAudio` devices.

use std::path::PathBuf;

const USAGE: &str = "Usage:
  superposition --help
  superposition --list-devices
  superposition --list-midi-ports
  superposition --scan
  superposition --headless --session <package> --device <id-or-unique-name> --duration-seconds <N> [--frames 32|64|128|256] [--midi-port <id-or-unique-name>]
  superposition --headless --session <package> --input-device <id-or-unique-name|none> --output-device <id-or-unique-name> --duration-seconds <N> [--frames 32|64|128|256] [--midi-port <id-or-unique-name>]

The device ID printed by --list-devices is a CoreAudio object ID (coreaudio:<number>).
Use --input-device none for an output-only route. Both device choices must be explicit.
The MIDI port ID printed by --list-midi-ports selects one input; no MIDI input is opened by default.
The live host requires a saved session with at least one loaded plug-in rack.
The --scan command updates the plug-in catalog under SUPERPOSITION_APP_SUPPORT.";

#[derive(Debug, Eq, PartialEq)]
enum Command {
    Help,
    ListDevices,
    ListMidiPorts,
    Scan,
    Host(HostOptions),
}

#[derive(Debug, Eq, PartialEq)]
struct HostOptions {
    session: PathBuf,
    input_device: Option<String>,
    output_device: String,
    midi_port: Option<String>,
    duration_seconds: u64,
    frames: u32,
}

#[allow(
    clippy::too_many_lines,
    reason = "argument validation stays in one pass"
)]
fn parse(arguments: &[String]) -> Result<Command, String> {
    let mut headless = false;
    let mut help = false;
    let mut list_devices = false;
    let mut list_midi_ports = false;
    let mut scan = false;
    let mut session = None;
    let mut device = None;
    let mut input_device = None;
    let mut output_device = None;
    let mut midi_port = None;
    let mut duration_seconds = None;
    let mut frames = None;
    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].as_str();
        let target = match argument {
            "--headless" => {
                if headless {
                    return Err("--headless was specified more than once".to_owned());
                }
                headless = true;
                None
            }
            "--help" => {
                if help {
                    return Err("--help was specified more than once".to_owned());
                }
                help = true;
                None
            }
            "--list-devices" => {
                if list_devices {
                    return Err("--list-devices was specified more than once".to_owned());
                }
                list_devices = true;
                None
            }
            "--list-midi-ports" => {
                if list_midi_ports {
                    return Err("--list-midi-ports was specified more than once".to_owned());
                }
                list_midi_ports = true;
                None
            }
            "--scan" => {
                if scan {
                    return Err("--scan was specified more than once".to_owned());
                }
                scan = true;
                None
            }
            "--session" => Some(&mut session),
            "--device" => Some(&mut device),
            "--input-device" => Some(&mut input_device),
            "--output-device" => Some(&mut output_device),
            "--midi-port" => Some(&mut midi_port),
            "--duration-seconds" => Some(&mut duration_seconds),
            "--frames" => Some(&mut frames),
            _ => {
                return Err(format!(
                    "unknown argument {argument:?}; use --help for usage"
                ));
            }
        };
        if let Some(target) = target {
            if target.is_some() {
                return Err(format!("{argument} was specified more than once"));
            }
            index += 1;
            let value = arguments
                .get(index)
                .filter(|value| !value.is_empty() && !value.starts_with("--"))
                .ok_or_else(|| format!("{argument} requires a value"))?;
            *target = Some(value.clone());
        }
        index += 1;
    }

    let special_count =
        u8::from(help) + u8::from(list_devices) + u8::from(list_midi_ports) + u8::from(scan);
    if special_count > 1 {
        return Err(
            "--help, --list-devices, --list-midi-ports, and --scan cannot be combined".to_owned(),
        );
    }
    if special_count != 0 {
        if session.is_some()
            || device.is_some()
            || input_device.is_some()
            || output_device.is_some()
            || midi_port.is_some()
            || duration_seconds.is_some()
            || frames.is_some()
        {
            return Err("session and audio options require --headless".to_owned());
        }
        return Ok(if help {
            Command::Help
        } else if list_devices {
            Command::ListDevices
        } else if list_midi_ports {
            Command::ListMidiPorts
        } else {
            Command::Scan
        });
    }
    if !headless {
        return Err("live audio requires --headless; use --help for usage".to_owned());
    }
    let session = session.ok_or("--headless requires --session <package>")?;
    if device.is_some() && (input_device.is_some() || output_device.is_some()) {
        return Err(
            "--device cannot be combined with --input-device or --output-device".to_owned(),
        );
    }
    let (input_device, output_device) = if let Some(device) = device {
        (Some(device.clone()), device)
    } else {
        let input = input_device
            .ok_or("--headless requires --device or both --input-device and --output-device")?;
        let output = output_device
            .ok_or("--headless requires --device or both --input-device and --output-device")?;
        (if input == "none" { None } else { Some(input) }, output)
    };
    if output_device == "none" {
        return Err("--output-device must select a CoreAudio device".to_owned());
    }
    let duration_seconds = duration_seconds
        .ok_or("--headless requires --duration-seconds <N>")?
        .parse::<u64>()
        .map_err(|_| "--duration-seconds must be a positive whole number".to_owned())?;
    if duration_seconds == 0 {
        return Err("--duration-seconds must be greater than zero".to_owned());
    }
    let frames = frames.map_or(Ok(128), |value| {
        value
            .parse::<u32>()
            .map_err(|_| "--frames must be 32, 64, 128, or 256".to_owned())
    })?;
    if !matches!(frames, 32 | 64 | 128 | 256) {
        return Err("--frames must be 32, 64, 128, or 256".to_owned());
    }
    Ok(Command::Host(HostOptions {
        session: PathBuf::from(session),
        input_device,
        output_device,
        midi_port,
        duration_seconds,
        frames,
    }))
}

pub(crate) fn run(arguments: &[String]) -> Result<(), String> {
    match parse(arguments)? {
        Command::Help => {
            println!("{USAGE}");
            Ok(())
        }
        Command::ListDevices => list_devices(),
        Command::ListMidiPorts => list_midi_ports(),
        Command::Scan => scan(),
        Command::Host(options) => host(&options),
    }
}

fn scan() -> Result<(), String> {
    let mut product = crate::ProductRuntime::open(crate::default_application_support())?;
    let scanned = product.scan_standard_locations(false)?;
    println!(
        "scan: bundles={scanned} catalog_entries={}",
        product.catalog_count()
    );
    for diagnostic in product.diagnostics() {
        println!("diagnostic: {diagnostic}");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn list_devices() -> Result<(), String> {
    let devices = sp_audio_io_macos::enumerate_devices()
        .map_err(|error| format!("CoreAudio device discovery failed: {error}"))?;
    if devices.is_empty() {
        println!("No CoreAudio devices were found.");
    }
    for device in devices {
        let frames = device
            .supported_buffer_frames
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        println!(
            "id={} name={:?} input_channels={} output_channels={} frames=[{}]",
            device.info.id,
            device.info.name,
            device.capabilities.max_input_channels,
            device.capabilities.max_output_channels,
            frames
        );
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn list_devices() -> Result<(), String> {
    Err("CoreAudio device discovery requires macOS".to_owned())
}

#[cfg(target_os = "macos")]
fn list_midi_ports() -> Result<(), String> {
    let ports = sp_midi::MidirInput::enumerate_ports()
        .map_err(|error| format!("CoreMIDI input discovery failed: {error}"))?;
    if ports.is_empty() {
        println!("No MIDI input ports were found.");
    }
    for port in ports {
        println!("id={} name={:?}", port.id.as_str(), port.name);
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn list_midi_ports() -> Result<(), String> {
    Err("CoreMIDI input discovery requires macOS".to_owned())
}

#[cfg(target_os = "macos")]
#[allow(
    clippy::too_many_lines,
    reason = "live host lifetime and teardown stay together"
)]
fn host(options: &HostOptions) -> Result<(), String> {
    use sp_audio_io::{AudioEndpoint, AudioEndpointEvent, AudioFormat, AudioRouteConfig};
    use sp_audio_io_macos::MacOsAudioEndpoint;
    use sp_session::{AtomicFileSessionStore, CapturedPluginState, SessionController};
    use std::time::{Duration, Instant};

    // The controller permits a fresh empty document; the store check requires a saved package.
    AtomicFileSessionStore::new(&options.session)
        .load()
        .map_err(|error| {
            format!(
                "cannot load saved session {}: {error}",
                options.session.display()
            )
        })?;
    let controller = SessionController::open(&options.session).map_err(|error| {
        format!(
            "cannot open saved session {}: {error}",
            options.session.display()
        )
    })?;
    let model = &controller.document().model;
    if !model.racks.iter().any(|rack| !rack.slots.is_empty()) {
        return Err("saved session has no plug-in racks to host".to_owned());
    }
    let devices = sp_audio_io_macos::enumerate_devices()
        .map_err(|error| format!("CoreAudio device discovery failed: {error}"))?;
    let output = select_device_for_direction(
        &devices,
        &options.output_device,
        options.frames,
        DeviceDirection::Output,
    )?;
    let input = options
        .input_device
        .as_deref()
        .map(|requested| {
            select_device_for_direction(&devices, requested, options.frames, DeviceDirection::Input)
        })
        .transpose()?;
    let mut format = AudioFormat::product_stereo(options.frames)
        .map_err(|error| format!("invalid audio format: {error}"))?;
    format.channel_count = output.capabilities.max_output_channels.min(64);
    let route = AudioRouteConfig {
        input: input.map(|device| device.info.id.clone()),
        output: output.info.id.clone(),
        format,
    };

    let mut product = crate::ProductRuntime::open(crate::default_application_support())?;
    let mut loaded_racks = Vec::new();
    for (rack_index, rack) in model.racks.iter().enumerate() {
        if rack.slots.is_empty() {
            continue;
        }
        let states = rack
            .slots
            .iter()
            .map(|slot| {
                controller
                    .load_plugin_state(&slot.id.0)
                    .map(|saved| {
                        saved.map(|(component, controller, metadata)| CapturedPluginState {
                            instance_id: slot.id.0.clone(),
                            component,
                            controller,
                            metadata,
                        })
                    })
                    .map_err(|error| format!("cannot read state for slot {}: {error}", slot.id.0))
            })
            .collect::<Result<Vec<_>, _>>()?;
        product
            .load_rack(rack_index, rack, &states)
            .map_err(|error| format!("cannot load rack {}: {error}", rack_index + 1))?;
        if product.worker_running(rack_index) {
            loaded_racks.push(rack_index);
        }
    }
    if loaded_racks.is_empty() {
        return Err("no plug-in workers could be loaded from the saved session".to_owned());
    }

    let prepared = crate::engine::prepare_audio(&mut product, model)?;
    let mut renderer = prepared.renderer;
    if let Some(requested) = options.midi_port.as_deref() {
        let ports = sp_midi::MidirInput::enumerate_ports()
            .map_err(|error| format!("CoreMIDI input discovery failed: {error}"))?;
        let selected = select_midi_port(&ports, requested)?;
        let mut midi = sp_midi::MidirInput::new();
        midi.open_port(&selected.id)
            .map_err(|error| format!("MIDI input {requested:?} could not open: {error}"))?;
        println!("midi: id={} name={:?}", selected.id.as_str(), selected.name);
        renderer = renderer.with_midi_input(midi);
    }
    let renderer_telemetry = renderer.telemetry();
    let mut control = prepared.control;
    let _mapping_publisher = prepared.mapping_publisher;
    let _mappings = prepared.mappings;
    let mut endpoint = MacOsAudioEndpoint::with_renderer(renderer).allow_device_reconfiguration();
    endpoint
        .start_route(route)
        .map_err(|error| format!("CoreAudio could not start: {error}"))?;
    println!(
        "host: session={} input_device={} output_device={} input_name={:?} output_name={:?} input_channels={} output_channels={} sample_rate=48000 frames={} loaded_racks={} duration_seconds={}",
        options.session.display(),
        input.map_or("none", |device| device.info.id.as_str()),
        output.info.id,
        input.map(|device| device.info.name.as_str()),
        output.info.name,
        input.map_or(0, |device| device.capabilities.max_input_channels.min(64)),
        format.channel_count,
        options.frames,
        loaded_racks.len(),
        options.duration_seconds
    );

    let run_result = (|| {
        let duration = Duration::from_secs(options.duration_seconds);
        let start = Instant::now();
        let mut last_report = Instant::now();
        let mut last_latencies = vec![u32::MAX; model.racks.len()];
        let mut max_peak = [0.0_f32; 2];
        let mut max_rms = [0.0_f32; 2];
        let mut clipped = false;
        let mut rack_input_peaks = vec![[0.0_f32; 2]; model.racks.len()];
        let mut rack_input_rms = vec![[0.0_f32; 2]; model.racks.len()];
        let mut rack_input_clipped = vec![false; model.racks.len()];
        let mut rack_output_peaks = vec![[0.0_f32; 2]; model.racks.len()];
        let mut rack_output_rms = vec![[0.0_f32; 2]; model.racks.len()];
        let mut rack_output_clipped = vec![false; model.racks.len()];
        let mut device_error = None;
        'host: while start.elapsed() < duration {
            product.poll();
            let meter = renderer_telemetry.output_meter();
            for channel in 0..2 {
                max_peak[channel] = max_peak[channel].max(meter.peak[channel]);
                max_rms[channel] = max_rms[channel].max(meter.rms[channel]);
            }
            clipped |= meter.clipped;
            for &rack in &loaded_racks {
                if let Some(meter) = renderer_telemetry.rack_input_meter(rack) {
                    record_meter_max(
                        meter,
                        &mut rack_input_peaks[rack],
                        &mut rack_input_rms[rack],
                        &mut rack_input_clipped[rack],
                    );
                }
                if let Some(meter) = renderer_telemetry.rack_output_meter(rack) {
                    record_meter_max(
                        meter,
                        &mut rack_output_peaks[rack],
                        &mut rack_output_rms[rack],
                        &mut rack_output_clipped[rack],
                    );
                }
            }
            if let Some(event) = endpoint.poll_event() {
                device_error = Some(match event {
                    AudioEndpointEvent::DeviceLost { device } => {
                        format!("audio device {device} disconnected")
                    }
                    AudioEndpointEvent::DeviceConfigurationChanged { device } => {
                        format!("audio device {device} changed configuration")
                    }
                });
                break 'host;
            }
            for &rack in &loaded_racks {
                if let Some(latency) = product.rack_latency_samples(rack)
                    && last_latencies[rack] != latency
                    && control.set_rack_latency(rack, latency)
                {
                    last_latencies[rack] = latency;
                }
            }
            if last_report.elapsed() >= Duration::from_secs(1) {
                let callbacks = endpoint
                    .callback_telemetry()
                    .map_or(0, |data| data.callbacks);
                let completions = loaded_racks
                    .iter()
                    .filter_map(|&rack| renderer_telemetry.rack_diagnostics(rack))
                    .map(|data| data.completed)
                    .sum::<u64>();
                println!(
                    "progress: elapsed_seconds={} callbacks={callbacks} rack_completions={completions}",
                    start.elapsed().as_secs()
                );
                last_report = Instant::now();
            }
            std::thread::sleep(
                Duration::from_millis(25).min(duration.saturating_sub(start.elapsed())),
            );
        }
        let callback = endpoint
            .callback_telemetry()
            .ok_or("CoreAudio stopped before final telemetry was available")?;
        println!(
            "audio: callbacks={} rendered={} silenced={} invalid_frames={} invalid_buffers={} invalid_channels={} invalid_bytes={} frames_32={} frames_64={} frames_128={} frames_256={}",
            callback.callbacks,
            callback.rendered,
            callback.silenced,
            callback.invalid_frames,
            callback.invalid_buffers,
            callback.invalid_channels,
            callback.invalid_bytes,
            callback.frame_histogram_32,
            callback.frame_histogram_64,
            callback.frame_histogram_128,
            callback.frame_histogram_256
        );
        let meter = renderer_telemetry.output_meter();
        for channel in 0..2 {
            max_peak[channel] = max_peak[channel].max(meter.peak[channel]);
            max_rms[channel] = max_rms[channel].max(meter.rms[channel]);
        }
        clipped |= meter.clipped;
        for &rack in &loaded_racks {
            if let Some(meter) = renderer_telemetry.rack_input_meter(rack) {
                record_meter_max(
                    meter,
                    &mut rack_input_peaks[rack],
                    &mut rack_input_rms[rack],
                    &mut rack_input_clipped[rack],
                );
            }
            if let Some(meter) = renderer_telemetry.rack_output_meter(rack) {
                record_meter_max(
                    meter,
                    &mut rack_output_peaks[rack],
                    &mut rack_output_rms[rack],
                    &mut rack_output_clipped[rack],
                );
            }
        }
        println!(
            "output: max_peak_left={:.6} max_peak_right={:.6} max_rms_left={:.6} max_rms_right={:.6} clipped={clipped}",
            max_peak[0], max_peak[1], max_rms[0], max_rms[1]
        );
        let mut incomplete_racks = Vec::new();
        let mut degraded_racks = Vec::new();
        let mut unhealthy_racks = Vec::new();
        for &rack in &loaded_racks {
            println!(
                "rack_input: index={} max_peak_left={:.6} max_peak_right={:.6} max_rms_left={:.6} max_rms_right={:.6} clipped={}",
                rack + 1,
                rack_input_peaks[rack][0],
                rack_input_peaks[rack][1],
                rack_input_rms[rack][0],
                rack_input_rms[rack][1],
                rack_input_clipped[rack]
            );
            println!(
                "rack_output: index={} max_peak_left={:.6} max_peak_right={:.6} max_rms_left={:.6} max_rms_right={:.6} clipped={}",
                rack + 1,
                rack_output_peaks[rack][0],
                rack_output_peaks[rack][1],
                rack_output_rms[rack][0],
                rack_output_rms[rack][1],
                rack_output_clipped[rack]
            );
            if let Some(data) = renderer_telemetry.rack_diagnostics(rack) {
                println!(
                    "rack: index={} completed={} deadline_misses={} protocol_rejections={} fallback_activations={} gate_closed_blocks={}",
                    rack + 1,
                    data.completed,
                    data.deadline_misses,
                    data.protocol_rejections,
                    data.fallback_activations,
                    data.gate_closed_blocks
                );
                println!(
                    "rack_wake: index={} failures={} max_ticks={}",
                    rack + 1,
                    data.wake_failures,
                    data.max_wake_ticks
                );
                if data.deadline_misses > 0 {
                    println!(
                        "rack_deadline: index={} unclaimed={} in_progress={} completed_late={} last_block={} last_sequence={} request_tick={} claimed_tick={} observed_tick={}",
                        rack + 1,
                        data.missed_unclaimed,
                        data.missed_in_progress,
                        data.missed_completed_late,
                        data.last_miss_block_index,
                        data.last_miss_sequence,
                        data.last_miss_request_tick,
                        data.last_miss_claimed_tick,
                        data.last_miss_observed_tick
                    );
                    println!(
                        "rack_worker: index={} phase={} wait_sequence={} wake_sequence={} loop_tick={}",
                        rack + 1,
                        data.last_miss_worker_phase,
                        data.last_miss_worker_wait_sequence,
                        data.last_miss_wake_sequence,
                        data.last_miss_worker_loop_tick
                    );
                }
                if data.completed == 0 {
                    incomplete_racks.push(rack + 1);
                }
                if data.deadline_misses > 0
                    || data.protocol_rejections > 0
                    || data.wake_failures > 0
                {
                    degraded_racks.push(rack + 1);
                }
            } else {
                incomplete_racks.push(rack + 1);
            }
            if !product.worker_running(rack)
                || product.worker_recovering(rack)
                || product.worker_recovery_failed(rack)
            {
                unhealthy_racks.push(rack + 1);
            }
        }
        for diagnostic in product.diagnostics() {
            println!("diagnostic: {diagnostic}");
        }
        if let Some(error) = device_error {
            return Err(error);
        }
        if callback.callbacks == 0 || callback.rendered == 0 {
            return Err("CoreAudio did not render any callbacks".to_owned());
        }
        if callback.silenced > 0
            || callback.invalid_frames > 0
            || callback.invalid_buffers > 0
            || callback.invalid_channels > 0
            || callback.invalid_bytes > 0
        {
            return Err("CoreAudio reported silent or invalid callbacks".to_owned());
        }
        if !incomplete_racks.is_empty() {
            return Err(format!(
                "plug-in racks did not complete blocks: {incomplete_racks:?}"
            ));
        }
        if !degraded_racks.is_empty() {
            return Err(format!(
                "plug-in racks reported deadline misses, protocol rejections, or wake failures: {degraded_racks:?}"
            ));
        }
        if !unhealthy_racks.is_empty() {
            return Err(format!(
                "plug-in workers were closed or recovering at the end: {unhealthy_racks:?}"
            ));
        }
        if !callback.is_coherent() {
            return Err("CoreAudio callback telemetry is inconsistent".to_owned());
        }
        Ok(())
    })();
    let stop_result = endpoint
        .stop()
        .map_err(|error| format!("CoreAudio could not stop cleanly: {error}"));
    match (run_result, stop_result) {
        (Ok(()), Ok(())) => {
            println!("host: complete");
            Ok(())
        }
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(run_error), Err(stop_error)) => Err(format!("{run_error}; {stop_error}")),
    }
}

#[cfg(target_os = "macos")]
fn record_meter_max(
    meter: sp_engine::RackMeterSnapshot,
    peaks: &mut [f32; 2],
    rms: &mut [f32; 2],
    clipped: &mut bool,
) {
    for channel in 0..2 {
        peaks[channel] = peaks[channel].max(meter.peak[channel]);
        rms[channel] = rms[channel].max(meter.rms[channel]);
    }
    *clipped |= meter.clipped;
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy)]
enum DeviceDirection {
    Input,
    Output,
}

#[cfg(all(test, target_os = "macos"))]
fn select_device<'a>(
    devices: &'a [sp_audio_io_macos::MacOsAudioDevice],
    requested: &str,
    frames: u32,
) -> Result<&'a sp_audio_io_macos::MacOsAudioDevice, String> {
    let device = select_device_for_direction(devices, requested, frames, DeviceDirection::Input)?;
    select_device_for_direction(devices, requested, frames, DeviceDirection::Output)?;
    Ok(device)
}

#[cfg(target_os = "macos")]
fn select_device_for_direction<'a>(
    devices: &'a [sp_audio_io_macos::MacOsAudioDevice],
    requested: &str,
    frames: u32,
    direction: DeviceDirection,
) -> Result<&'a sp_audio_io_macos::MacOsAudioDevice, String> {
    let selected = if let Some(device) = devices
        .iter()
        .find(|device| device.info.id.as_str() == requested)
    {
        device
    } else {
        let matches = devices
            .iter()
            .filter(|device| device.info.name == requested)
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [selected] => *selected,
            [] => {
                return Err(format!(
                    "device {requested:?} was not found; use --list-devices"
                ));
            }
            _ => {
                return Err(format!(
                    "device name {requested:?} is ambiguous; use an exact device ID"
                ));
            }
        }
    };
    let (channels, label) = match direction {
        DeviceDirection::Input => (selected.capabilities.max_input_channels, "input"),
        DeviceDirection::Output => (selected.capabilities.max_output_channels, "output"),
    };
    if channels < 2 {
        return Err(format!(
            "device {requested:?} does not support stereo {label} audio"
        ));
    }
    if !selected.supported_buffer_frames.contains(&frames) {
        return Err(format!(
            "device {requested:?} does not support {frames}-frame audio at 48 kHz"
        ));
    }
    Ok(selected)
}

#[cfg(target_os = "macos")]
fn select_midi_port<'a>(
    ports: &'a [sp_midi::MidiPortInfo],
    requested: &str,
) -> Result<&'a sp_midi::MidiPortInfo, String> {
    if let Some(port) = ports.iter().find(|port| port.id.as_str() == requested) {
        return Ok(port);
    }
    let mut matches = ports.iter().filter(|port| port.name == requested);
    match (matches.next(), matches.next()) {
        (Some(port), None) => Ok(port),
        (None, _) => Err(format!(
            "MIDI input {requested:?} was not found; use --list-midi-ports"
        )),
        (Some(_), Some(_)) => Err(format!(
            "MIDI input name {requested:?} is ambiguous; use an exact port ID"
        )),
    }
}

#[cfg(not(target_os = "macos"))]
fn host(_options: &HostOptions) -> Result<(), String> {
    Err("live audio requires macOS".to_owned())
}

#[cfg(test)]
mod tests {
    use super::{Command, HostOptions, parse};
    use std::path::PathBuf;

    fn arguments(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn parses_explicit_host_configuration() {
        assert_eq!(
            parse(&arguments(&[
                "--headless",
                "--session",
                "Saved.superposition",
                "--device",
                "coreaudio:42",
                "--duration-seconds",
                "3",
                "--frames",
                "256",
            ])),
            Ok(Command::Host(HostOptions {
                session: PathBuf::from("Saved.superposition"),
                input_device: Some("coreaudio:42".to_owned()),
                output_device: "coreaudio:42".to_owned(),
                midi_port: None,
                duration_seconds: 3,
                frames: 256,
            }))
        );
    }

    #[test]
    fn parses_explicit_midi_selection_and_listing() {
        assert_eq!(
            parse(&arguments(&["--list-midi-ports"])),
            Ok(Command::ListMidiPorts)
        );
        assert_eq!(
            parse(&arguments(&[
                "--headless",
                "--session",
                "Saved.superposition",
                "--device",
                "coreaudio:42",
                "--duration-seconds",
                "3",
                "--midi-port",
                "midir:Test Source:0",
            ])),
            Ok(Command::Host(HostOptions {
                session: PathBuf::from("Saved.superposition"),
                input_device: Some("coreaudio:42".to_owned()),
                output_device: "coreaudio:42".to_owned(),
                midi_port: Some("midir:Test Source:0".to_owned()),
                duration_seconds: 3,
                frames: 128,
            }))
        );
    }

    #[test]
    fn parses_separate_devices_and_output_only() {
        for frames in ["32", "64", "128", "256"] {
            let mut values = vec![
                "--headless",
                "--session",
                "Saved.superposition",
                "--input-device",
                "coreaudio:11",
                "--output-device",
                "coreaudio:42",
                "--duration-seconds",
                "3",
                "--frames",
            ];
            values.push(frames);
            assert_eq!(
                parse(&arguments(&values)),
                Ok(Command::Host(HostOptions {
                    session: PathBuf::from("Saved.superposition"),
                    input_device: Some("coreaudio:11".to_owned()),
                    output_device: "coreaudio:42".to_owned(),
                    midi_port: None,
                    duration_seconds: 3,
                    frames: frames.parse().expect("supported frame count"),
                }))
            );
        }
        assert_eq!(
            parse(&arguments(&[
                "--headless",
                "--session",
                "saved",
                "--input-device",
                "none",
                "--output-device",
                "coreaudio:42",
                "--duration-seconds",
                "1",
            ])),
            Ok(Command::Host(HostOptions {
                session: PathBuf::from("saved"),
                input_device: None,
                output_device: "coreaudio:42".to_owned(),
                midi_port: None,
                duration_seconds: 1,
                frames: 128,
            }))
        );
    }

    #[test]
    #[allow(clippy::too_many_lines, reason = "table of invalid CLI combinations")]
    fn rejects_missing_or_invalid_live_options() {
        for values in [
            vec![
                "--headless",
                "--device",
                "coreaudio:42",
                "--duration-seconds",
                "1",
            ],
            vec![
                "--headless",
                "--session",
                "saved",
                "--duration-seconds",
                "1",
            ],
            vec!["--headless", "--session", "saved", "--device", "x"],
            vec![
                "--headless",
                "--session",
                "saved",
                "--device",
                "x",
                "--duration-seconds",
                "0",
            ],
            vec![
                "--headless",
                "--session",
                "saved",
                "--device",
                "x",
                "--duration-seconds",
                "1",
                "--frames",
                "16",
            ],
            vec![
                "--headless",
                "--session",
                "saved",
                "--device",
                "x",
                "--input-device",
                "x",
                "--duration-seconds",
                "1",
            ],
            vec![
                "--headless",
                "--session",
                "saved",
                "--device",
                "x",
                "--output-device",
                "x",
                "--duration-seconds",
                "1",
            ],
            vec![
                "--headless",
                "--session",
                "saved",
                "--input-device",
                "none",
                "--duration-seconds",
                "1",
            ],
            vec![
                "--headless",
                "--session",
                "saved",
                "--output-device",
                "x",
                "--duration-seconds",
                "1",
            ],
            vec![
                "--headless",
                "--session",
                "saved",
                "--input-device",
                "none",
                "--output-device",
                "none",
                "--duration-seconds",
                "1",
            ],
            vec!["--help", "--list-devices"],
            vec!["--help", "--list-midi-ports"],
            vec!["--list-devices", "--device", "x"],
            vec!["--list-midi-ports", "--midi-port", "x"],
            vec!["--headless", "--headless"],
            vec!["--midi-port", "x"],
            vec![
                "--headless",
                "--session",
                "saved",
                "--device",
                "x",
                "--duration-seconds",
                "1",
                "--midi-port",
            ],
            vec![
                "--headless",
                "--session",
                "saved",
                "--device",
                "x",
                "--duration-seconds",
                "1",
                "--midi-port",
                "one",
                "--midi-port",
                "two",
            ],
        ] {
            assert!(parse(&arguments(&values)).is_err(), "accepted {values:?}");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn validates_selected_device_direction_and_buffer_size() {
        use sp_audio_io::{AudioDeviceCapabilities, AudioDeviceId, AudioDeviceInfo};
        use sp_audio_io_macos::MacOsAudioDevice;

        let devices = [
            MacOsAudioDevice {
                info: AudioDeviceInfo {
                    id: AudioDeviceId::new("coreaudio:1"),
                    name: "Capture".to_owned(),
                    max_output_channels: 0,
                },
                capabilities: AudioDeviceCapabilities {
                    max_input_channels: 64,
                    max_output_channels: 0,
                    is_default_input: false,
                    is_default_output: false,
                },
                supported_buffer_frames: vec![32, 64],
            },
            MacOsAudioDevice {
                info: AudioDeviceInfo {
                    id: AudioDeviceId::new("coreaudio:2"),
                    name: "Playback".to_owned(),
                    max_output_channels: 64,
                },
                capabilities: AudioDeviceCapabilities {
                    max_input_channels: 0,
                    max_output_channels: 64,
                    is_default_input: false,
                    is_default_output: false,
                },
                supported_buffer_frames: vec![32, 64],
            },
        ];
        assert!(
            super::select_device_for_direction(
                &devices,
                "Capture",
                32,
                super::DeviceDirection::Input,
            )
            .is_ok()
        );
        assert!(
            super::select_device_for_direction(
                &devices,
                "Playback",
                64,
                super::DeviceDirection::Output,
            )
            .is_ok()
        );
        assert!(
            super::select_device_for_direction(
                &devices,
                "Capture",
                64,
                super::DeviceDirection::Output,
            )
            .is_err()
        );
        assert!(
            super::select_device_for_direction(
                &devices,
                "Playback",
                64,
                super::DeviceDirection::Input,
            )
            .is_err()
        );
        assert!(
            super::select_device_for_direction(
                &devices,
                "Playback",
                128,
                super::DeviceDirection::Output,
            )
            .is_err()
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn selects_only_an_exact_or_unique_midi_input() {
        use sp_midi::{MidiPortId, MidiPortInfo};

        let ports = [
            MidiPortInfo {
                id: MidiPortId::new("midir:Keyboard:0"),
                name: "Keyboard".to_owned(),
            },
            MidiPortInfo {
                id: MidiPortId::new("midir:Keyboard:1"),
                name: "Keyboard".to_owned(),
            },
            MidiPortInfo {
                id: MidiPortId::new("midir:Pad:0"),
                name: "Pad".to_owned(),
            },
        ];
        assert_eq!(
            super::select_midi_port(&ports, "midir:Keyboard:1")
                .expect("exact identity")
                .id,
            ports[1].id
        );
        assert_eq!(
            super::select_midi_port(&ports, "Pad")
                .expect("unique name")
                .id,
            ports[2].id
        );
        assert!(super::select_midi_port(&ports, "Keyboard").is_err());
        assert!(super::select_midi_port(&ports, "Unknown").is_err());
    }
}

#[cfg(all(test, target_os = "macos"))]
#[path = "live_editor_check.rs"]
mod live_editor_check;

#[cfg(all(test, target_os = "macos"))]
#[path = "live_scene_check.rs"]
mod live_scene_check;

#[cfg(all(test, target_os = "macos"))]
#[path = "live_maintenance_check.rs"]
mod live_maintenance_check;

#[cfg(all(test, target_os = "macos"))]
#[path = "live_save_check.rs"]
mod live_save_check;

#[cfg(all(test, target_os = "macos"))]
#[path = "live_topology_check.rs"]
mod live_topology_check;
