//! Byte-level regression coverage for consecutive control-protocol frames.

use sp_protocol::control::{
    BankIdentity, ControlOperation, ControlRequest, ControlRequestGate, ControlRequestId,
    ControlTarget, RackIdentity,
};
use sp_protocol::payload::{ControlPayloadCodec, PluginSlotConfiguration, RackTopology};

#[test]
fn health_then_rebuild_frames_stay_aligned() {
    let target = ControlTarget::new(
        RackIdentity::new(0, 1).unwrap(),
        BankIdentity::new(0, 1).unwrap(),
    );
    let health = ControlRequest::new(
        ControlRequestId::new(1).unwrap(),
        target,
        ControlOperation::QueryHealth,
        None,
        &[],
    )
    .unwrap();
    let topology = RackTopology {
        slots: vec![PluginSlotConfiguration {
            slot: 0,
            input_channels: 2,
            output_channels: 2,
            event_input_active: false,
            sidechain_active: false,
            bundle_path: "/Library/Audio/Plug-Ins/VST3/ValhallaSupermassive.vst3".to_owned(),
            class_id: Some("565354207376616C76616C68616C6C61".to_owned()),
        }],
    };
    let rebuild = ControlRequest::new(
        ControlRequestId::new(2).unwrap(),
        target,
        ControlOperation::RebuildRack,
        None,
        &topology.encode().unwrap(),
    )
    .unwrap();
    let mut wire = Vec::new();
    health.write_to(&mut wire).unwrap();
    rebuild.write_to(&mut wire).unwrap();
    let mut reader = wire.as_slice();
    let mut gate = ControlRequestGate::new(target);
    let first = gate.read_from(&mut reader).expect("health frame");
    assert_eq!(first.operation(), ControlOperation::QueryHealth);
    let second = gate.read_from(&mut reader).expect("rebuild frame");
    assert_eq!(second.operation(), ControlOperation::RebuildRack);
    assert!(reader.is_empty());
}
