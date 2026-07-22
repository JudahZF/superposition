//! Integration tests for prepared graph contracts.

use std::collections::BTreeMap;

use sp_engine::{
    FallbackReason, GateOutcome, GraphArena, PrepareError, PreparedChannelLayout, PreparedGraph,
    RackGate, RackGateState, WorkerObservation,
};
use sp_model::{
    ChannelLayout, Endpoint, EndpointId, NormalizedParameters, NormalizedValue, ParameterId,
    PluginDescriptor, PluginFingerprint, PluginIdentity, PluginInstanceId, PluginSlot, Rack,
    RackId, RackTopology, Session, Source, SourceId,
};
use sp_protocol::{BlockTicket, ProtocolError};

fn valid_session() -> Session {
    let mut parameters = BTreeMap::new();
    parameters.insert(
        ParameterId("gain".into()),
        NormalizedValue::new(0.4).unwrap(),
    );
    Session {
        sources: vec![Source {
            id: SourceId("source".into()),
            name: "Source".into(),
            layout: ChannelLayout::Stereo,
        }],
        endpoints: vec![Endpoint {
            id: EndpointId("endpoint".into()),
            name: "Endpoint".into(),
            layout: ChannelLayout::Stereo,
        }],
        racks: vec![Rack {
            id: RackId("rack".into()),
            name: "Rack".into(),
            source_id: SourceId("source".into()),
            endpoint_id: EndpointId("endpoint".into()),
            topology: RackTopology::Serial,
            gain_db: sp_model::GainDb::default(),
            muted: false,
            bypassed: false,
            slots: vec![PluginSlot {
                id: PluginInstanceId("slot".into()),
                plugin: PluginDescriptor {
                    identity: PluginIdentity {
                        vendor: "Acme".into(),
                        name: "Gain".into(),
                        unique_id: "acme.gain".into(),
                    },
                    fingerprint: PluginFingerprint {
                        algorithm: "sha256".into(),
                        digest: "abc123".into(),
                        plugin_version: "1.0".into(),
                    },
                },
                bypassed: false,
                parameters: NormalizedParameters { values: parameters },
            }],
        }],
        ..Session::new()
    }
}

#[test]
fn compiles_fixed_capacity_serial_graph() {
    let graph = PreparedGraph::compile(&valid_session()).unwrap();
    assert_eq!(graph.rack_count(), 1);
    let rack = graph.rack(0).unwrap();
    assert_eq!(rack.layout(), PreparedChannelLayout::Stereo);
    assert_eq!(rack.slot_count(), 1);
    assert_eq!(rack.slot(0).unwrap().parameter_count(), 1);
}

#[test]
fn rejects_topology_and_layout_outside_phase_zero_support() {
    let mut parallel = valid_session();
    parallel.racks[0].topology = RackTopology::Parallel;
    assert!(matches!(
        PreparedGraph::compile(&parallel),
        Err(PrepareError::InvalidSession(_))
    ));

    let mut surround = valid_session();
    surround.sources[0].layout = ChannelLayout::Discrete { channels: 6 };
    surround.endpoints[0].layout = ChannelLayout::Discrete { channels: 6 };
    assert!(matches!(
        PreparedGraph::compile(&surround),
        Err(PrepareError::InvalidSession(_))
    ));
}

#[test]
fn rejects_unreferenced_layouts_outside_alpha_support() {
    let mut session = valid_session();
    session.sources.push(Source {
        id: SourceId("surround-source".into()),
        name: "Surround source".into(),
        layout: ChannelLayout::Discrete { channels: 6 },
    });
    session.endpoints.push(Endpoint {
        id: EndpointId("surround-endpoint".into()),
        name: "Surround endpoint".into(),
        layout: ChannelLayout::Discrete { channels: 6 },
    });

    assert!(matches!(
        PreparedGraph::compile(&session),
        Err(PrepareError::InvalidSession(_))
    ));
}

#[test]
fn audio_thread_acknowledges_only_block_boundary_swaps() {
    let initial = PreparedGraph::compile(&valid_session()).unwrap();
    let mut arena = GraphArena::new(initial);
    let request = arena.stage(PreparedGraph::empty());

    let mut audio_thread = arena.activate_audio_thread();
    let first_block = audio_thread.activate_block();
    assert_eq!(
        first_block.acknowledgement().unwrap().generation,
        request.generation
    );
    assert_eq!(first_block.graph().rack_count(), 0);

    let second_block = audio_thread.activate_block();
    assert!(second_block.acknowledgement().is_none());
    assert_eq!(second_block.active_generation(), request.generation);
}

#[test]
fn rack_gate_accepts_one_matching_completion_then_reopens() {
    let mut gate = RackGate::new();
    let first_ticket = BlockTicket {
        generation: 3,
        sequence: 1,
    };
    let second_ticket = BlockTicket {
        generation: 3,
        sequence: 2,
    };

    assert_eq!(
        gate.dispatch(first_ticket, 10),
        GateOutcome::DispatchAllowed
    );
    assert_eq!(
        gate.state(),
        RackGateState::Awaiting {
            ticket: first_ticket,
            dispatch_block: 10,
        }
    );
    assert_eq!(
        gate.observe(WorkerObservation::Pending, 11),
        GateOutcome::Awaiting
    );
    assert_eq!(
        gate.observe(WorkerObservation::Completed(first_ticket), 11),
        GateOutcome::WorkerResultAccepted
    );
    assert_eq!(gate.state(), RackGateState::Open);
    assert_eq!(
        gate.dispatch(second_ticket, 12),
        GateOutcome::DispatchAllowed
    );
}

#[test]
fn rack_gate_closes_after_three_consecutive_deadline_misses() {
    let mut gate = RackGate::new();
    for sequence in 1..=3 {
        let ticket = BlockTicket {
            generation: 3,
            sequence,
        };
        assert_eq!(
            gate.dispatch(ticket, 19 + sequence),
            GateOutcome::DispatchAllowed
        );
        assert_eq!(
            gate.deadline_expired(20 + sequence),
            GateOutcome::UseFallback(FallbackReason::DeadlineMiss)
        );
        if sequence < 3 {
            assert_eq!(gate.state(), RackGateState::Open);
        }
    }
    assert_eq!(
        gate.state(),
        RackGateState::Closed {
            reason: FallbackReason::DeadlineMiss,
            closed_block: 23,
        }
    );
    assert_eq!(
        gate.dispatch(
            BlockTicket {
                generation: 3,
                sequence: 4,
            },
            24,
        ),
        GateOutcome::UseFallback(FallbackReason::DeadlineMiss)
    );
}

#[test]
fn rack_gate_success_resets_deadline_miss_streak() {
    let mut gate = RackGate::new();
    for sequence in 1..=2 {
        let ticket = BlockTicket {
            generation: 8,
            sequence,
        };
        assert_eq!(
            gate.dispatch(ticket, sequence),
            GateOutcome::DispatchAllowed
        );
        assert_eq!(
            gate.deadline_expired(sequence),
            GateOutcome::UseFallback(FallbackReason::DeadlineMiss)
        );
    }
    let success = BlockTicket {
        generation: 8,
        sequence: 3,
    };
    assert_eq!(gate.dispatch(success, 3), GateOutcome::DispatchAllowed);
    assert_eq!(
        gate.observe(WorkerObservation::Completed(success), 3),
        GateOutcome::WorkerResultAccepted
    );
    let next = BlockTicket {
        generation: 8,
        sequence: 4,
    };
    assert_eq!(gate.dispatch(next, 4), GateOutcome::DispatchAllowed);
    assert_eq!(
        gate.deadline_expired(4),
        GateOutcome::UseFallback(FallbackReason::DeadlineMiss)
    );
    assert_eq!(gate.state(), RackGateState::Open);
}

#[test]
fn rack_gate_faults_stay_local_until_explicit_replacement() {
    let mut gate = RackGate::new();
    let ticket = BlockTicket {
        generation: 4,
        sequence: 1,
    };

    assert_eq!(gate.dispatch(ticket, 30), GateOutcome::DispatchAllowed);
    assert_eq!(
        gate.observe(
            WorkerObservation::ProtocolFault(ProtocolError::MalformedCompletion),
            31,
        ),
        GateOutcome::UseFallback(FallbackReason::MalformedCompletion)
    );
    assert_eq!(
        gate.worker_lost(32),
        GateOutcome::UseFallback(FallbackReason::MalformedCompletion)
    );

    gate.reset_after_replacement();
    assert_eq!(gate.state(), RackGateState::Open);
    assert_eq!(
        gate.worker_lost(33),
        GateOutcome::UseFallback(FallbackReason::WorkerExited)
    );
}
