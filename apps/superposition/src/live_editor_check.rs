//! Explicitly opted-in live check of an approved plug-in editor during `BlackHole` audio.

use std::{
    env,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use sp_audio_io::{AudioEndpoint, AudioEndpointEvent, AudioFormat, AudioRouteConfig};
use sp_audio_io_macos::{MacOsAudioEndpoint, enumerate_devices};
use sp_session::{AtomicFileSessionStore, CapturedPluginState, SessionController};

use crate::{ProductRuntime, engine};

const VALHALLA_CLASS_ID: &str = "565354734D617376616C68616C6C6173";
const PRO_Q_4_CLASS_ID: &str = "ED57BD725C60467EA64DD2F400758B6F";
const NOLLY_X_CLASS_ID: &str = "ABCDEF019182FAEB4E4453504E414E58";

#[derive(Clone, Copy, Default)]
struct ObservedMeter {
    peak: [f32; 2],
    rms: [f32; 2],
    clipped: bool,
}

#[derive(Debug)]
struct EditorTransition {
    name: &'static str,
    open: bool,
    before_completed: Option<u64>,
    after_completed: Option<u64>,
    elapsed: Duration,
    result: Result<(), String>,
}

impl ObservedMeter {
    fn record(&mut self, meter: sp_engine::RackMeterSnapshot) {
        super::record_meter_max(meter, &mut self.peak, &mut self.rms, &mut self.clipped);
    }
}

fn required_env(name: &str) -> String {
    env::var(name).unwrap_or_else(|_| panic!("{name} must be set for the live editor check"))
}

#[test]
#[ignore = "opens an approved plug-in editor and starts BlackHole audio; explicit opt-in required"]
#[allow(
    clippy::too_many_lines,
    reason = "the live endpoint and editor must be stopped and closed before any assertion"
)]
fn plugin_editor_remains_open_during_blackhole_audio() {
    assert_eq!(
        required_env("SUPERPOSITION_LIVE_EDITOR_CHECK"),
        "1",
        "explicit SUPERPOSITION_LIVE_EDITOR_CHECK=1 is required"
    );
    let session_path = PathBuf::from(required_env("SUPERPOSITION_LIVE_EDITOR_SESSION"));
    let app_support = PathBuf::from(required_env("SUPERPOSITION_LIVE_EDITOR_APP_SUPPORT"));
    let helpers = PathBuf::from(required_env("SUPERPOSITION_HELPERS_DIR"));
    assert!(
        helpers.is_absolute() && helpers.is_dir(),
        "helper directory is invalid"
    );
    let requested_device = required_env("SUPERPOSITION_LIVE_EDITOR_DEVICE");
    let frames = required_env("SUPERPOSITION_LIVE_EDITOR_FRAMES")
        .parse::<u32>()
        .expect("SUPERPOSITION_LIVE_EDITOR_FRAMES must be an integer");
    let editor_slot = env::var("SUPERPOSITION_LIVE_EDITOR_SLOT")
        .unwrap_or_else(|_| "1".to_owned())
        .parse::<usize>()
        .ok()
        .and_then(|slot| slot.checked_sub(1))
        .expect("SUPERPOSITION_LIVE_EDITOR_SLOT must be a positive slot number");
    let duration_seconds = env::var("SUPERPOSITION_LIVE_EDITOR_SECONDS")
        .unwrap_or_else(|_| "30".to_owned())
        .parse::<u64>()
        .expect("SUPERPOSITION_LIVE_EDITOR_SECONDS must be an integer");
    assert!(
        (8..=60).contains(&duration_seconds),
        "SUPERPOSITION_LIVE_EDITOR_SECONDS must be in 8..=60"
    );
    let duration = Duration::from_secs(duration_seconds);
    let mut format = AudioFormat::product_stereo(frames)
        .expect("SUPERPOSITION_LIVE_EDITOR_FRAMES must be 32, 64, 128, or 256");

    AtomicFileSessionStore::new(&session_path)
        .load()
        .expect("saved session package exists");
    let controller = SessionController::open(&session_path).expect("open saved session");
    let model = &controller.document().model;
    assert!(
        model
            .racks
            .first()
            .is_some_and(|rack| editor_slot < rack.slots.len()),
        "rack 1 must contain the editor target"
    );
    let plugin_name = &model.racks[0].slots[editor_slot].plugin.identity.name;
    for slot in model.racks.iter().flat_map(|rack| &rack.slots) {
        let identity = &slot.plugin.identity;
        assert!(
            matches!(
                (
                    identity.vendor.as_str(),
                    identity.name.as_str(),
                    identity.unique_id.as_str()
                ),
                (
                    "Valhalla DSP, LLC",
                    "ValhallaSupermassive",
                    VALHALLA_CLASS_ID
                ) | ("FabFilter", "Pro-Q 4", PRO_Q_4_CLASS_ID)
                    | ("Neural DSP", "Archetype Nolly X", NOLLY_X_CLASS_ID)
            ),
            "every fixture slot must use an approved plug-in"
        );
    }
    for rack in &model.racks {
        assert_eq!(
            model.rack_routes.get(&rack.id),
            Some(&sp_model::RackChannelRoute {
                input: Some(sp_model::PhysicalChannels::Stereo { left: 0, right: 1 }),
                output: sp_model::PhysicalChannels::Stereo { left: 2, right: 3 },
            }),
            "editor fixtures must use BlackHole 1–2 → 3–4 without loopback feedback"
        );
    }

    let devices = enumerate_devices().expect("enumerate CoreAudio devices");
    let selected = super::select_device(&devices, &requested_device, frames)
        .expect("select exact-frame duplex device");
    assert_eq!(selected.info.name, "BlackHole 64ch");
    format.channel_count = selected.capabilities.max_output_channels.min(64);
    assert_eq!(
        format.channel_count, 64,
        "BlackHole must expose 64 output channels"
    );
    let route = AudioRouteConfig {
        input: Some(selected.info.id.clone()),
        output: selected.info.id.clone(),
        format,
    };

    let mut product = ProductRuntime::open(app_support).expect("open product runtime");
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
                    .map_err(|error| format!("cannot read state for {}: {error}", slot.id.0))
            })
            .collect::<Result<Vec<_>, _>>()
            .expect("read saved plug-in states");
        product
            .load_rack(rack_index, rack, &states)
            .expect("load approved plug-in rack");
        assert!(
            product.worker_running(rack_index),
            "rack worker must be running"
        );
        loaded_racks.push(rack_index);
    }
    assert!(loaded_racks.contains(&0), "rack 1 worker must be running");

    let prepared = engine::prepare_audio(&mut product, model).expect("prepare live audio");
    let telemetry = prepared.renderer.telemetry();
    let _control = prepared.control;
    let _mapping_publisher = prepared.mapping_publisher;
    let _mappings = prepared.mappings;
    let mut endpoint =
        MacOsAudioEndpoint::with_renderer(prepared.renderer).allow_device_reconfiguration();

    let start_result = endpoint.start_route(route);
    let mut run_error = start_result
        .as_ref()
        .err()
        .map(|error| format!("CoreAudio could not start: {error}"));
    let mut output_meter = ObservedMeter::default();
    let mut rack_input_meters = vec![ObservedMeter::default(); model.racks.len()];
    let mut rack_output_meters = vec![ObservedMeter::default(); model.racks.len()];
    let mut transitions = Vec::new();
    let editor_schedule = [
        ("open", true, duration.mul_f64(0.10)),
        ("close", false, duration.mul_f64(1.0 / 3.0)),
        ("reopen", true, duration.mul_f64(0.47)),
        ("close_again", false, duration.mul_f64(0.73)),
    ];
    let mut observe_meters = || {
        output_meter.record(telemetry.output_meter());
        for &rack in &loaded_racks {
            if let Some(meter) = telemetry.rack_input_meter(rack) {
                rack_input_meters[rack].record(meter);
            }
            if let Some(meter) = telemetry.rack_output_meter(rack) {
                rack_output_meters[rack].record(meter);
            }
        }
    };
    if start_result.is_ok() {
        let started = Instant::now();
        let mut next_transition = 0;
        while started.elapsed() < duration {
            product.poll();
            observe_meters();
            if let Some(event) = endpoint.poll_event() {
                run_error = Some(match event {
                    AudioEndpointEvent::DeviceLost { device } => {
                        format!("audio device {device} disconnected")
                    }
                    AudioEndpointEvent::DeviceConfigurationChanged { device } => {
                        format!("audio device {device} changed configuration")
                    }
                });
                break;
            }
            if let Some(&(name, open, when)) = editor_schedule.get(next_transition)
                && started.elapsed() >= when
            {
                let before_completed = telemetry.rack_diagnostics(0).map(|data| data.completed);
                let request_started = Instant::now();
                let result = product.set_native_editor_open(0, editor_slot, open, true);
                let elapsed = request_started.elapsed();
                let after_completed = telemetry.rack_diagnostics(0).map(|data| data.completed);
                println!(
                    "editor_transition: plugin={plugin_name:?} action={name} audio_elapsed={:?} request_elapsed={elapsed:?} before_completed={before_completed:?} after_completed={after_completed:?} result={result:?}",
                    started.elapsed()
                );
                let failed = result.is_err();
                transitions.push(EditorTransition {
                    name,
                    open,
                    before_completed,
                    after_completed,
                    elapsed,
                    result,
                });
                next_transition += 1;
                if failed {
                    break;
                }
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    observe_meters();
    // The endpoint drops callback telemetry when stopped, so snapshot it first.
    let callback = endpoint.callback_telemetry();
    let workers_healthy = loaded_racks.iter().all(|&rack| {
        product.worker_running(rack)
            && !product.worker_recovering(rack)
            && !product.worker_recovery_failed(rack)
    });
    let stop_result = if start_result.is_ok() {
        endpoint.stop().map_err(|error| error.to_string())
    } else {
        Ok(())
    };
    // Workers are quiescent now; these are the final rack counters.
    let rack_reports = loaded_racks
        .iter()
        .map(|&rack| (rack, telemetry.rack_diagnostics(rack)))
        .collect::<Vec<_>>();
    let cleanup_close_result = if transitions
        .iter()
        .rev()
        .find(|transition| transition.result.is_ok())
        .is_some_and(|transition| transition.open)
    {
        Some(product.set_native_editor_open(0, editor_slot, false, false))
    } else {
        None
    };
    println!("editor: cleanup_close_result={cleanup_close_result:?}");
    println!(
        "audio: frames={frames} output_peak={:?} output_rms={:?} output_clipped={} callbacks={callback:?}",
        output_meter.peak, output_meter.rms, output_meter.clipped
    );
    for (rack, report) in &rack_reports {
        println!("rack: index={} diagnostics={report:?}", rack + 1);
        let input = rack_input_meters[*rack];
        let output = rack_output_meters[*rack];
        println!(
            "rack_meter: index={} input_peak={:?} input_rms={:?} input_clipped={} output_peak={:?} output_rms={:?} output_clipped={}",
            rack + 1,
            input.peak,
            input.rms,
            input.clipped,
            output.peak,
            output.rms,
            output.clipped
        );
    }
    for diagnostic in product.diagnostics() {
        println!("diagnostic: {diagnostic}");
    }

    stop_result.expect("stop BlackHole audio before assertions");
    assert!(run_error.is_none(), "{}", run_error.unwrap_or_default());
    for transition in &transitions {
        assert!(
            transition.result.is_ok(),
            "editor {} failed after {:?}: {:?}",
            transition.name,
            transition.elapsed,
            transition.result
        );
    }
    if let Some(result) = cleanup_close_result {
        result.expect("close plug-in editor after audio stop");
    }
    assert_eq!(
        transitions.len(),
        editor_schedule.len(),
        "editor cycle incomplete"
    );
    for transition in &transitions {
        assert!(
            transition.before_completed.is_some() && transition.after_completed.is_some(),
            "editor {} has no rack completion counters",
            transition.name
        );
    }
    for pair in transitions.windows(2) {
        assert!(
            pair[1].before_completed > pair[0].after_completed,
            "rack did not complete audio between editor transitions"
        );
    }
    assert!(workers_healthy, "one or more workers recovered or stopped");
    let callback = callback.expect("callback telemetry before endpoint stop");
    assert!(
        callback.is_coherent(),
        "callback telemetry must be coherent"
    );
    assert!(callback.rendered > 0 && callback.callbacks > 0);
    let minimum_callbacks = duration.as_millis().saturating_sub(100) * 48 / u128::from(frames);
    assert!(
        u128::from(callback.callbacks) >= minimum_callbacks,
        "device callbacks stopped advancing during the live check; zero worker misses alone cannot prove continuous audio"
    );
    assert_eq!(callback.silenced, 0);
    assert_eq!(callback.invalid_frames, 0);
    assert_eq!(callback.invalid_buffers, 0);
    assert_eq!(callback.invalid_channels, 0);
    assert_eq!(callback.invalid_bytes, 0);
    let observed_frames = [
        callback.frame_histogram_32,
        callback.frame_histogram_64,
        callback.frame_histogram_128,
        callback.frame_histogram_256,
    ];
    let expected_frames = match frames {
        32 => [callback.rendered, 0, 0, 0],
        64 => [0, callback.rendered, 0, 0],
        128 => [0, 0, callback.rendered, 0],
        256 => [0, 0, 0, callback.rendered],
        _ => unreachable!("audio format was validated before starting the endpoint"),
    };
    assert_eq!(
        observed_frames, expected_frames,
        "unexpected callback frame size"
    );
    assert!(!output_meter.clipped, "audio output clipped");
    for (rack, report) in rack_reports {
        let report = report.unwrap_or_else(|| panic!("rack {} has no telemetry", rack + 1));
        assert!(
            report.completed > 0,
            "rack {} completed no blocks",
            rack + 1
        );
        assert_eq!(
            report.deadline_misses,
            0,
            "rack {} missed deadlines",
            rack + 1
        );
        assert_eq!(
            report.protocol_rejections,
            0,
            "rack {} rejected blocks",
            rack + 1
        );
        assert_eq!(report.wake_failures, 0, "rack {} failed wakes", rack + 1);
        assert_eq!(
            report.fallback_activations,
            0,
            "rack {} activated fallback",
            rack + 1
        );
        assert_eq!(
            report.gate_closed_blocks,
            0,
            "rack {} closed its gate",
            rack + 1
        );
    }
}
