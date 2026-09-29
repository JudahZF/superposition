//! Integration tests for versioned model contracts.

use std::collections::BTreeMap;

use sp_model::{
    ChannelLayout, Endpoint, EndpointId, GainDb, MAX_RACKS, MidiController, MidiMapping,
    MidiMappingId, NormalizedParameters, NormalizedValue, PageId, ParameterAddress, ParameterId,
    PhysicalChannels, PluginArchitecture, PluginBusConfiguration, PluginBusMetadata,
    PluginClassScanMetadata, PluginDescriptor, PluginFingerprint, PluginIdentity, PluginInstanceId,
    PluginParameterMetadata, PluginScanMetadata, PluginScanOutcome, PluginSlot, Rack, RackBypass,
    RackGain, RackId, RackMute, RackPage, RackTopology, Scene, SceneId, SceneParameterTransition,
    SceneParameterValue, Session, SlotBypass, SlotSidechain, Source, SourceId, ValidationError,
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
            gain_db: sp_model::GainDb::default(),
            muted: false,
            bypassed: false,
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
                bypassed: false,
                parameters: NormalizedParameters { values: parameters },
                sidechain: None,
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
            rack_bypasses: vec![RackBypass {
                rack_id: RackId("rack-a".into()),
                bypassed: false,
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
                transition: SceneParameterTransition::Ramp,
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
fn legacy_scene_and_scan_metadata_default_new_fields() {
    let session = valid_session();
    let mut encoded = serde_json::to_value(&session).unwrap();
    let scene = &mut encoded["scenes"][0];
    scene.as_object_mut().unwrap().remove("rack_bypasses");
    scene["parameter_values"][0]
        .as_object_mut()
        .unwrap()
        .remove("transition");
    let decoded: Session = serde_json::from_value(encoded).unwrap();
    assert!(decoded.scenes[0].rack_bypasses.is_empty());
    assert_eq!(
        decoded.scenes[0].parameter_values[0].transition,
        SceneParameterTransition::Ramp
    );
    decoded.validate().unwrap();

    let old_parameter = serde_json::json!({
        "id": 74,
        "name": "Cutoff",
        "default_normalized": 0.5
    });
    let decoded: PluginParameterMetadata = serde_json::from_value(old_parameter).unwrap();
    assert_eq!(decoded.step_count, 0);
}

#[test]
fn sidechain_sources_are_validated_and_round_trip() {
    let mut session = valid_session();
    let mut drums = session.racks[0].clone();
    drums.id = RackId("rack-drums".into());
    session.racks.push(drums);
    let slot_id = || "slot-a".to_owned();
    let mut set = |sidechain| {
        session.racks[0].slots[0].sidechain = Some(sidechain);
        session.clone()
    };

    let from_rack = set(SlotSidechain::RackOutput(RackId("rack-drums".into())));
    from_rack.validate().unwrap();
    let encoded = serde_json::to_value(&from_rack).unwrap();
    assert_eq!(
        encoded["racks"][0]["slots"][0]["sidechain"],
        serde_json::json!({ "rack_output": "rack-drums" })
    );
    assert!(encoded["racks"][1]["slots"][0].get("sidechain").is_none());
    assert_eq!(
        serde_json::from_value::<Session>(encoded).unwrap(),
        from_rack
    );

    let from_input = set(SlotSidechain::PhysicalInput(PhysicalChannels::Stereo {
        left: 2,
        right: 3,
    }));
    from_input.validate().unwrap();
    assert_eq!(
        serde_json::to_value(&from_input).unwrap()["racks"][0]["slots"][0]["sidechain"],
        serde_json::json!({ "physical_input": { "kind": "stereo", "left": 2, "right": 3 } })
    );

    assert_eq!(
        set(SlotSidechain::PhysicalInput(PhysicalChannels::Mono {
            channel: 2
        }))
        .validate(),
        Err(ValidationError::MonoSidechain { slot_id: slot_id() })
    );
    assert_eq!(
        set(SlotSidechain::RackOutput(RackId("rack-a".into()))).validate(),
        Err(ValidationError::SelfSidechain { slot_id: slot_id() })
    );
    assert!(matches!(
        set(SlotSidechain::RackOutput(RackId("missing".into()))).validate(),
        Err(ValidationError::UnknownReference {
            owner: sp_model::EntityKind::PluginSlot,
            reference: sp_model::EntityKind::Rack,
            ..
        })
    ));
}

#[test]
fn rack_pages_reference_known_racks_once_and_are_omitted_when_empty() {
    let mut session = valid_session();
    assert!(
        serde_json::to_value(&session)
            .unwrap()
            .get("pages")
            .is_none(),
        "sessions without pages keep their JSON shape"
    );
    let page = |racks: &[&str]| RackPage {
        id: PageId("page-1".into()),
        name: "Drums".into(),
        racks: racks.iter().map(|rack| RackId((*rack).into())).collect(),
    };
    session.pages = vec![page(&["rack-a"])];
    session.validate().unwrap();
    let encoded = serde_json::to_value(&session).unwrap();
    assert_eq!(serde_json::from_value::<Session>(encoded).unwrap(), session);

    session.pages = vec![page(&["missing"])];
    assert!(matches!(
        session.validate(),
        Err(ValidationError::UnknownReference {
            owner: sp_model::EntityKind::Page,
            reference: sp_model::EntityKind::Rack,
            ..
        })
    ));
    session.pages = vec![page(&["rack-a", "rack-a"])];
    assert!(matches!(
        session.validate(),
        Err(ValidationError::DuplicateId { .. })
    ));
    session.pages = vec![page(&[]), page(&[])];
    assert!(matches!(
        session.validate(),
        Err(ValidationError::DuplicateId {
            kind: sp_model::EntityKind::Page,
            ..
        })
    ));
}

#[test]
fn scene_rack_bypass_requires_one_known_target_per_rack() {
    let mut session = valid_session();
    session.scenes[0].rack_bypasses[0].rack_id = RackId("missing".into());
    assert!(matches!(
        session.validate(),
        Err(ValidationError::UnknownReference {
            reference: sp_model::EntityKind::Rack,
            ..
        })
    ));

    session.scenes[0].rack_bypasses[0].rack_id = RackId("rack-a".into());
    let bypass = session.scenes[0].rack_bypasses[0].clone();
    session.scenes[0].rack_bypasses.push(bypass.clone());
    assert_eq!(
        session.validate(),
        Err(ValidationError::DuplicateTarget {
            kind: sp_model::EntityKind::Scene,
            id: "rack-a".into(),
        })
    );

    session.scenes[0].rack_bypasses = vec![bypass; MAX_RACKS + 1];
    assert_eq!(
        session.validate(),
        Err(ValidationError::CapacityExceeded {
            collection: sp_model::Collection::SceneRackBypasses,
            capacity: MAX_RACKS,
            found: MAX_RACKS + 1,
        })
    );
}

#[test]
fn step_transition_round_trips() {
    let mut session = valid_session();
    session.scenes[0].parameter_values[0].transition = SceneParameterTransition::Step;
    let encoded = serde_json::to_value(&session).unwrap();
    assert_eq!(
        encoded["scenes"][0]["parameter_values"][0]["transition"],
        "step"
    );
    let decoded: Session = serde_json::from_value(encoded).unwrap();
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
            gain_db: sp_model::GainDb::default(),
            muted: false,
            bypassed: false,
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
fn rejects_rack_slot_capacity_before_duplicate_slot_ids() {
    let mut session = valid_session();
    session.racks[0].slots =
        vec![session.racks[0].slots[0].clone(); sp_model::MAX_SLOTS_PER_RACK + 1];

    assert_eq!(
        session.validate(),
        Err(ValidationError::CapacityExceeded {
            collection: sp_model::Collection::RackSlots,
            capacity: sp_model::MAX_SLOTS_PER_RACK,
            found: sp_model::MAX_SLOTS_PER_RACK + 1,
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

#[test]
fn persisted_sessions_require_an_explicit_supported_version() {
    let encoded = serde_json::json!({
        "sources": [],
        "endpoints": [],
        "racks": [],
        "scenes": [],
        "midi_mappings": []
    });

    assert!(serde_json::from_value::<Session>(encoded).is_err());

    let mut unsupported = valid_session();
    unsupported.version = 2;
    assert!(matches!(
        unsupported.validate(),
        Err(ValidationError::UnsupportedVersion {
            found: 2,
            supported: 1
        })
    ));
}

#[test]
fn alpha_validation_rejects_persisted_parallel_and_discrete_topologies() {
    let mut parallel = valid_session();
    parallel.racks[0].topology = RackTopology::Parallel;
    assert!(matches!(
        parallel.validate_for_alpha(),
        Err(ValidationError::UnsupportedRackTopology {
            topology: RackTopology::Parallel
        })
    ));

    let mut discrete = valid_session();
    discrete.sources[0].layout = ChannelLayout::Discrete { channels: 2 };
    assert!(matches!(
        discrete.validate_for_alpha(),
        Err(ValidationError::UnsupportedChannelLayout {
            layout: ChannelLayout::Discrete { channels: 2 }
        })
    ));
}

#[test]
fn alpha_validation_rejects_unsupported_channel_conversion() {
    let mut session = valid_session();
    session.endpoints[0].layout = ChannelLayout::Mono;

    assert!(matches!(
        session.validate_for_alpha(),
        Err(ValidationError::MismatchedRackLayouts {
            source_layout: ChannelLayout::Stereo,
            endpoint_layout: ChannelLayout::Mono,
            ..
        })
    ));
}

#[test]
fn scan_metadata_round_trips_without_vst3_sdk_types() {
    let metadata = PluginScanMetadata {
        version: sp_model::PLUGIN_SCAN_METADATA_VERSION,
        architecture: PluginArchitecture::Universal,
        outcome: PluginScanOutcome::Supported,
        classes: vec![PluginClassScanMetadata {
            identity: PluginIdentity {
                vendor: "Acme".into(),
                name: "Filter".into(),
                unique_id: "acme.filter".into(),
            },
            version: "1.2.3".into(),
            buses: PluginBusConfiguration {
                inputs: vec![PluginBusMetadata {
                    index: 0,
                    channels: 2,
                    main: true,
                    event: false,
                }],
                outputs: vec![PluginBusMetadata {
                    index: 0,
                    channels: 2,
                    main: true,
                    event: false,
                }],
            },
            parameters: vec![PluginParameterMetadata {
                id: 74,
                name: "Cutoff".into(),
                short_name: "Cut".into(),
                unit: "Hz".into(),
                default_normalized: Some(0.5),
                step_count: 0,
                automatable: true,
                read_only: false,
                bypass: false,
            }],
            editor_supported: true,
            sidechain_capable: true,
        }],
        detail: None,
    };

    let encoded = serde_json::to_string(&metadata).expect("serialize scan metadata");
    let decoded: PluginScanMetadata = serde_json::from_str(&encoded).expect("deserialize metadata");
    assert_eq!(decoded, metadata);
}
