//! Explicitly opted-in device test using the deployed host, scanner, worker, and session store.

use std::{
    path::PathBuf,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use sp_model::{
    ChannelLayout, Endpoint, EndpointId, GainDb, NormalizedParameters, PhysicalChannels,
    PluginDescriptor, PluginFingerprint, PluginInstanceId, PluginSlot, Rack, RackChannelRoute,
    RackId, RackTopology, Source, SourceId,
};
use sp_session::{AtomicFileSessionStore, SessionDocument, SessionPackage};
use sp_supervisor::{PluginCatalog, Scanner};

#[test]
#[ignore = "opens an explicitly selected BlackHole route; build helper binaries first"]
#[allow(
    clippy::too_many_lines,
    reason = "one explicit fixture-to-device lifecycle retains its evidence together"
)]
fn saved_session_processes_live_audio() {
    let device = std::env::var("SUPERPOSITION_TEST_AUDIO_DEVICE")
        .expect("set SUPERPOSITION_TEST_AUDIO_DEVICE to an explicit BlackHole device name or ID");
    assert!(
        device == "BlackHole 64ch",
        "this test only targets BlackHole 64ch"
    );
    let frames = std::env::var("SUPERPOSITION_TEST_FRAMES").unwrap_or_else(|_| "128".into());
    assert!(matches!(frames.as_str(), "32" | "64" | "128" | "256"));
    let host = PathBuf::from(env!("CARGO_BIN_EXE_superposition"));
    let helpers = host.parent().expect("host directory");
    for name in ["sp-plugin-worker", "sp-plugin-scanner"] {
        assert!(
            helpers.join(name).is_file(),
            "build helper {name} before running this test"
        );
    }
    let root = std::env::var_os("SUPERPOSITION_TEST_ARTIFACT_DIR").map_or_else(
        || {
            std::env::temp_dir().join(format!(
                "superposition-live-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos()
            ))
        },
        PathBuf::from,
    );
    std::fs::create_dir(&root).expect("artifact directory must be new to preserve prior evidence");
    let app_support = root.join("application-support");
    std::fs::create_dir(&app_support).expect("isolated application support");
    let catalog = PluginCatalog::open(app_support.join("plugin-catalog.json")).expect("catalog");
    let mut scanner = Scanner::with_default_timeout(helpers.join("sp-plugin-scanner"), catalog);
    let bundle = std::env::var_os("SUPERPOSITION_TEST_PLUGIN_BUNDLE").map_or_else(
        || PathBuf::from("/Library/Audio/Plug-Ins/VST3/ValhallaSupermassive.vst3"),
        PathBuf::from,
    );
    let scan = scanner.scan(&bundle).expect("isolated fixture scan");
    assert!(
        scan.is_supported(),
        "fixture scan failed: {:?}",
        scan.metadata
    );
    let class = scan.metadata.classes.first().expect("fixture class");
    let mut document = SessionDocument::empty();
    let source_id = SourceId("input".into());
    let endpoint_id = EndpointId("output".into());
    document.model.sources.push(Source {
        id: source_id.clone(),
        name: "BlackHole input 1–2".into(),
        layout: ChannelLayout::Stereo,
    });
    document.model.endpoints.push(Endpoint {
        id: endpoint_id.clone(),
        name: "BlackHole output 3–4".into(),
        layout: ChannelLayout::Stereo,
    });
    document.model.racks.push(Rack {
        id: RackId("live-test".into()),
        name: "Live audio check".into(),
        source_id,
        endpoint_id,
        topology: RackTopology::Serial,
        gain_db: GainDb::new(0.0).expect("unity gain for one rack"),
        muted: false,
        bypassed: false,
        slots: vec![PluginSlot {
            id: PluginInstanceId("effect".into()),
            plugin: PluginDescriptor {
                identity: class.identity.clone(),
                fingerprint: PluginFingerprint {
                    algorithm: scan.fingerprint.algorithm,
                    digest: scan.fingerprint.digest,
                    plugin_version: class.version.clone(),
                },
            },
            bypassed: false,
            parameters: NormalizedParameters::default(),
            sidechain: None,
        }],
    });
    let rack_count = std::env::var("SUPERPOSITION_TEST_RACKS")
        .map_or(1, |value| value.parse::<usize>().expect("rack count"));
    assert!((1..=sp_model::MAX_RACKS).contains(&rack_count));
    for index in 1..rack_count {
        let mut rack = document.model.racks[0].clone();
        rack.id = RackId(format!("live-test-{index}"));
        rack.slots[0].id = PluginInstanceId(format!("effect-{index}"));
        document.model.racks.push(rack);
    }
    let rack_gain_db = if rack_count == 1 {
        0.0
    } else {
        -20.0 * f32::from(u8::try_from(rack_count).expect("bounded rack count")).log10()
    };
    for rack in &mut document.model.racks {
        rack.gain_db = GainDb::new(rack_gain_db).expect("bounded multi-rack sum");
        document.model.rack_routes.insert(
            rack.id.clone(),
            RackChannelRoute {
                input: Some(PhysicalChannels::Stereo { left: 0, right: 1 }),
                output: PhysicalChannels::Stereo { left: 2, right: 3 },
            },
        );
    }
    let session = root.join("Live.superposition");
    AtomicFileSessionStore::new(&session)
        .save_atomic(&SessionPackage::new(document))
        .expect("save valid live session");
    println!("Live test artifacts: {}", root.display());
    let mut command = Command::new(&host);
    command.args(["--headless", "--session"]).arg(&session);
    if let Ok(input_device) = std::env::var("SUPERPOSITION_TEST_INPUT_DEVICE") {
        command.args(["--input-device", &input_device, "--output-device", &device]);
    } else {
        command.args(["--device", &device]);
    }
    let output = command
        .args(["--duration-seconds", "10", "--frames", &frames])
        .env("SUPERPOSITION_APP_SUPPORT", &app_support)
        .env("SUPERPOSITION_HELPERS_DIR", helpers)
        .output()
        .expect("launch headless product host");
    std::fs::write(root.join("host.stdout.txt"), &output.stdout).expect("retain stdout");
    std::fs::write(root.join("host.stderr.txt"), &output.stderr).expect("retain stderr");
    println!("{}", String::from_utf8_lossy(&output.stdout));
    assert!(
        output.status.success(),
        "headless host failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
