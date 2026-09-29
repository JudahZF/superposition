//! Opt-in live scene recall check with the approved Pro-Q 4 `BlackHole` fixture.

use std::{
    env, fs,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use sp_audio_io::{AudioEndpoint, AudioEndpointEvent, AudioFormat, AudioRouteConfig};
use sp_audio_io_macos::{MacOsAudioEndpoint, enumerate_devices};
use sp_model::{
    NormalizedValue, ParameterId, PhysicalChannels, RackChannelRoute, Scene, SceneId,
    SceneParameterTransition, SceneParameterValue,
};
use sp_session::{AtomicFileSessionStore, CapturedPluginState, SessionController};

use crate::{ProductRuntime, engine};

const PRO_Q_4_CLASS_ID: &str = "ED57BD725C60467EA64DD2F400758B6F";
const OUTPUT_LEVEL_ID: u32 = 556;
const PRO_Q_4_PARAMETER_COUNT: usize = 713;

fn required_env(name: &str) -> String {
    env::var(name).unwrap_or_else(|_| panic!("{name} must be set for the live scene check"))
}

fn approved_temp_directory(path: &Path) -> PathBuf {
    let canonical = fs::canonicalize(path).expect("approved fixture directory exists");
    assert!(
        canonical.starts_with("/private/tmp") || canonical.starts_with("/tmp"),
        "live scene fixtures must stay under /tmp"
    );
    canonical
}

fn scene_with_output_level(
    id: &str,
    rack_id: &sp_model::RackId,
    slot_id: &sp_model::PluginInstanceId,
    value: NormalizedValue,
) -> Scene {
    Scene {
        id: SceneId(id.to_owned()),
        name: id.to_owned(),
        gains: Vec::new(),
        mutes: Vec::new(),
        rack_bypasses: Vec::new(),
        bypasses: Vec::new(),
        parameter_values: vec![SceneParameterValue {
            rack_id: rack_id.clone(),
            slot_id: slot_id.clone(),
            parameter_id: ParameterId(OUTPUT_LEVEL_ID.to_string()),
            value,
            transition: SceneParameterTransition::Ramp,
        }],
        transition_ms: 250,
    }
}

#[test]
#[ignore = "starts approved BlackHole audio; explicit SUPERPOSITION_LIVE_SCENE_CHECK=1 required"]
#[allow(
    clippy::too_many_lines,
    reason = "the live endpoint must be stopped before final assertions and parameter reads"
)]
fn pro_q_scene_recall_keeps_blackhole_audio_healthy() {
    assert_eq!(
        required_env("SUPERPOSITION_LIVE_SCENE_CHECK"),
        "1",
        "explicit SUPERPOSITION_LIVE_SCENE_CHECK=1 is required"
    );
    let session_path = PathBuf::from(required_env("SUPERPOSITION_LIVE_SCENE_SESSION"));
    let app_support = PathBuf::from(required_env("SUPERPOSITION_LIVE_SCENE_APP_SUPPORT"));
    let helpers = PathBuf::from(required_env("SUPERPOSITION_HELPERS_DIR"));
    let frames = env::var("SUPERPOSITION_LIVE_SCENE_FRAMES")
        .unwrap_or_else(|_| "64".to_owned())
        .parse::<u32>()
        .expect("SUPERPOSITION_LIVE_SCENE_FRAMES must be 32 or 64");
    let seconds = env::var("SUPERPOSITION_LIVE_SCENE_SECONDS")
        .unwrap_or_else(|_| "12".to_owned())
        .parse::<u64>()
        .expect("SUPERPOSITION_LIVE_SCENE_SECONDS must be 12..=20");
    assert!(matches!(frames, 32 | 64));
    assert!((12..=20).contains(&seconds));
    let restart_enabled = env::var("SUPERPOSITION_LIVE_SCENE_RESTART")
        .ok()
        .is_some_and(|value| {
            assert_eq!(value, "1", "SUPERPOSITION_LIVE_SCENE_RESTART must be 1");
            true
        });
    assert!(helpers.is_absolute() && helpers.is_dir());
    let session_path = approved_temp_directory(&session_path);
    let app_support = approved_temp_directory(&app_support);

    AtomicFileSessionStore::new(&session_path)
        .load()
        .expect("approved saved session exists");
    let controller = SessionController::open(&session_path).expect("open approved session");
    let mut model = controller.document().model.clone();
    assert_eq!(model.racks.len(), 1, "fixture must contain one Pro-Q rack");
    assert_eq!(
        model.racks[0].slots.len(),
        1,
        "fixture must contain one Pro-Q slot"
    );
    assert!(
        model.scenes.is_empty(),
        "fixture must not contain saved scenes"
    );
    let rack = &model.racks[0];
    let slot = &rack.slots[0];
    assert_eq!(slot.plugin.identity.vendor, "FabFilter");
    assert_eq!(slot.plugin.identity.name, "Pro-Q 4");
    assert_eq!(slot.plugin.identity.unique_id, PRO_Q_4_CLASS_ID);
    assert!(!rack.bypassed && !slot.bypassed);
    assert_eq!(slot.parameters.values.len(), PRO_Q_4_PARAMETER_COUNT);
    assert_eq!(
        model.rack_routes.get(&rack.id),
        Some(&RackChannelRoute {
            input: Some(PhysicalChannels::Stereo { left: 0, right: 1 }),
            output: PhysicalChannels::Stereo { left: 2, right: 3 },
        }),
        "fixture must route BlackHole input 1–2 to output 3–4"
    );
    let rack_id = rack.id.clone();
    let slot_id = slot.id.clone();

    let devices = enumerate_devices().expect("enumerate CoreAudio devices");
    let selected = super::select_device(&devices, "BlackHole 64ch", frames)
        .expect("select exact-frame BlackHole duplex device");
    assert_eq!(selected.info.name, "BlackHole 64ch");
    assert_eq!(selected.capabilities.max_input_channels, 64);
    assert_eq!(selected.capabilities.max_output_channels, 64);
    let mut format = AudioFormat::product_stereo(frames).expect("valid callback frame count");
    format.channel_count = 64;
    let route = AudioRouteConfig {
        input: Some(selected.info.id.clone()),
        output: selected.info.id.clone(),
        format,
    };

    let mut product = ProductRuntime::open(app_support).expect("open isolated product support");
    let saved = controller
        .load_plugin_state(&slot_id.0)
        .expect("read saved Pro-Q state")
        .map(|(component, controller, metadata)| CapturedPluginState {
            instance_id: slot_id.0.clone(),
            component,
            controller,
            metadata,
        });
    product
        .load_rack(0, &model.racks[0], &[saved])
        .expect("load approved Pro-Q rack");
    assert!(product.worker_running(0));
    let (parameter_index, catalog_parameter) = product
        .scene_parameter_metadata(&model.racks[0].slots[0])
        .iter()
        .enumerate()
        .find(|(_, parameter)| parameter.id == OUTPUT_LEVEL_ID)
        .expect("catalog has Pro-Q Output Level");
    assert_eq!(catalog_parameter.name, "Output Level");
    assert!(catalog_parameter.automatable && !catalog_parameter.read_only);
    assert!(
        parameter_index > 256,
        "selected parameter must exercise capture beyond the first 256 catalog entries"
    );

    let mut higher_scene = scene_with_output_level(
        "higher-output-level",
        &rack_id,
        &slot_id,
        NormalizedValue::new(0.5).expect("normalized value"),
    );
    product
        .capture_scene_parameters(&mut model, &mut higher_scene)
        .expect("capture selected Pro-Q parameter while audio is stopped");
    let captured = higher_scene.parameter_values[0].value.get();
    assert!(
        captured > 0.10,
        "fixture Output Level must allow a safe reduction"
    );
    assert_eq!(
        higher_scene.parameter_values[0].transition,
        SceneParameterTransition::Ramp,
        "Output Level must be continuous in worker metadata"
    );
    let reduced = NormalizedValue::new(captured - 0.05).expect("safe lower output level");
    let higher = NormalizedValue::new(captured - 0.025).expect("safe higher output level");
    higher_scene.parameter_values[0].value = higher;
    let mut lower_scene = higher_scene.clone();
    lower_scene.id = SceneId("lower-output-level".to_owned());
    lower_scene.name = "Lower output level".to_owned();
    lower_scene.parameter_values[0].value = reduced;
    model.scenes = vec![lower_scene, higher_scene];
    assert_eq!(
        model.racks[0].slots[0].parameters.values.len(),
        PRO_Q_4_PARAMETER_COUNT
    );
    model
        .validate_for_alpha()
        .expect("scene fixture remains valid");
    println!(
        "scene_setup: bundle=Pro-Q4 parameter_id={OUTPUT_LEVEL_ID} catalog_index={parameter_index} captured={captured:.6} lower={:.6} higher={:.6} retained_snapshot_parameters={}",
        reduced.get(),
        higher.get(),
        model.racks[0].slots[0].parameters.values.len()
    );

    let prepared = engine::prepare_audio(&mut product, &model).expect("prepare live scenes");
    let telemetry = prepared.renderer.telemetry();
    let mut control = prepared.control;
    let _mapping_publisher = prepared.mapping_publisher;
    let _mappings = prepared.mappings;
    let mut endpoint =
        MacOsAudioEndpoint::with_renderer(prepared.renderer).allow_device_reconfiguration();
    let started_audio = endpoint.start_route(route.clone());
    let mut run_error = started_audio
        .as_ref()
        .err()
        .map(|error| format!("CoreAudio could not start: {error}"));
    let mut next_scene = 0;
    let mut scene_completions = Vec::new();
    let mut output_peak = [0.0_f32; 2];
    let mut output_clipped = false;
    let duration = Duration::from_secs(seconds);
    if started_audio.is_ok() {
        let start = Instant::now();
        while start.elapsed() < duration {
            product.poll();
            let meter = telemetry.take_output_meter();
            for (peak, observed) in output_peak.iter_mut().zip(meter.peak) {
                *peak = (*peak).max(observed);
            }
            output_clipped |= meter.clipped;
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
            let due = [Duration::from_secs(3), Duration::from_secs(7)];
            if next_scene < due.len() && start.elapsed() >= due[next_scene] {
                let before = telemetry.rack_diagnostics(0).map(|report| report.completed);
                let trigger_start = Instant::now();
                let accepted = control.trigger_scene(next_scene);
                let elapsed = trigger_start.elapsed();
                println!(
                    "scene_trigger: scene={next_scene} audio_elapsed={:?} enqueue_elapsed={elapsed:?} before_completed={before:?} accepted={accepted}",
                    start.elapsed()
                );
                scene_completions.push((before, accepted));
                next_scene += 1;
                if !accepted {
                    run_error = Some("scene command queue rejected a trigger".to_owned());
                    break;
                }
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    // Capture callback-owned counters before stop; no assertions or controller calls until the
    // endpoint has stopped and its realtime thread can no longer race the final metadata read.
    let final_meter = telemetry.take_output_meter();
    for (peak, observed) in output_peak.iter_mut().zip(final_meter.peak) {
        *peak = (*peak).max(observed);
    }
    output_clipped |= final_meter.clipped;
    let callback = endpoint.callback_telemetry();
    let current_scene = telemetry.current_scene();
    let pre_stop_report = telemetry.rack_diagnostics(0);
    let workers_healthy = product.worker_running(0)
        && !product.worker_recovering(0)
        && !product.worker_recovery_failed(0);
    let stop_result = if started_audio.is_ok() {
        endpoint.stop().map_err(|error| error.to_string())
    } else {
        Ok(())
    };
    let first_stopped_report = telemetry.rack_diagnostics(0);
    println!(
        "scene_segment: name=initial seconds={seconds} callback={callback:?} rack={first_stopped_report:?} output_peak={output_peak:?} current_scene={current_scene:?} stop={stop_result:?}"
    );

    let mut restart_error = None;
    let mut restart_callback = None;
    let mut restart_report = None;
    let mut restart_peak = [0.0_f32; 2];
    let mut restart_clipped = false;
    let mut restart_scene_before = None;
    let mut restart_scene_after = None;
    let mut restart_pre_trigger_peak = None;
    let mut restart_triggers = Vec::new();
    let mut restart_stop_result = Ok(());
    let mut restart_workers_healthy = None;
    if restart_enabled && started_audio.is_ok() && stop_result.is_ok() && run_error.is_none() {
        restart_scene_before = telemetry.current_scene();
        match endpoint.start_route(route) {
            Ok(()) => {
                let restart_start = Instant::now();
                while restart_start.elapsed() < Duration::from_secs(4) {
                    product.poll();
                    let meter = telemetry.take_output_meter();
                    for (peak, observed) in restart_peak.iter_mut().zip(meter.peak) {
                        *peak = (*peak).max(observed);
                    }
                    restart_clipped |= meter.clipped;
                    if let Some(event) = endpoint.poll_event() {
                        restart_error = Some(format!("restart device event: {event:?}"));
                        break;
                    }
                    let due = [Duration::from_secs(1), Duration::from_secs(2)];
                    if restart_triggers.len() < due.len()
                        && restart_start.elapsed() >= due[restart_triggers.len()]
                    {
                        let scene = restart_triggers.len();
                        if scene == 0 {
                            restart_pre_trigger_peak = Some(restart_peak);
                        }
                        let before = telemetry.rack_diagnostics(0).map(|data| data.completed);
                        let accepted = control.trigger_scene(scene);
                        println!(
                            "scene_restart_trigger: scene={scene} elapsed={:?} before_completed={before:?} accepted={accepted}",
                            restart_start.elapsed()
                        );
                        restart_triggers.push((before, accepted));
                        if !accepted {
                            restart_error =
                                Some("restart scene command queue rejected a trigger".to_owned());
                            break;
                        }
                    }
                    thread::sleep(Duration::from_millis(25));
                }
                let meter = telemetry.take_output_meter();
                for (peak, observed) in restart_peak.iter_mut().zip(meter.peak) {
                    *peak = (*peak).max(observed);
                }
                restart_clipped |= meter.clipped;
                restart_callback = endpoint.callback_telemetry();
                restart_scene_after = telemetry.current_scene();
                let pre_stop = telemetry.rack_diagnostics(0);
                restart_workers_healthy = Some(
                    product.worker_running(0)
                        && !product.worker_recovering(0)
                        && !product.worker_recovery_failed(0),
                );
                restart_stop_result = endpoint.stop().map_err(|error| error.to_string());
                restart_report = telemetry.rack_diagnostics(0);
                println!(
                    "scene_segment: name=restart seconds=4 callback={restart_callback:?} pre_stop_rack={pre_stop:?} final_rack={restart_report:?} output_peak={restart_peak:?} clipped={restart_clipped} scene_before={restart_scene_before:?} scene_after={restart_scene_after:?} stop={restart_stop_result:?} error={restart_error:?}"
                );
            }
            Err(error) => restart_error = Some(format!("CoreAudio could not restart: {error}")),
        }
    }
    let report = restart_report.or(first_stopped_report);
    let final_value = if started_audio.is_ok()
        && stop_result.is_ok()
        && restart_stop_result.is_ok()
        && restart_error.is_none()
    {
        let worker = product.workers.get_mut(&0).expect("Pro-Q worker retained");
        let session = worker
            .session
            .as_mut()
            .expect("Pro-Q control session retained");
        Some(
            crate::worker_parameter_metadata(session, 0, OUTPUT_LEVEL_ID)
                .expect("read final Pro-Q Output Level after stopping audio")
                .normalized,
        )
    } else {
        None
    };
    println!(
        "scene_audio: frames={frames} seconds={seconds} callback={callback:?} pre_stop_rack={pre_stop_report:?} final_rack={report:?} output_peak={output_peak:?} clipped={output_clipped} current_scene={current_scene:?} final_output_level={final_value:?} run_error={run_error:?}"
    );
    for diagnostic in product.diagnostics() {
        println!("diagnostic: {diagnostic}");
    }

    stop_result.expect("stop BlackHole audio before assertions");
    restart_stop_result.expect("stop restarted BlackHole audio before assertions");
    assert!(run_error.is_none(), "{}", run_error.unwrap_or_default());
    assert!(
        restart_error.is_none(),
        "{}",
        restart_error.unwrap_or_default()
    );
    assert!(
        workers_healthy,
        "Pro-Q worker recovered or stopped during scene recall"
    );
    assert_eq!(scene_completions.len(), 2, "both scenes must trigger");
    assert!(scene_completions.iter().all(|(_, accepted)| *accepted));
    assert_eq!(
        current_scene,
        Some(1),
        "second scene must reach the renderer"
    );
    let callback = callback.expect("callback telemetry before endpoint stop");
    assert!(callback.is_coherent());
    assert!(callback.rendered > 0 && callback.callbacks > 0);
    let minimum_callbacks = duration.as_millis().saturating_sub(100) * 48 / u128::from(frames);
    assert!(
        u128::from(callback.callbacks) >= minimum_callbacks,
        "initial device callbacks did not cover the requested audio duration"
    );
    assert_eq!(callback.silenced, 0);
    assert_eq!(callback.invalid_frames, 0);
    assert_eq!(callback.invalid_buffers, 0);
    assert_eq!(callback.invalid_channels, 0);
    assert_eq!(callback.invalid_bytes, 0);
    let expected_histogram = if frames == 32 {
        [callback.rendered, 0, 0, 0]
    } else {
        [0, callback.rendered, 0, 0]
    };
    assert_eq!(
        [
            callback.frame_histogram_32,
            callback.frame_histogram_64,
            callback.frame_histogram_128,
            callback.frame_histogram_256,
        ],
        expected_histogram,
        "callback did not use exact requested frame size"
    );
    assert!(!output_clipped, "scene playback clipped BlackHole output");
    assert!(
        output_peak.iter().any(|peak| *peak > 0.0),
        "BlackHole output stayed silent; external whisper probe is required"
    );
    let pre_stop_report = pre_stop_report.expect("rack telemetry before endpoint stop");
    assert!(pre_stop_report.completed > 0);
    assert_eq!(pre_stop_report.deadline_misses, 0);
    assert_eq!(pre_stop_report.protocol_rejections, 0);
    assert_eq!(pre_stop_report.wake_failures, 0);
    assert_eq!(pre_stop_report.fallback_activations, 0);
    assert_eq!(pre_stop_report.gate_closed_blocks, 0);
    let report = report.expect("rack telemetry remains after endpoint stop");
    assert!(report.completed > 0);
    assert_eq!(report.deadline_misses, 0);
    assert_eq!(report.protocol_rejections, 0);
    assert_eq!(report.wake_failures, 0);
    assert_eq!(report.fallback_activations, 0);
    assert_eq!(report.gate_closed_blocks, 0);
    assert!(
        scene_completions[0]
            .0
            .zip(scene_completions[1].0)
            .is_some_and(|(first, second)| second > first),
        "rack must keep completing blocks between scene triggers"
    );
    for (before, _) in scene_completions {
        assert!(before.is_some_and(|completed| report.completed > completed));
    }
    if restart_enabled {
        assert_eq!(
            restart_scene_before,
            Some(1),
            "scene was lost before restart"
        );
        assert_eq!(restart_scene_after, Some(1), "scene was lost after restart");
        assert_eq!(
            restart_workers_healthy,
            Some(true),
            "worker failed after restart"
        );
        assert_eq!(
            restart_triggers.len(),
            2,
            "restart control did not recall both scenes"
        );
        assert!(restart_triggers.iter().all(|(_, accepted)| *accepted));
        let restart_callback = restart_callback.expect("restart callback telemetry exists");
        assert!(restart_callback.is_coherent());
        assert!(restart_callback.rendered > 0 && restart_callback.callbacks > 0);
        let minimum_restart_callbacks = 3_900 * 48 / u128::from(frames);
        assert!(
            u128::from(restart_callback.callbacks) >= minimum_restart_callbacks,
            "restart device callbacks did not cover the requested audio duration"
        );
        assert_eq!(restart_callback.silenced, 0);
        assert_eq!(restart_callback.invalid_frames, 0);
        assert_eq!(restart_callback.invalid_buffers, 0);
        assert_eq!(restart_callback.invalid_channels, 0);
        assert_eq!(restart_callback.invalid_bytes, 0);
        let restart_histogram = if frames == 32 {
            [restart_callback.rendered, 0, 0, 0]
        } else {
            [0, restart_callback.rendered, 0, 0]
        };
        assert_eq!(
            [
                restart_callback.frame_histogram_32,
                restart_callback.frame_histogram_64,
                restart_callback.frame_histogram_128,
                restart_callback.frame_histogram_256,
            ],
            restart_histogram,
            "restart callback did not use exact requested frame size"
        );
        assert!(
            !restart_clipped,
            "restart playback clipped BlackHole output"
        );
        assert!(
            restart_peak.iter().any(|peak| *peak > 0.0),
            "BlackHole output stayed silent after restart"
        );
        assert!(
            restart_pre_trigger_peak.is_some_and(|peaks| peaks.iter().any(|peak| *peak > 0.0)),
            "retained scene produced no output before restart control commands"
        );
        let first = first_stopped_report.expect("initial rack telemetry exists");
        let second = restart_report.expect("restart rack telemetry exists");
        assert!(
            second.completed > first.completed,
            "rack stopped dispatching after restart"
        );
        assert_eq!(second.deadline_misses, first.deadline_misses);
        assert_eq!(second.protocol_rejections, first.protocol_rejections);
        assert_eq!(second.wake_failures, first.wake_failures);
        assert_eq!(second.fallback_activations, first.fallback_activations);
        assert_eq!(second.gate_closed_blocks, first.gate_closed_blocks);
        assert!(
            restart_triggers[0]
                .0
                .is_some_and(|completed| completed > first.completed),
            "rack did not resume dispatch before restart scene controls"
        );
        assert!(
            restart_triggers[0]
                .0
                .zip(restart_triggers[1].0)
                .is_some_and(|(first, second)| second > first),
            "rack did not complete blocks between restart scene triggers"
        );
    }
    assert!(
        final_value.is_some_and(|value| (value - f64::from(higher.get())).abs() < 1e-5),
        "final Pro-Q Output Level must match the second scene, not the initial value"
    );
    assert_eq!(
        model.racks[0].slots[0].parameters.values.len(),
        PRO_Q_4_PARAMETER_COUNT
    );
}
