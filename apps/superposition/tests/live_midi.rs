//! Opt-in `CoreMIDI` ingress check using a temporary virtual source, with no audio device or plug-in.

#![cfg(target_os = "macos")]

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use midir::os::unix::VirtualOutput;
use sp_audio_io_macos::{
    BlockFrames, InterleavedStereoF32, OutputRenderer, ProductControl, ProductRenderer,
    prepare_product_scenes,
};
use sp_engine::PreparedGraph;
use sp_midi::{
    BoundedMidiEvents, MidiInput, MidiLearnTable, MidiMappingPublisher, MidiMappingTarget,
    MidirInput,
};
use sp_model::{Scene, SceneId, Session};

#[test]
#[ignore = "creates a temporary CoreMIDI virtual source; no audio device or plug-in is opened"]
#[allow(
    clippy::too_many_lines,
    reason = "one virtual-source lifecycle keeps packet and renderer assertions together"
)]
fn virtual_source_reaches_midi_ingress_and_product_scene() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    let name = format!("superposition-midi-test-{}-{nonce}", std::process::id());
    let mut source = midir::MidiOutput::new(&name)
        .expect("CoreMIDI output client")
        .create_virtual(&name)
        .expect("CoreMIDI virtual source");

    let deadline = Instant::now() + Duration::from_secs(3);
    let port = loop {
        let matches: Vec<_> = MidirInput::enumerate_ports()
            .expect("enumerate CoreMIDI inputs")
            .into_iter()
            .filter(|port| port.name.contains(&name))
            .collect();
        if let [port] = matches.as_slice() {
            break port.clone();
        }
        assert!(
            Instant::now() < deadline,
            "virtual source did not appear once"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut observer = MidirInput::new();
    observer.open_port(&port.id).expect("open observer input");
    let mut renderer_input = MidirInput::new();
    renderer_input
        .open_port(&port.id)
        .expect("open product renderer input");
    let mut cc_monitor = renderer_input.cc_monitor();

    let mut session = Session::default();
    for index in 0..2 {
        session.scenes.push(Scene {
            id: SceneId(format!("scene-{index}")),
            name: format!("Scene {index}"),
            gains: Vec::new(),
            mutes: Vec::new(),
            rack_bypasses: Vec::new(),
            bypasses: Vec::new(),
            parameter_values: Vec::new(),
            transition_ms: 0,
        });
    }
    let (_control, control_receiver) = ProductControl::new();
    let (_mapping_publisher, mapping_receiver) = MidiMappingPublisher::new();
    let target = MidiMappingTarget {
        rack_index: 0,
        slot_index: 0,
        parameter_id: 50,
        minimum: 0.0,
        maximum: 1.0,
    };
    let mappings = MidiLearnTable::default().with_mapping(1, 15, target);
    assert_eq!(mappings.target_for(1, 15), Some(target));
    let mut renderer = ProductRenderer::new(PreparedGraph::empty())
        .with_midi_input(renderer_input)
        .with_live_control(
            control_receiver,
            mapping_receiver,
            mappings,
            prepare_product_scenes(&session),
            Vec::new(),
        );
    let telemetry = renderer.telemetry();
    let messages: [&[u8]; 4] = [
        &[0x90, 60, 100],
        &[0xb0, 15, 127],
        &[0xc0, 1],
        &[0x80, 60, 0],
    ];
    for message in messages {
        source.send(message).expect("send virtual MIDI packet");
    }

    let mut received = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(3);
    while received.len() < messages.len() && Instant::now() < deadline {
        let mut block = BoundedMidiEvents::new();
        observer.drain_into(&mut block).expect("drain MIDI ingress");
        received.extend(
            block
                .as_slice()
                .iter()
                .map(|event| event.message().to_vec()),
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        received,
        messages
            .iter()
            .map(|message| message.to_vec())
            .collect::<Vec<_>>()
    );

    let deadline = Instant::now() + Duration::from_secs(3);
    while telemetry.current_scene() != Some(1) && Instant::now() < deadline {
        let mut samples = [0.0_f32; 256];
        let block = InterleavedStereoF32::new(&mut samples, BlockFrames::Frames128)
            .expect("fixed stereo block");
        let _ = renderer.render(block);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(telemetry.current_scene(), Some(1));
    assert_eq!(
        cc_monitor.take_latest().expect("received CC").message(),
        &[0xb0, 15, 127]
    );
    assert_eq!(observer.rejected_count(), 0);
    // This proves CoreMIDI ingress and Program Change scene routing. The CC mapping is
    // configured and received, but parameter delivery to a plug-in is not observable here.
    // No plug-in worker or audio device is opened.
}
