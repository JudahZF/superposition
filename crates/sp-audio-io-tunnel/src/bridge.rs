//! The adapter between the Tunnel network thread and the product renderer.
//!
//! [`NetworkSide`] runs inside the Tunnel cycle (12 or 24 frames). It only converts samples and
//! moves them through two preallocated SPSC rings. [`RenderStage`] takes whole product blocks
//! (32–256 frames) from the input ring, runs the [`ProductRenderer`] once per block, and pushes
//! the output. Production runs the stage on its own thread; [`InlineApp`] runs it inside the
//! cycle for the virtual-time simulator.
//!
//! The network side plays [`AddedLatency`] frames of silence before it reads the output ring,
//! so output frame `j` is always input frame `j - latency`. Late output does not move that
//! alignment: the missing frames play as silence and are discarded when they come. An input
//! ring without room starts a resync: both rings are emptied and the lead-in plays again.

use std::sync::Arc;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::thread::Thread;

use rtrb::{Consumer, Producer, RingBuffer};
use sp_audio_io::{
    BlockFrames, CallbackTelemetry, InterleavedStereoF32, MultiChannelDuplexF32,
    MultiChannelDuplexRenderer, OutputRenderer, ProductRenderer, RenderDisposition,
};
use tunnel_host::audio::{AudioApp, Block, MAX_FRAMES, from_f32, to_f32};

use crate::{AddedLatency, INPUTS, OUTPUTS};

/// The shape of one session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionFormat {
    /// The product block: the renderer runs once per this many frames.
    pub block: BlockFrames,
    /// The session rate. It must be the Tunnel system rate.
    pub sample_rate_hz: u32,
    /// Channels of "Superposition inputs" that the renderer reads, from channel 0. Zero for an
    /// output-only route.
    pub inputs: u16,
    /// Channels of "Superposition outputs" that the renderer writes, from channel 0. At least 1.
    pub outputs: u16,
}

impl SessionFormat {
    /// The latency that this session adds.
    pub const fn latency(self) -> AddedLatency {
        AddedLatency::new(self.block, self.sample_rate_hz)
    }

    const fn frames(self) -> usize {
        self.block.as_u32() as usize
    }

    /// Samples per frame in the input ring. An output-only route still sends one silent
    /// sample per frame: the render stage counts frames from it.
    fn input_stride(self) -> usize {
        usize::from(self.inputs.max(1))
    }
}

/// Counters of one session, from its start.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SessionCounters {
    /// Product blocks that the renderer filled.
    pub rendered: u64,
    /// Product blocks that the renderer left silent.
    pub silenced: u64,
    /// Tunnel cycles that played some silence because the render stage was late.
    pub underruns: u64,
    /// Tunnel cycles whose input did not fit the input ring. Each starts a resync.
    pub overruns: u64,
}

impl SessionCounters {
    /// Maps the counters onto the backend-neutral callback telemetry. A callback is one product
    /// block (`rendered` or `silenced`, in the histogram bucket of `block`) or one Tunnel cycle
    /// with an underrun or overrun, which counts as `invalid_bytes`: a buffer that could not be
    /// exchanged whole. The snapshot is always coherent.
    pub fn callback_telemetry(self, block: BlockFrames) -> CallbackTelemetry {
        let blocks = self.rendered.wrapping_add(self.silenced);
        let xruns = self.underruns.wrapping_add(self.overruns);
        let bucket = |frames: BlockFrames| if frames == block { blocks } else { 0 };
        CallbackTelemetry {
            callbacks: blocks.wrapping_add(xruns),
            rendered: self.rendered,
            silenced: self.silenced,
            invalid_bytes: xruns,
            frame_histogram_32: bucket(BlockFrames::Frames32),
            frame_histogram_64: bucket(BlockFrames::Frames64),
            frame_histogram_128: bucket(BlockFrames::Frames128),
            frame_histogram_256: bucket(BlockFrames::Frames256),
            ..CallbackTelemetry::default()
        }
    }
}

/// State shared by the two halves of a session and the control thread.
#[derive(Debug, Default)]
pub(crate) struct Shared {
    /// The resync epoch that the network side asks for.
    resync_request: AtomicU64,
    /// The last epoch whose input the render stage has dropped.
    resync_ack: AtomicU64,
    /// The network side writes silence but keeps the rings moving.
    pub(crate) muted: AtomicBool,
    /// Ends the render thread.
    pub(crate) stop: AtomicBool,
    rendered: AtomicU64,
    silenced: AtomicU64,
    underruns: AtomicU64,
    overruns: AtomicU64,
}

/// Reads the counters of a running session from any thread.
#[derive(Clone, Debug)]
pub struct SessionTelemetry(Arc<Shared>);

impl SessionTelemetry {
    /// The counters now. Each counter has one writer, so a read never blocks.
    pub fn counters(&self) -> SessionCounters {
        SessionCounters {
            rendered: self.0.rendered.load(Relaxed),
            silenced: self.0.silenced.load(Relaxed),
            underruns: self.0.underruns.load(Relaxed),
            overruns: self.0.overruns.load(Relaxed),
        }
    }

    pub(crate) fn shared(&self) -> &Shared {
        &self.0
    }
}

/// Adds one to a counter that only the calling thread writes.
fn bump(counter: &AtomicU64) {
    counter.store(counter.load(Relaxed).wrapping_add(1), Relaxed);
}

/// The network-thread half: Tunnel buffers to and from the rings.
pub(crate) struct NetworkSide {
    input: Producer<f32>,
    output: Consumer<f32>,
    format: SessionFormat,
    /// Output frames still to play as silence before the first rendered frame.
    lead_in: usize,
    /// Output frames that played as silence and must be discarded when they come.
    owed: usize,
    /// Input frames pushed since the last complete product block.
    since_block: usize,
    /// The resync epoch: the network side waits for its acknowledgement while it is ahead.
    epoch: u64,
    resyncing: bool,
    /// One cycle of interleaved samples, for whichever direction is wider.
    interleaved: Box<[f32]>,
    shared: Arc<Shared>,
    /// The render thread to wake; `None` when the stage runs inline.
    render: Option<Thread>,
}

impl std::fmt::Debug for NetworkSide {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetworkSide")
            .field("format", &self.format)
            .finish_non_exhaustive()
    }
}

impl NetworkSide {
    /// Runs one Tunnel cycle and wakes the render thread when it has work.
    ///
    /// Real-time: no allocation, no free, no lock, no wait. Waking is a futex call only when
    /// the render thread sleeps.
    pub(crate) fn process(&mut self, block: &mut Block<'_>) {
        if self.receive(block)
            && let Some(render) = &self.render
        {
            render.unpark();
        }
        self.send(block);
    }

    pub(crate) fn set_render_thread(&mut self, thread: Thread) {
        self.render = Some(thread);
    }

    /// Pushes the cycle's input frames. Returns whether the render stage has new work: a
    /// complete product block or a resync request.
    fn receive(&mut self, block: &Block<'_>) -> bool {
        if self.resyncing {
            if self.shared.resync_ack.load(Acquire) != self.epoch {
                return false;
            }
            // The render stage dropped every older input and renders nothing until new input
            // comes, so the output ring holds only stale frames.
            let stale = self.output.slots();
            discard(&mut self.output, stale);
            self.resyncing = false;
            self.lead_in = self.format.latency().frames as usize;
            self.owed = 0;
            self.since_block = 0;
        }
        let frames = block.frames();
        let stride = self.format.input_stride();
        let samples = frames * stride;
        if self.input.slots() < samples {
            bump(&self.shared.overruns);
            self.epoch = self.epoch.wrapping_add(1);
            self.shared.resync_request.store(self.epoch, Release);
            self.resyncing = true;
            return true;
        }
        let interleaved = &mut self.interleaved[..samples];
        interleaved.fill(0.0);
        let mut planar = [0.0; MAX_FRAMES];
        for channel in 0..self.format.inputs {
            let Some(sink) = block.sink(INPUTS, channel) else {
                continue;
            };
            to_f32(sink, &mut planar[..frames]);
            let channel = usize::from(channel);
            for (frame, sample) in planar[..frames].iter().enumerate() {
                interleaved[frame * stride + channel] = *sample;
            }
        }
        let pushed = self.input.push_entire_slice(interleaved);
        debug_assert!(pushed.is_ok(), "the input ring had room");
        self.since_block += frames;
        if self.since_block < self.format.frames() {
            return false;
        }
        self.since_block -= self.format.frames();
        true
    }

    /// Fills the cycle's output channels: lead-in silence, then rendered frames. Frames that
    /// have not come play as silence and are owed.
    fn send(&mut self, block: &mut Block<'_>) {
        if self.resyncing {
            return;
        }
        let frames = block.frames();
        let silent = self.lead_in.min(frames);
        self.lead_in -= silent;
        let wanted = frames - silent;
        let outputs = usize::from(self.format.outputs);
        let available = self.output.slots() / outputs;
        let late = self.owed.min(available);
        discard(&mut self.output, late * outputs);
        self.owed -= late;
        let ready = (available - late).min(wanted);
        if ready < wanted {
            self.owed += wanted - ready;
            bump(&self.shared.underruns);
        }
        if ready == 0 {
            return;
        }
        let interleaved = &mut self.interleaved[..ready * outputs];
        let popped = self.output.pop_entire_slice(interleaved);
        debug_assert!(popped.is_ok(), "the output ring had the frames");
        if self.shared.muted.load(Relaxed) {
            return;
        }
        let mut planar = [0.0; MAX_FRAMES];
        for channel in 0..self.format.outputs {
            let Some(source) = block.source(OUTPUTS, channel) else {
                continue;
            };
            let column = usize::from(channel);
            for (frame, sample) in planar[..ready].iter_mut().enumerate() {
                *sample = interleaved[frame * outputs + column];
            }
            from_f32(&planar[..ready], &mut source[silent..silent + ready]);
        }
    }
}

/// Drops `samples` samples from the front of `ring`.
fn discard(ring: &mut Consumer<f32>, samples: usize) {
    if let Ok(chunk) = ring.read_chunk(samples) {
        chunk.commit_all();
    }
}

/// The render half: whole product blocks through the renderer.
pub(crate) struct RenderStage {
    renderer: Box<ProductRenderer>,
    input: Consumer<f32>,
    output: Producer<f32>,
    format: SessionFormat,
    /// Drives the legacy stereo [`OutputRenderer`] path instead of the routed duplex path.
    stereo: bool,
    input_block: Box<[f32]>,
    output_block: Box<[f32]>,
    epoch: u64,
    pub(crate) shared: Arc<Shared>,
}

impl RenderStage {
    pub(crate) const fn format(&self) -> SessionFormat {
        self.format
    }

    pub(crate) fn into_renderer(self) -> Box<ProductRenderer> {
        self.renderer
    }

    pub(crate) fn telemetry(&self) -> SessionTelemetry {
        SessionTelemetry(Arc::clone(&self.shared))
    }

    /// Answers a resync request, or renders every complete block that has room in the output
    /// ring. Returns whether it did any work.
    ///
    /// Real-time: allocation-free; it waits only inside the renderer, for workers.
    pub(crate) fn run(&mut self) -> bool {
        let request = self.shared.resync_request.load(Acquire);
        if request != self.epoch {
            let stale = self.input.slots();
            if let Ok(chunk) = self.input.read_chunk(stale) {
                chunk.commit_all();
            }
            self.epoch = request;
            self.shared.resync_ack.store(request, Release);
            return true;
        }
        let mut rendered = false;
        while self.input.slots() >= self.input_block.len()
            && self.output.slots() >= self.output_block.len()
        {
            let popped = self.input.pop_entire_slice(&mut self.input_block);
            debug_assert!(popped.is_ok(), "the input ring had a block");
            let disposition = self.render();
            if disposition == RenderDisposition::Silence {
                self.output_block.fill(0.0);
            }
            let pushed = self.output.push_entire_slice(&self.output_block);
            debug_assert!(pushed.is_ok(), "the output ring had room");
            bump(match disposition {
                RenderDisposition::Rendered => &self.shared.rendered,
                RenderDisposition::Silence => &self.shared.silenced,
            });
            rendered = true;
        }
        rendered
    }

    fn render(&mut self) -> RenderDisposition {
        let format = self.format;
        if self.stereo {
            return match InterleavedStereoF32::new(&mut self.output_block, format.block) {
                Ok(output) => OutputRenderer::render(&mut *self.renderer, output),
                Err(_) => RenderDisposition::Silence,
            };
        }
        let input = if format.inputs == 0 {
            &[][..]
        } else {
            &self.input_block[..]
        };
        match MultiChannelDuplexF32::new(
            input,
            &mut self.output_block,
            format.inputs,
            format.outputs,
            format.block,
        ) {
            Some(block) => MultiChannelDuplexRenderer::render(&mut *self.renderer, block),
            None => RenderDisposition::Silence,
        }
    }
}

/// Builds both halves of a session around `renderer`. Allocates every buffer.
///
/// The input ring holds the lead-in and two cycles: the network side only overruns after the
/// render stage has missed that much. The output ring adds the input ring's size, so a render
/// stage that catches up on a backlog never waits for room.
pub(crate) fn session(
    renderer: Box<ProductRenderer>,
    format: SessionFormat,
    stereo: bool,
) -> (NetworkSide, RenderStage) {
    let latency = format.latency().frames as usize;
    let input_frames = latency + 2 * MAX_FRAMES;
    let stride = format.input_stride();
    let outputs = usize::from(format.outputs);
    let (input_tx, input_rx) = RingBuffer::new(input_frames * stride);
    let (output_tx, output_rx) = RingBuffer::new((input_frames + latency + MAX_FRAMES) * outputs);
    let shared = Arc::new(Shared::default());
    let network = NetworkSide {
        input: input_tx,
        output: output_rx,
        format,
        lead_in: latency,
        owed: 0,
        since_block: 0,
        epoch: 0,
        resyncing: false,
        interleaved: vec![0.0; MAX_FRAMES * stride.max(outputs)].into_boxed_slice(),
        shared: Arc::clone(&shared),
        render: None,
    };
    let frames = format.frames();
    let stage = RenderStage {
        renderer,
        input: input_rx,
        output: output_tx,
        format,
        stereo,
        input_block: vec![0.0; frames * stride].into_boxed_slice(),
        output_block: vec![0.0; frames * outputs].into_boxed_slice(),
        epoch: 0,
        shared,
    };
    (network, stage)
}

/// A session whose render stage runs inside the Tunnel cycle, for the virtual-time simulator
/// and offline runs. A real-time system uses [`crate::TunnelAudioEndpoint`], whose render
/// stage runs on its own thread. The rings, the lead-in and the latency are the same.
pub struct InlineApp {
    network: NetworkSide,
    stage: RenderStage,
}

impl std::fmt::Debug for InlineApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InlineApp")
            .field("format", &self.stage.format)
            .finish_non_exhaustive()
    }
}

impl InlineApp {
    /// A session of `format` around `renderer`. Allocates every buffer.
    pub fn new(renderer: Box<ProductRenderer>, format: SessionFormat) -> Self {
        let (network, stage) = session(renderer, format, false);
        Self { network, stage }
    }

    /// Reads the session counters while the app runs elsewhere.
    pub fn telemetry(&self) -> SessionTelemetry {
        SessionTelemetry(Arc::clone(&self.stage.shared))
    }

    /// Ends the session and gives the renderer back.
    pub fn into_renderer(self) -> Box<ProductRenderer> {
        self.stage.into_renderer()
    }
}

impl AudioApp for InlineApp {
    /// Pushes the cycle's input, renders every complete block, then plays the output.
    fn process(&mut self, block: &mut Block<'_>) {
        self.network.receive(block);
        self.stage.run();
        self.network.send(block);
    }
}

#[cfg(test)]
mod tests {
    use sp_engine::PreparedGraph;
    use sp_model::{
        ChannelLayout, Endpoint, EndpointId, PhysicalChannels, Rack, RackChannelRoute, RackId,
        RackTopology, Session, Source, SourceId,
    };
    use tunnel_host::audio::{AudioBuffers, AudioLayout};
    use tunnel_types::MediaTime;

    use super::*;
    use crate::{Endpoints, TunnelChannels};

    const FORMAT: SessionFormat = SessionFormat {
        block: BlockFrames::Frames32,
        sample_rate_hz: 48_000,
        inputs: 2,
        outputs: 2,
    };

    /// Input frames before this media time are silent, so the renderer's start-up fade from
    /// silence to the dry path never touches the checked signal.
    const WARM_UP: usize = 128;

    /// One bypassed stereo rack from inputs 0–1 to outputs 0–1: a bit-exact passthrough.
    fn passthrough() -> Box<ProductRenderer> {
        let mut session = Session::new();
        session.sources.push(Source {
            id: SourceId("in".into()),
            name: "In".into(),
            layout: ChannelLayout::Stereo,
        });
        session.endpoints.push(Endpoint {
            id: EndpointId("out".into()),
            name: "Out".into(),
            layout: ChannelLayout::Stereo,
        });
        let id = RackId("rack".into());
        session.racks.push(Rack {
            id: id.clone(),
            name: "Rack".into(),
            topology: RackTopology::Serial,
            source_id: SourceId("in".into()),
            endpoint_id: EndpointId("out".into()),
            gain_db: sp_model::GainDb::default(),
            muted: false,
            bypassed: false,
            slots: Vec::new(),
        });
        let stereo = PhysicalChannels::Stereo { left: 0, right: 1 };
        session.rack_routes.insert(
            id,
            RackChannelRoute {
                input: Some(stereo),
                output: stereo,
            },
        );
        let mut renderer = ProductRenderer::new(PreparedGraph::compile(&session).unwrap());
        renderer.mixer_mut().set_rack_bypassed(0, true);
        Box::new(renderer)
    }

    /// The input sample at media time `time`: silence during the warm-up, then `time + 1`.
    fn input(time: usize) -> i32 {
        if time < WARM_UP {
            0
        } else {
            i32::try_from(time + 1).unwrap()
        }
    }

    /// Tunnel cycles of 24 frames from media time 0, with [`input`] on both input channels.
    struct Cycles {
        layout: AudioLayout,
        buffers: AudioBuffers,
        time: usize,
        /// Output frames that played silence where the delayed input was not silent.
        silent: usize,
    }

    impl Cycles {
        fn new() -> Self {
            let channels = TunnelChannels {
                inputs: 2,
                outputs: 2,
            };
            let layout = AudioLayout::new(&Endpoints::generate().decls(channels));
            let buffers = AudioBuffers::new(&layout);
            Self {
                layout,
                buffers,
                time: 0,
                silent: 0,
            }
        }

        /// Runs `step` on the next cycle and checks the alignment invariant: each output
        /// sample is the input exactly one added latency earlier, or silence. Returns whether
        /// the whole cycle had the delayed input.
        fn run(&mut self, step: impl FnOnce(&mut Block<'_>)) -> bool {
            self.buffers.clear();
            for channel in 0..2 {
                for (frame, sample) in self.buffers.sink_mut(channel).unwrap()[..24]
                    .iter_mut()
                    .enumerate()
                {
                    *sample = input(self.time + frame);
                }
            }
            let time = MediaTime(u64::try_from(self.time).unwrap());
            step(&mut self.buffers.block(&self.layout, time, 24));
            let latency = FORMAT.latency().frames as usize;
            let mut whole = true;
            for channel in 0..2 {
                for (frame, &sample) in self.buffers.source(channel).unwrap()[..24]
                    .iter()
                    .enumerate()
                {
                    let want = (self.time + frame).checked_sub(latency).map_or(0, input);
                    assert!(
                        sample == want || sample == 0,
                        "{sample} at {} channel {channel}",
                        self.time + frame
                    );
                    if sample != want {
                        whole = false;
                        self.silent += usize::from(channel == 0);
                    }
                }
            }
            self.time += 24;
            whole
        }
    }

    /// The latency is two product blocks and the largest Tunnel cycle, whatever the cycle.
    #[test]
    fn the_added_latency_is_two_blocks_and_one_cycle() {
        let latency = FORMAT.latency();
        assert_eq!(latency.frames, 2 * 32 + 24);
        assert_eq!(latency.duration().as_micros(), 1_833);
    }

    /// A render stage that misses cycles costs only the late frames: they play as silence and
    /// count as underruns, then are discarded when they come, so every later frame keeps the
    /// added latency. No resync happens.
    #[test]
    fn a_late_render_stage_plays_silence_and_keeps_the_alignment() {
        let (mut network, mut stage) = session(passthrough(), FORMAT, false);
        let mut cycles = Cycles::new();
        for cycle in 0..40 {
            let stalled = (12..16).contains(&cycle);
            cycles.run(|block| {
                network.receive(block);
                if !stalled {
                    stage.run();
                }
                network.send(block);
            });
        }
        let counters = stage.telemetry().counters();
        assert!(counters.underruns > 0);
        assert!(cycles.silent > 0);
        assert_eq!(counters.overruns, 0);
        assert!(cycles.run(|block| {
            network.receive(block);
            stage.run();
            network.send(block);
        }));
        let telemetry = counters.callback_telemetry(FORMAT.block);
        assert!(telemetry.is_coherent());
        assert_eq!(telemetry.invalid_bytes, counters.underruns);
    }

    /// With no render stage the input ring fills. The cycle that does not fit counts as an
    /// overrun and starts a resync; the output is silent meanwhile. Once the stage drops the
    /// stale input, the lead-in plays again and the output is aligned as before.
    #[test]
    fn an_overrun_resyncs_to_the_same_latency() {
        let (mut network, mut stage) = session(passthrough(), FORMAT, false);
        let mut cycles = Cycles::new();
        let mut inline = |cycles: &mut Cycles, network: &mut NetworkSide| {
            cycles.run(|block| {
                network.receive(block);
                stage.run();
                network.send(block);
            })
        };
        while cycles.time < WARM_UP + 2 * FORMAT.latency().frames as usize {
            assert!(inline(&mut cycles, &mut network));
        }
        let stalled = cycles.time;
        while network.shared.overruns.load(Relaxed) == 0 {
            cycles.run(|block| network.process(block));
        }
        assert!(network.resyncing);
        let input_ring = FORMAT.latency().frames as usize + 2 * MAX_FRAMES;
        assert!(cycles.time - stalled > input_ring - 2 * 32);
        for _ in 0..20 {
            inline(&mut cycles, &mut network);
        }
        assert!(!network.resyncing);
        assert!(inline(&mut cycles, &mut network));
        let counters = SessionTelemetry(Arc::clone(&network.shared)).counters();
        assert_eq!(counters.overruns, 1);
        assert!(counters.underruns > 0);
    }
}
