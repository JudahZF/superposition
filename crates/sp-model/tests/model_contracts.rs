//! Integration tests for versioned model contracts.

use std::collections::BTreeMap;

use sp_model::{
    ChannelLayout, Endpoint, EndpointId, GainDb, MAX_RACKS, MidiController, MidiMapping,
    MidiMappingId, NormalizedParameters, NormalizedValue, ParameterAddress, ParameterId,
    PluginDescriptor, PluginFingerprint, PluginIdentity, PluginInstanceId, PluginSlot, Rack,
    RackGain, RackId, RackMute, RackTopology, Scene, SceneId, SceneParameterValue, Session,
    SlotBypass, Source, SourceId, ValidationError,
};

fn valid_session() -> Session {
    let mut parameters = BTreeMap::new();
    parameters.insert(
        ParameterId("mix".into()),
        NormalizedValue::new(0.5).unwrap(),
    );

    Session {
        sources: vec![Source {
            id: SourceId("input-a".into()),
            name: "Input A".into(),
            layout: ChannelLayout::Stereo,
        }],
        endpoints: vec![Endpoint {
            id: EndpointId("output-a".into()),
            name: "Output A".into(),
            layout: ChannelLayout::Stereo,
        }],
        racks: vec![Rack {
            id: RackId("rack-a".into()),
            name: "Rack A".into(),
            source_id: SourceId("input-a".into()),
            endpoint_id: EndpointId("output-a".into()),
            topology: RackTopology::default(),
            slots: vec![PluginSlot {
                id: PluginInstanceId("slot-a".into()),
                plugin: PluginDescriptor {
                    identity: PluginIdentity {
                        vendor: "Acme".into(),
                        name: "Filter".into(),
                        unique_id: "acme.filter".into(),
                    },
                    fingerprint: PluginFingerprint {
                        algorithm: "sha256".into(),
                        digest: "aabbcc".into(),
                        plugin_version: "1.0.0".into(),
                    },
                },
                parameters: NormalizedParameters { values: parameters },
            }],
        }],
        scenes: vec![Scene {
            id: SceneId("verse".into()),
            name: "Verse".into(),
            gains: vec![RackGain {
                rack_id: RackId("rack-a".into()),
                gain_db: GainDb::new(-3.0).unwrap(),
            }],
            mutes: vec![RackMute {
                rack_id: RackId("rack-a".into()),
                muted: false,
            }],
            bypasses: vec![SlotBypass {
                rack_id: RackId("rack-a".into()),
                slot_id: PluginInstanceId("slot-a".into()),
                bypassed: false,
            }],
            parameter_values: vec![SceneParameterValue {
                rack_id: RackId("rack-a".into()),
                slot_id: PluginInstanceId("slot-a".into()),
                parameter_id: ParameterId("mix".into()),
                value: NormalizedValue::new(0.75).unwrap(),
            }],
            transition_ms: 250,
        }],
        midi_mappings: vec![MidiMapping {
            id: MidiMappingId("mix-knob".into()),
            source: MidiController {
                channel: 1,
                controller: 74,
            },
            target: ParameterAddress {
                rack_id: RackId("rack-a".into()),
                slot_id: PluginInstanceId("slot-a".into()),
                parameter_id: ParameterId("mix".into()),
            },
            minimum: NormalizedValue::new(0.0).unwrap(),
            maximum: NormalizedValue::new(1.0).unwrap(),
        }],
        ..Session::new()
    }
}

#[test]
fn valid_session_round_trips_as_versioned_serde_data() {
    let session = valid_session();
    session.validate().unwrap();

    let encoded = serde_json::to_string(&session).unwrap();
    assert!(encoded.contains("\"version\":1"));
    let decoded: Session = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, session);
    decoded.validate().unwrap();
}

#[test]
fn rejects_capacity_before_following_invalid_references() {
    let mut session = Session::new();
    session.racks = (0..=MAX_RACKS)
        .map(|index| Rack {
            id: RackId(format!("rack-{index}")),
            name: String::new(),
            source_id: SourceId("missing".into()),
            endpoint_id: EndpointId("missing".into()),
            topology: RackTopology::default(),
            slots: Vec::new(),
        })
        .collect();

    assert_eq!(
        session.validate(),
        Err(ValidationError::CapacityExceeded {
            collection: sp_model::Collection::Racks,
            capacity: MAX_RACKS,
            found: MAX_RACKS + 1,
        })
    );
}

#[test]
fn rejects_scene_parameter_that_is_not_declared_by_its_slot() {
    let mut session = valid_session();
    session.scenes[0].parameter_values[0].parameter_id = ParameterId("missing".into());

    assert!(matches!(
        session.validate(),
        Err(ValidationError::UnknownReference {
            reference: sp_model::EntityKind::Parameter,
            ..
        })
    ));
}
