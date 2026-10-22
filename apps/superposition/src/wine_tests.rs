//! The app's own path for a Windows rack, under real Wine: the scan catalogs the plug-in as
//! Windows, the rack launches the Windows worker through Wine, joins audio as a pipelined lane,
//! and restarts at a new session rate.
//!
//! Ignored: it needs Wine, Xvfb, the native and Windows helpers, and a Windows bundle. From the
//! dev shell:
//!
//! ```text
//! cargo build -p sp-plugin-worker -p sp-plugin-scanner -p sp-window-streamer
//! cargo xtask windows-helpers
//! SUPERPOSITION_HELPERS_DIR=$PWD/target/debug \
//! SUPERPOSITION_TEST_WINDOWS_VST3="/path/to/Plugin.vst3" \
//!     cargo test -p superposition wine_tests -- --ignored
//! ```
//!
//! The app data, prefix included, lives under `target/tmp` and is deleted afterwards.

use std::path::PathBuf;

use sp_model::{
    ChannelLayout, Endpoint, EndpointId, Rack, RackId, RackTopology, Session, Source, SourceId,
};

use crate::{ProductRuntime, engine};

#[test]
#[ignore = "needs Wine, Xvfb, SUPERPOSITION_HELPERS_DIR, and SUPERPOSITION_TEST_WINDOWS_VST3"]
fn windows_rack_runs_pipelined_in_the_wine_worker() {
    // The renderer is large in a debug build; the app's main thread has more stack than a test.
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(run)
        .expect("test thread")
        .join()
        .expect("the Windows rack test passes");
}

fn run() {
    let bundle = PathBuf::from(
        std::env::var_os("SUPERPOSITION_TEST_WINDOWS_VST3")
            .expect("set SUPERPOSITION_TEST_WINDOWS_VST3 to a Windows .vst3 bundle"),
    );
    let support = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/tmp")
        .join(format!("app-wine-{}", std::process::id()));
    let mut product = ProductRuntime::open(&support).expect("open the product runtime");
    assert!(product.windows_available(), "{:?}", product.diagnostics());

    // The first scan prepares the prefix; the second finds the installed bundle.
    product
        .scan_standard_locations(true)
        .expect("prepare and scan");
    let vst3 = product.wine.as_ref().expect("Wine").vst3_dir();
    let status = std::process::Command::new("cp")
        .arg("-r")
        .arg(&bundle)
        .arg(&vst3)
        .status()
        .expect("run cp");
    assert!(status.success());
    product.scan_standard_locations(true).expect("rescan");
    let plugin = product
        .catalog_plugins()
        .into_iter()
        .find(|plugin| plugin.windows)
        .expect("the Windows plug-in is cataloged as Windows");

    let mut model = Session::new();
    model.sources.push(Source {
        id: SourceId("input-1".to_owned()),
        name: "Input 1".to_owned(),
        layout: ChannelLayout::Stereo,
    });
    model.endpoints.push(Endpoint {
        id: EndpointId("output-1".to_owned()),
        name: "Output 1".to_owned(),
        layout: ChannelLayout::Stereo,
    });
    model.racks.push(Rack {
        id: RackId("rack-1".to_owned()),
        name: "Rack 1".to_owned(),
        source_id: SourceId("input-1".to_owned()),
        endpoint_id: EndpointId("output-1".to_owned()),
        topology: RackTopology::Serial,
        gain_db: sp_model::GainDb::default(),
        muted: false,
        bypassed: false,
        slots: vec![sp_model::PluginSlot {
            id: sp_model::PluginInstanceId("slot-1".to_owned()),
            plugin: plugin.descriptor.clone(),
            bypassed: false,
            parameters: plugin.parameters.clone(),
            sidechain: None,
        }],
    });

    product
        .load_rack(0, &model.racks[0], &[])
        .expect("launch the Windows worker through Wine");
    assert!(product.worker_running(0));
    assert!(product.rack_is_pipelined(0));
    assert_eq!(product.audio_bank_mappings().unwrap().len(), 1);
    // Audio start maps the Windows rack and marks it pipelined.
    let prepared = engine::prepare_audio(&mut product, &model).expect("prepare audio");
    drop(prepared);

    // A new session rate restarts the worker at that rate, keeping its state.
    let before = product.workers[&0].session.as_ref().unwrap().process_id();
    product
        .apply_sample_rate(96_000)
        .expect("restart at 96 kHz");
    let worker = &product.workers[&0];
    assert_eq!(worker.sample_rate_hz, 96_000);
    assert_ne!(worker.session.as_ref().unwrap().process_id(), before);
    assert!(product.rack_is_pipelined(0));

    product.unload_all_racks();
    drop(product);
    std::fs::remove_dir_all(&support).expect("delete the test app data and prefix");
}
