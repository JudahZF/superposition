//! Opt-in check that adding, removing, and rebuilding racks leaves other racks playing.

use std::{
    env, fs,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use sp_audio_io::{AudioEndpoint, AudioFormat, AudioRouteConfig};
use sp_audio_io_macos::{MacOsAudioEndpoint, ProductTelemetry, enumerate_devices};
use sp_model::{PhysicalChannels, RackChannelRoute, RackId};
use sp_session::SessionController;

use crate::{ProductRuntime, RackPlan, engine};

fn required_env(name: &str) -> String {
    env::var(name).unwrap_or_else(|_| panic!("{name} must be set for the live topology check"))
}

/// Polls the product until `done` holds, failing after a bounded wait.
fn wait_until(product: &mut ProductRuntime, mut done: impl FnMut(&mut ProductRuntime) -> bool) {
    let start = Instant::now();
    while !done(product) {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "live change did not settle"
        );
        product.poll();
        thread::sleep(Duration::from_millis(10));
    }
}

fn completed(telemetry: &ProductTelemetry, rack: usize) -> u64 {
    telemetry
        .rack_diagnostics(rack)
        .map_or(0, |data| data.completed)
}

#[test]
#[ignore = "starts BlackHole audio; explicit SUPERPOSITION_LIVE_TOPOLOGY_CHECK=1 required"]
#[allow(
    clippy::too_many_lines,
    reason = "one live run covers add, remove, and rebuild in order"
)]
fn rack_edits_keep_other_racks_playing() {
    const FRAMES: u32 = 128;
    assert_eq!(required_env("SUPERPOSITION_LIVE_TOPOLOGY_CHECK"), "1");
    let source = PathBuf::from(required_env("SUPERPOSITION_LIVE_TOPOLOGY_SESSION"));
    let helpers = PathBuf::from(required_env("SUPERPOSITION_HELPERS_DIR"));
    assert!(helpers.is_absolute() && helpers.is_dir());

    // Unix socket paths are short; keep the isolated support directory directly under /tmp.
    let work = PathBuf::from(format!("/tmp/sp-topo-{}", std::process::id()));
    let _ = fs::remove_dir_all(&work);
    let support = work.join("support");
    fs::create_dir_all(&support).expect("create isolated support");
    for file in ["plugin-catalog.json", "quarantine.json"] {
        let from = crate::default_application_support().join(file);
        if from.exists() {
            fs::copy(&from, support.join(file)).expect("copy catalog");
        }
    }

    // Two racks with the source session's first rack chain, on output pairs 3-4 and 5-6.
    let controller = SessionController::open(&source).expect("open source session");
    let template = controller
        .document()
        .model
        .racks
        .iter()
        .find(|rack| !rack.slots.is_empty())
        .cloned()
        .expect("source session has a plug-in rack");
    let mut model = controller.document().model.clone();
    model.racks.clear();
    model.rack_routes.clear();
    model.scenes.clear();
    model.midi_mappings.clear();
    let rack = |index: usize| {
        let mut rack = template.clone();
        rack.id = RackId(format!("live-{index}"));
        rack.name = format!("Live {index}");
        rack.gain_db = sp_model::GainDb::new(-12.0).expect("gain");
        for (slot, plugin) in rack.slots.iter_mut().enumerate() {
            plugin.id = sp_model::PluginInstanceId(format!("live-{index}-{slot}"));
        }
        rack
    };
    let route = |index: u8| RackChannelRoute {
        input: Some(PhysicalChannels::Stereo { left: 0, right: 1 }),
        output: PhysicalChannels::Stereo {
            left: 2 + 2 * index,
            right: 3 + 2 * index,
        },
    };
    for index in 0..2 {
        let rack = rack(usize::from(index));
        model.rack_routes.insert(rack.id.clone(), route(index));
        model.racks.push(rack);
    }

    let mut product = ProductRuntime::open(support).expect("open product");
    for (index, rack) in model.racks.iter().enumerate() {
        product.load_rack(index, rack, &[]).expect("load rack");
    }
    let devices = enumerate_devices().expect("enumerate devices");
    let device = super::select_device(&devices, "BlackHole 64ch", FRAMES).expect("BlackHole");
    let mut format = AudioFormat::product_stereo(FRAMES).expect("format");
    format.channel_count = device.capabilities.max_output_channels.min(64);
    let prepared = engine::prepare_audio(&mut product, &model).expect("prepare audio");
    let telemetry = prepared.renderer.telemetry();
    let mut control = prepared.control;
    let _mapping_publisher = prepared.mapping_publisher;
    let mut endpoint =
        MacOsAudioEndpoint::with_renderer(prepared.renderer).allow_device_reconfiguration();
    endpoint
        .start_route(AudioRouteConfig {
            input: Some(device.info.id.clone()),
            output: device.info.id.clone(),
            format,
        })
        .expect("start audio");
    thread::sleep(Duration::from_secs(2));
    let baseline = telemetry.rack_diagnostics(1).expect("rack 2 telemetry");
    let callbacks_before = endpoint.callback_telemetry().expect("callbacks").callbacks;

    // 1. Add a third rack while racks 1 and 2 keep playing.
    let before: Vec<RackId> = model.racks.iter().map(|rack| rack.id.clone()).collect();
    let added = rack(2);
    model.rack_routes.insert(added.id.clone(), route(2));
    model.racks.push(added);
    let plan = [
        RackPlan::Keep(0),
        RackPlan::Keep(1),
        RackPlan::Rebuild(None),
    ];
    let started = Instant::now();
    product
        .publish_live_topology(&mut control, &model, &plan, &[vec![], vec![], vec![]])
        .expect("add rack live");
    println!("add: publish took {:?}", started.elapsed());
    wait_until(&mut product, |product| {
        product.finish_live_topology(&mut control);
        control.changes_applied()
    });
    thread::sleep(Duration::from_secs(1));
    assert!(completed(&telemetry, 2) > 0, "new rack delivers wet audio");

    // 2. Remove rack 1; rack 2 moves to position 0 and must not restart.
    let _ = before;
    model.racks.remove(0);
    model.rack_routes.remove(&RackId("live-0".to_owned()));
    let kept_before_remove = completed(&telemetry, 1);
    let plan = [RackPlan::Keep(1), RackPlan::Keep(2)];
    product
        .publish_live_topology(&mut control, &model, &plan, &[vec![], vec![]])
        .expect("remove rack live");
    wait_until(&mut product, |product| {
        product.finish_live_topology(&mut control);
        control.changes_applied()
    });
    thread::sleep(Duration::from_secs(1));
    assert!(
        completed(&telemetry, 0) > kept_before_remove,
        "moved rack keeps its counters and keeps completing"
    );

    // 3. Rebuild rack at position 1 (a rack reload) while position 0 plays on.
    let plan = [RackPlan::Keep(0), RackPlan::Rebuild(Some(1))];
    product
        .publish_live_topology(&mut control, &model, &plan, &[vec![], vec![]])
        .expect("rebuild rack live");
    wait_until(&mut product, |product| {
        product.finish_live_topology(&mut control);
        control.changes_applied()
    });
    thread::sleep(Duration::from_secs(2));

    let callback = endpoint.callback_telemetry().expect("callback telemetry");
    let untouched = telemetry.rack_diagnostics(0).expect("untouched rack");
    let rebuilt = telemetry.rack_diagnostics(1).expect("rebuilt rack");
    let healthy = product.worker_running(0) && product.worker_running(1);
    endpoint.stop().expect("stop audio");
    product.stop_retiring_workers();
    println!("topology_audio: callback={callback:?}");
    println!("untouched_rack: {untouched:?}");
    println!("rebuilt_rack: {rebuilt:?}");
    for diagnostic in product.diagnostics() {
        println!("diagnostic: {diagnostic}");
    }

    assert!(healthy, "both remaining workers run");
    assert!(callback.is_coherent());
    assert_eq!(callback.silenced, 0, "no callback rendered silence");
    // The rack that was never edited completed every callback after the baseline.
    let elapsed_callbacks = callback.callbacks - callbacks_before;
    let untouched_completed = untouched.completed - baseline.completed;
    println!("untouched completed {untouched_completed} of {elapsed_callbacks} callbacks");
    assert!(
        untouched_completed + 2 >= elapsed_callbacks,
        "untouched rack missed blocks during live edits"
    );
    assert_eq!(untouched.deadline_misses, baseline.deadline_misses);
    assert_eq!(
        untouched.fallback_activations,
        baseline.fallback_activations
    );
    assert_eq!(untouched.gate_closed_blocks, baseline.gate_closed_blocks);
    assert!(
        rebuilt.completed > 0,
        "rebuilt rack delivers wet audio again"
    );
    let _ = fs::remove_dir_all(&work);
}
