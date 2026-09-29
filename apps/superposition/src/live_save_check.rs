//! Opt-in check that Save and scene capture run during playback without interrupting audio.

use std::{
    env, fs,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use sp_audio_io::{AudioEndpoint, AudioEndpointEvent, AudioFormat, AudioRouteConfig};
use sp_audio_io_macos::{MacOsAudioEndpoint, enumerate_devices};
use sp_model::{Scene, SceneId, SceneParameterTransition, SceneParameterValue};
use sp_session::{CapturedPluginState, SessionController};

use crate::{ProductRuntime, engine};

fn required_env(name: &str) -> String {
    env::var(name).unwrap_or_else(|_| panic!("{name} must be set for the live save check"))
}

#[test]
#[ignore = "starts BlackHole audio; explicit SUPERPOSITION_LIVE_SAVE_CHECK=1 required"]
#[allow(
    clippy::too_many_lines,
    reason = "the endpoint must stop before final assertions"
)]
fn save_and_scene_capture_keep_audio_running() {
    assert_eq!(required_env("SUPERPOSITION_LIVE_SAVE_CHECK"), "1");
    let source = PathBuf::from(required_env("SUPERPOSITION_LIVE_SAVE_SESSION"));
    let helpers = PathBuf::from(required_env("SUPERPOSITION_HELPERS_DIR"));
    assert!(helpers.is_absolute() && helpers.is_dir());
    let frames = env::var("SUPERPOSITION_LIVE_SAVE_FRAMES")
        .map_or(Ok(128), |value| value.parse::<u32>())
        .expect("SUPERPOSITION_LIVE_SAVE_FRAMES must be a frame count");

    // Work on a copy so the source session is never modified.
    // Unix socket paths are short; keep the fixture directly under /tmp.
    let work = PathBuf::from(format!("/tmp/sp-save-{}", std::process::id()));
    let _ = fs::remove_dir_all(&work);
    let session_path = work.join("Live.superposition");
    copy_dir(&source, &session_path);
    let app_support = work.join("support");
    fs::create_dir_all(&app_support).expect("create isolated app support");
    for file in ["plugin-catalog.json", "quarantine.json"] {
        let from = crate::default_application_support().join(file);
        if from.exists() {
            fs::copy(&from, app_support.join(file)).expect("copy catalog");
        }
    }

    let mut controller = SessionController::open(&session_path).expect("open session copy");
    let mut model = controller.document().model.clone();
    let rack_index = model
        .racks
        .iter()
        .position(|rack| !rack.slots.is_empty())
        .expect("session has a plug-in rack");
    let rack = model.racks[rack_index].clone();
    let saved: Vec<_> = rack
        .slots
        .iter()
        .map(|slot| {
            controller
                .load_plugin_state(&slot.id.0)
                .expect("read saved state")
                .map(|(component, controller, metadata)| CapturedPluginState {
                    instance_id: slot.id.0.clone(),
                    component,
                    controller,
                    metadata,
                })
        })
        .collect();
    let mut product = ProductRuntime::open(app_support).expect("open isolated product");
    product
        .load_rack(rack_index, &rack, &saved)
        .expect("load rack");

    let parameter = product
        .scene_parameter_metadata(&rack.slots[0])
        .iter()
        .find(|parameter| parameter.automatable && !parameter.read_only && !parameter.bypass)
        .cloned()
        .expect("first slot has a recallable parameter");

    let devices = enumerate_devices().expect("enumerate CoreAudio devices");
    let device = super::select_device(&devices, "BlackHole 64ch", frames)
        .expect("select BlackHole duplex device");
    let mut format = AudioFormat::product_stereo(frames).expect("valid frame count");
    format.channel_count = device.capabilities.max_output_channels.min(64);
    let route = AudioRouteConfig {
        input: Some(device.info.id.clone()),
        output: device.info.id.clone(),
        format,
    };

    let prepared = engine::prepare_audio(&mut product, &model).expect("prepare audio");
    let telemetry = prepared.renderer.telemetry();
    let mut control = prepared.control;
    let _mapping_publisher = prepared.mapping_publisher;
    let mut endpoint =
        MacOsAudioEndpoint::with_renderer(prepared.renderer).allow_device_reconfiguration();
    endpoint.start_route(route).expect("start BlackHole audio");

    // Plug-ins may request a latency-driven worker replacement right after start.
    let settle = Instant::now();
    while settle.elapsed() < Duration::from_secs(15)
        && (product.maintenance_pending() || product.worker_recovering(rack_index))
        || settle.elapsed() < Duration::from_secs(1)
    {
        product.poll();
        thread::sleep(Duration::from_millis(20));
    }
    let baseline = telemetry
        .rack_diagnostics(rack_index)
        .expect("rack telemetry");
    println!("settled: after={:?} rack={baseline:?}", settle.elapsed());

    let start = Instant::now();
    let mut run_error = None;
    let mut scene_captured = false;
    let mut saved_live = None;
    let mut completed_before_save = None;
    while start.elapsed() < Duration::from_secs(8) {
        product.poll();
        if let Some(event) = endpoint.poll_event() {
            run_error = Some(match event {
                AudioEndpointEvent::DeviceLost { device } => format!("{device} disconnected"),
                AudioEndpointEvent::DeviceConfigurationChanged { device } => {
                    format!("{device} changed configuration")
                }
            });
            break;
        }
        if !scene_captured && start.elapsed() >= Duration::from_secs(2) {
            scene_captured = true;
            let mut scene = Scene {
                id: SceneId("live".to_owned()),
                name: "Live".to_owned(),
                gains: Vec::new(),
                mutes: Vec::new(),
                rack_bypasses: Vec::new(),
                bypasses: Vec::new(),
                parameter_values: vec![SceneParameterValue {
                    rack_id: rack.id.clone(),
                    slot_id: rack.slots[0].id.clone(),
                    parameter_id: sp_model::ParameterId(parameter.id.to_string()),
                    value: sp_model::NormalizedValue(0.0),
                    transition: SceneParameterTransition::Ramp,
                }],
                transition_ms: 100,
            };
            let capture_start = Instant::now();
            if let Err(error) = product.capture_scene_parameters(&mut model, &mut scene) {
                for diagnostic in product.diagnostics() {
                    println!("diagnostic: {diagnostic}");
                }
                println!("rack: {:?}", telemetry.rack_diagnostics(rack_index));
                panic!("capture scene during playback: {error}");
            }
            model.scenes.push(scene);
            assert!(control.publish_scenes(&model), "publish live scenes");
            println!("live_capture: elapsed={:?}", capture_start.elapsed());
        }
        if scene_captured
            && telemetry.current_scene().is_none()
            && start.elapsed() >= Duration::from_secs(3)
        {
            assert!(control.trigger_scene(0));
        }
        if saved_live.is_none() && start.elapsed() >= Duration::from_secs(4) {
            completed_before_save = telemetry
                .rack_diagnostics(rack_index)
                .map(|data| data.completed);
            let save_start = Instant::now();
            controller.document_mut().model = model.clone();
            product
                .sync_worker_parameters(&mut controller.document_mut().model)
                .expect("read parameters during playback");
            let captures = product
                .capture_plugin_states(&controller.document().model)
                .expect("capture plug-in state during playback");
            controller
                .save_with_plugin_states(&captures)
                .expect("write session during playback");
            saved_live = Some((save_start.elapsed(), captures.len()));
            println!(
                "live_save: elapsed={:?} states={}",
                save_start.elapsed(),
                captures.len()
            );
        }
        thread::sleep(Duration::from_millis(20));
    }

    let callback = endpoint.callback_telemetry();
    let current_scene = telemetry.current_scene();
    let report = telemetry.rack_diagnostics(rack_index);
    let healthy = product.worker_running(rack_index)
        && !product.worker_recovering(rack_index)
        && !product.worker_recovery_failed(rack_index);
    endpoint.stop().expect("stop audio");
    println!("live_save_audio: callback={callback:?} rack={report:?} scene={current_scene:?}");

    assert!(run_error.is_none(), "{}", run_error.unwrap_or_default());
    assert!(healthy, "worker recovered or stopped during live save");
    assert_eq!(
        current_scene,
        Some(0),
        "live-captured scene reached the renderer"
    );
    let (_, states) = saved_live.expect("save ran during playback");
    assert_eq!(states, rack.slots.len(), "every slot state was captured");
    let callback = callback.expect("callback telemetry");
    assert!(callback.is_coherent());
    assert_eq!(callback.silenced, 0);
    let report = report.expect("rack telemetry");
    assert!(report.completed > completed_before_save.unwrap_or(u64::MAX));
    // Only the save and capture window counts; start-up maintenance happened before it.
    assert_eq!(report.deadline_misses, baseline.deadline_misses);
    assert_eq!(report.protocol_rejections, baseline.protocol_rejections);
    assert_eq!(report.fallback_activations, baseline.fallback_activations);
    assert_eq!(report.gate_closed_blocks, baseline.gate_closed_blocks);

    // The saved package must reopen with the live-captured state and scene.
    let reopened = SessionController::open(&session_path).expect("reopen saved session");
    assert_eq!(reopened.document().model.scenes.len(), 1);
    for slot in &rack.slots {
        assert!(
            reopened
                .load_plugin_state(&slot.id.0)
                .expect("read saved state")
                .is_some_and(|(component, _, _)| !component.is_empty()),
            "saved state is present for every slot"
        );
    }
    let _ = fs::remove_dir_all(&work);
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    fs::create_dir_all(to).expect("create copy directory");
    for entry in fs::read_dir(from).expect("read session directory") {
        let entry = entry.expect("session entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("entry type").is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).expect("copy session file");
        }
    }
}
