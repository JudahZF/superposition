//! Superposition's Tunnel adapter in a simulated Tunnel system: a test-pattern device feeds
//! "Superposition inputs", a passthrough renderer runs, and "Superposition outputs" feeds a
//! recorder device.
//!
//! `SimSystem` runs every device on one thread in virtual time, so the render stage cannot run
//! on its own real-time thread: it would fall behind the simulated clock. Here the device's app
//! is [`InlineApp`], which runs the render stage inside the cycle. The network side, the rings,
//! the lead-in and the added latency are the same as in production.
//!
//! This binary sets an allocator that aborts the process if the adapter or the renderer
//! allocates or frees inside a cycle.

use std::num::NonZeroU16;
use std::time::Duration;

use assert_no_alloc::{AllocDisabler, assert_no_alloc};
use sp_audio_io::{BlockFrames, ProductRenderer};
use sp_audio_io_tunnel::{Endpoints, InlineApp, SessionFormat, TunnelChannels};
use sp_engine::PreparedGraph;
use sp_model::{
    ChannelLayout, Endpoint, PhysicalChannels, Rack, RackChannelRoute, RackId, RackTopology,
    Session, Source, SourceId,
};
use tunnel_device::{Client, ClientError, DeviceIndex, Pending, SimDeviceSpec, SimSystem};
use tunnel_host::audio::apps::{RecordedBlock, Recorder, TestPattern};
use tunnel_host::audio::{AudioApp, AudioBuffers, AudioLayout, Block};
use tunnel_types::{
    ClockState, EndpointDecl, EndpointId, EndpointKind, EndpointRecord, ErrorCode, LatencyClass,
    MediaTime, QueryKind, RouteOp, RouteOpKind, RouteRange, SampleRate, SystemConfig, TxnReply,
};

#[global_allocator]
static ALLOCATOR: AllocDisabler = AllocDisabler;

/// The routed channels: 32 stereo racks.
const CHANNELS: u16 = 64;

const PATTERN: EndpointId = EndpointId([0x50; 16]);
const RECORD: EndpointId = EndpointId([0x52; 16]);

/// Long enough for any single request.
const REQUEST: Duration = Duration::from_secs(15);

/// Long enough for PTP to lock and every device to register.
const SETTLE: Duration = Duration::from_secs(30);

/// The Superposition device's app, with allocation checked in each cycle.
struct Checked(InlineApp);

impl AudioApp for Checked {
    fn process(&mut self, block: &mut Block<'_>) {
        assert_no_alloc(|| self.0.process(block));
    }
}

fn endpoint(id: EndpointId, kind: EndpointKind) -> EndpointDecl {
    EndpointDecl {
        id,
        kind,
        channel_count: CHANNELS,
        label: format!("{kind:?}"),
        channel_labels: None,
    }
}

fn device(
    sim: &mut SimSystem,
    n: u8,
    endpoints: Vec<EndpointDecl>,
    app: Box<dyn AudioApp>,
) -> DeviceIndex {
    let mut spec = SimDeviceSpec::host(&format!("device {n}"), [10, 0, 0, n]);
    spec.config.endpoints = endpoints;
    spec.app = app;
    sim.add_device(spec).unwrap()
}

/// Inputs `2k` and `2k + 1` through bypassed stereo rack `k` to outputs `2k` and `2k + 1`, at
/// unity gain: no workers, a bit-exact dry path.
fn passthrough(rate: SampleRate) -> ProductRenderer {
    let mut session = Session::new();
    session.sources.push(Source {
        id: SourceId("in".into()),
        name: "In".into(),
        layout: ChannelLayout::Stereo,
    });
    session.endpoints.push(Endpoint {
        id: sp_model::EndpointId("out".into()),
        name: "Out".into(),
        layout: ChannelLayout::Stereo,
    });
    for pair in 0..u8::try_from(CHANNELS / 2).unwrap() {
        let id = RackId(format!("rack-{pair}"));
        session.racks.push(Rack {
            id: id.clone(),
            name: format!("Rack {pair}"),
            topology: RackTopology::Serial,
            source_id: SourceId("in".into()),
            endpoint_id: sp_model::EndpointId("out".into()),
            gain_db: sp_model::GainDb::default(),
            muted: false,
            bypassed: false,
            slots: Vec::new(),
        });
        let channels = PhysicalChannels::Stereo {
            left: 2 * pair,
            right: 2 * pair + 1,
        };
        session.rack_routes.insert(
            id,
            RackChannelRoute {
                input: Some(channels),
                output: channels,
            },
        );
    }
    let mut renderer = ProductRenderer::new(PreparedGraph::compile(&session).unwrap())
        .with_sample_rate(rate.hz())
        .unwrap();
    for rack in 0..usize::from(CHANNELS / 2) {
        renderer.mixer_mut().set_rack_bypassed(rack, true);
    }
    renderer
}

/// Creates a system on `master`, adopts `others` and runs until each is locked and
/// registered.
fn system(sim: &mut SimSystem, rate: SampleRate, master: DeviceIndex, others: &[DeviceIndex]) {
    sim.create_system(master, rate).unwrap();
    for &other in others {
        sim.adopt(master, other).unwrap();
    }
    let settled = sim.run_until(SETTLE, |sim| {
        sim.status(master).master
            && others.iter().all(|&device| {
                let status = sim.status(device);
                status.registered && status.clock == Some(ClockState::Locked)
            })
    });
    assert!(settled, "the system did not settle");
}

/// Runs until the master has made all four endpoints active.
fn wait_for_endpoints(sim: &mut SimSystem, client: &Client) {
    for _ in 0..40 {
        let reply = sim
            .wait(client.query(QueryKind::Endpoints, None), REQUEST)
            .unwrap();
        let records: Vec<EndpointRecord> = tunnel_types::cbor::decode(&reply.encode()).unwrap();
        if records.len() == 4 && records.iter().all(|r| r.active_host.is_some()) {
            return;
        }
        sim.run_for(Duration::from_millis(500));
    }
    panic!("the endpoints did not become active");
}

/// Routes `count` channels from `source` to `sink`, again while the master is busy.
fn route(sim: &mut SimSystem, client: &Client, source: EndpointId, sink: EndpointId) -> TxnReply {
    let request = || -> Pending<TxnReply> {
        client.route(vec![RouteOp {
            kind: RouteOpKind::Add,
            range: RouteRange {
                source,
                source_first: 0,
                sink,
                sink_first: 0,
                count: NonZeroU16::new(CHANNELS).unwrap(),
            },
        }])
    };
    for _ in 0..20 {
        match sim.wait(request(), REQUEST) {
            Ok(reply) => return reply,
            Err(ClientError::Rejected(error)) if error.code.effective() == ErrorCode::Busy => {
                sim.run_for(Duration::from_millis(500));
            }
            Err(error) => panic!("route failed: {error}"),
        }
    }
    panic!("the master stayed busy");
}

/// The test pattern of all channels for the `frames` media times from `time`, channel after
/// channel.
fn pattern(time: u64, frames: usize) -> Vec<i32> {
    let decls = [endpoint(PATTERN, EndpointKind::Source)];
    let layout = AudioLayout::new(&decls);
    let mut buffers = AudioBuffers::new(&layout);
    TestPattern::new(0, None).process(&mut buffers.block(&layout, MediaTime(time), frames));
    (0..usize::from(CHANNELS))
        .flat_map(|channel| buffers.source(channel).unwrap()[..frames].to_vec())
        .collect()
}

/// Checks every recorded sample: zero before media time `first`, then the pattern `total`
/// samples earlier. Returns how many samples were at or after `first`.
fn check(blocks: &[RecordedBlock], first: u64, total: u64) -> u64 {
    let mut checked = 0;
    for block in blocks {
        let frames = block.samples.len() / usize::from(CHANNELS);
        let expected = pattern(block.time.0 - total, frames);
        for (index, (&sample, &want)) in block.samples.iter().zip(&expected).enumerate() {
            let time = block.time.0 + u64::try_from(index % frames).unwrap();
            let channel = index / frames;
            let want = if time < first { 0 } else { want };
            assert_eq!(sample, want, "channel {channel} at {time}");
            checked += u64::from(time >= first);
        }
    }
    checked
}

/// Plan section 05 phase 3: the samples of all 64 routed channels pass the Tunnel flow into
/// "Superposition inputs", a passthrough renderer and the flow from "Superposition outputs"
/// bit-exact, and arrive at the recorder after the two flow latencies plus the session's
/// added latency. No block underruns or overruns, and nothing allocates in a cycle.
fn passes_bit_exact_at_the_added_latency(rate: SampleRate, block: BlockFrames) {
    let endpoints = Endpoints::generate();
    let channels = TunnelChannels {
        inputs: CHANNELS,
        outputs: CHANNELS,
    };
    let format = SessionFormat {
        block,
        sample_rate_hz: rate.hz(),
        inputs: CHANNELS,
        outputs: CHANNELS,
    };
    let app = InlineApp::new(Box::new(passthrough(rate)), format);
    let telemetry = app.telemetry();

    let mut sim = SimSystem::new(7);
    let source = device(
        &mut sim,
        1,
        vec![endpoint(PATTERN, EndpointKind::Source)],
        Box::new(TestPattern::new(0, None)),
    );
    let superposition = device(
        &mut sim,
        2,
        endpoints.decls(channels),
        Box::new(Checked(app)),
    );
    let (recorder, mut recording) = Recorder::new(0, CHANNELS, 4_000);
    let sink = device(
        &mut sim,
        3,
        vec![endpoint(RECORD, EndpointKind::Sink)],
        Box::new(recorder),
    );
    system(&mut sim, rate, source, &[superposition, sink]);

    let client = sim.device_client(superposition).unwrap();
    wait_for_endpoints(&mut sim, &client);
    let into = route(&mut sim, &client, PATTERN, endpoints.inputs);
    let from = route(&mut sim, &client, endpoints.outputs, RECORD);

    recording.take();
    let mut blocks = Vec::new();
    for _ in 0..20 {
        sim.run_for(Duration::from_millis(100));
        blocks.extend(recording.take());
    }

    let counters = telemetry.counters();
    assert!(counters.rendered > 0);
    assert_eq!(
        (counters.silenced, counters.underruns, counters.overruns),
        (0, 0, 0)
    );

    let flow = u64::from(SystemConfig::defaults(rate).latency(LatencyClass::Host));
    let added = u64::from(format.latency().frames);
    let total = 2 * flow + added;
    // The recorder presents Superposition output m at m + flow; that output is the input that
    // Superposition was presented at m - added.
    let first = (from.at.0 + flow).max(into.at.0 + total);
    let checked = check(&blocks, first, total);
    // About 1.5 s of all channels.
    assert!(
        checked > u64::from(rate.hz()) * u64::from(CHANNELS),
        "{checked} samples checked"
    );
    println!(
        "{} Hz, {}-frame blocks: added {added} frames ({:?}), recorder total {total} frames",
        rate.hz(),
        block.as_u32(),
        format.latency().duration(),
    );
}

#[test]
fn bit_exact_at_96_khz_with_64_frame_blocks() {
    passes_bit_exact_at_the_added_latency(SampleRate::Hz96000, BlockFrames::Frames64);
}

#[test]
fn bit_exact_at_96_khz_with_128_frame_blocks() {
    passes_bit_exact_at_the_added_latency(SampleRate::Hz96000, BlockFrames::Frames128);
}

#[test]
fn bit_exact_at_48_khz_with_64_frame_blocks() {
    passes_bit_exact_at_the_added_latency(SampleRate::Hz48000, BlockFrames::Frames64);
}

#[test]
fn bit_exact_at_48_khz_with_128_frame_blocks() {
    passes_bit_exact_at_the_added_latency(SampleRate::Hz48000, BlockFrames::Frames128);
}
