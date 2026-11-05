//! The Lambda-shaped side of a rack: an insert task on the real-time path and a control plane
//! that starts, chains, and replaces the rack's segment workers.

use std::{
    ffi::OsString,
    io::{self, BufRead, BufReader, Write},
    path::PathBuf,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use sp_rack_lane::{
    FaderBlock, FaderSettings, LaneFader, LaneFallback, LaneLayout, MIN_TRANSITION_FRAMES, RackLane,
};
use sp_shared_memory::{BlockRequest, BlockTiming};
use sp_shared_memory_os::{
    MonotonicClock, RackRecoverySignal, RackRecoveryState, SharedMemoryRegion, reset_reaped_region,
};

use crate::echo::{EchoConfig, EchoWorker};

/// How long a new worker may take to open its bank. A cold Wine prefix takes the longest.
const READY_TIMEOUT: Duration = Duration::from_mins(2);

/// Where a segment's plug-ins run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Platform {
    /// A Linux worker.
    Native,
    /// A Windows worker under Wine.
    Windows,
}

impl Platform {
    /// The report name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Windows => "windows",
        }
    }
}

/// One segment of a rack: a run of same-platform plug-ins in one worker.
#[derive(Clone, Copy, Debug)]
pub struct SegmentSpec {
    /// Where the worker runs.
    pub platform: Platform,
    /// The gain the segment applies, so a test can see which segments processed a block.
    pub gain: f32,
    /// Busy time per block, standing in for plug-in cost.
    pub load: Duration,
}

/// How to start workers.
#[derive(Clone, Debug)]
pub enum Launch {
    /// Threads in this process, without real-time policy. Both platforms run natively; tests
    /// use them for deterministic dispatch and recovery.
    Threads,
    /// Worker processes with the product's real-time policy.
    Processes(ProcessLaunch),
}

/// Executables and Wine setup for worker processes.
#[derive(Clone, Debug)]
pub struct ProcessLaunch {
    /// The Linux `sp-lane-worker`.
    pub native_worker: PathBuf,
    /// The Windows `sp-lane-worker.exe` and its Wine environment, for Windows segments.
    pub wine: Option<WineLaunch>,
}

/// How to run the Windows worker under Wine.
#[derive(Clone, Debug)]
pub struct WineLaunch {
    /// The `wine` loader.
    pub wine: OsString,
    /// `sp-lane-worker.exe`, built for `x86_64-pc-windows-gnu`.
    pub worker_exe: PathBuf,
    /// The directory that holds `x86_64-windows/` and `x86_64-unix/` of `sp_wine_bridge`.
    pub bridge_dir: PathBuf,
    /// The Wine prefix.
    pub prefix: PathBuf,
}

/// A running worker.
enum WorkerHandle {
    Thread {
        stop: Arc<AtomicBool>,
        hang: Arc<AtomicBool>,
        /// Set by the thread once it no longer serves.
        stalled: Arc<AtomicBool>,
        control: mpsc::Sender<(Option<String>, mpsc::Sender<io::Result<()>>)>,
        join: JoinHandle<io::Result<()>>,
    },
    Process {
        child: Child,
        stdin: ChildStdin,
        stdout: BufReader<ChildStdout>,
    },
}

impl WorkerHandle {
    fn start(
        launch: &Launch,
        bank: &str,
        segment: SegmentSpec,
        worker_id: u32,
        sample_rate: u32,
    ) -> io::Result<Self> {
        let config = EchoConfig {
            worker_id,
            gain: segment.gain,
            load: segment.load,
            sample_rate,
            realtime: matches!(launch, Launch::Processes(_)),
        };
        match launch {
            Launch::Threads => Ok(Self::spawn_thread(bank.to_owned(), config)),
            Launch::Processes(processes) => {
                let mut command = match (segment.platform, processes.wine.as_ref()) {
                    (Platform::Native, _) => Command::new(&processes.native_worker),
                    (Platform::Windows, Some(wine)) => {
                        let mut command = Command::new(&wine.wine);
                        command
                            .arg(&wine.worker_exe)
                            .env("WINEPREFIX", &wine.prefix)
                            .env("WINEDLLPATH", &wine.bridge_dir)
                            .env("WINEDEBUG", "-all")
                            .env_remove("DISPLAY")
                            .env_remove("WAYLAND_DISPLAY");
                        command
                    }
                    (Platform::Windows, None) => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "a Windows segment needs the Wine worker",
                        ));
                    }
                };
                let mut child = command
                    .args(config.arguments(bank))
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn()?;
                let stdin = child.stdin.take().expect("piped stdin");
                let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
                Ok(Self::Process {
                    child,
                    stdin,
                    stdout,
                })
            }
        }
    }

    fn spawn_thread(bank: String, config: EchoConfig) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let hang = Arc::new(AtomicBool::new(false));
        let stalled = Arc::new(AtomicBool::new(false));
        let (control, commands) = mpsc::channel::<(Option<String>, mpsc::Sender<io::Result<()>>)>();
        let join = thread::spawn({
            let stop = Arc::clone(&stop);
            let hang = Arc::clone(&hang);
            let stalled = Arc::clone(&stalled);
            move || {
                let mut worker = EchoWorker::open(&bank, config)?;
                while !stop.load(Ordering::Acquire) {
                    while let Ok((next, reply)) = commands.try_recv() {
                        let _ = reply.send(worker.retarget(next.as_deref()));
                    }
                    if hang.load(Ordering::Acquire) {
                        stalled.store(true, Ordering::Release);
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    worker.serve_once()?;
                }
                Ok(())
            }
        });
        Self::Thread {
            stop,
            hang,
            stalled,
            control,
            join,
        }
    }

    /// Points the worker at the next segment's bank and waits until it has.
    fn retarget(&mut self, next: Option<&str>) -> io::Result<()> {
        match self {
            Self::Thread { control, .. } => {
                let (reply, done) = mpsc::channel();
                control
                    .send((next.map(str::to_owned), reply))
                    .map_err(|_| io::Error::other("the worker thread stopped"))?;
                done.recv_timeout(READY_TIMEOUT)
                    .map_err(|_| io::Error::other("the worker thread did not retarget"))?
            }
            Self::Process { stdin, stdout, .. } => {
                writeln!(stdin, "retarget {}", next.unwrap_or("-"))?;
                stdin.flush()?;
                let mut line = String::new();
                stdout.read_line(&mut line)?;
                match line.trim() {
                    "ok" => Ok(()),
                    error => Err(io::Error::other(format!("worker: {error}"))),
                }
            }
        }
    }

    /// Stops serving without exiting, as a hung plug-in does, and returns once the worker has
    /// stopped. Threads only.
    fn hang(&self) {
        if let Self::Thread { hang, stalled, .. } = self {
            hang.store(true, Ordering::Release);
            let started = Instant::now();
            while !stalled.load(Ordering::Acquire) && started.elapsed() < READY_TIMEOUT {
                thread::sleep(Duration::from_micros(100));
            }
        }
    }

    /// Stops the worker and reaps it, so its banks can be reset.
    fn stop(self) {
        match self {
            Self::Thread { stop, join, .. } => {
                stop.store(true, Ordering::Release);
                let _ = join.join();
            }
            Self::Process { mut child, .. } => {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

impl EchoConfig {
    /// Command-line arguments of `sp-lane-worker` for this configuration.
    #[must_use]
    pub fn arguments(&self, bank: &str) -> Vec<String> {
        vec![
            "--bank".to_owned(),
            bank.to_owned(),
            "--worker-id".to_owned(),
            self.worker_id.to_string(),
            "--gain".to_owned(),
            self.gain.to_string(),
            "--load-ns".to_owned(),
            self.load.as_nanos().to_string(),
            "--sample-rate".to_owned(),
            self.sample_rate.to_string(),
        ]
    }
}

/// One segment's banks and worker, owned by the control plane.
struct Segment {
    spec: SegmentSpec,
    /// The owner mappings of the segment's two banks.
    banks: [SharedMemoryRegion; 2],
    active: usize,
    worker: Option<WorkerHandle>,
    /// `chain_link_failures` already handled, per bank.
    failures_seen: [u32; 2],
}

/// Counters of one rack's control plane.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ControlCounters {
    /// First-segment workers replaced after the lane closed.
    pub replacements: u32,
    /// Later segments replaced after a chain-link failure.
    pub segment_replacements: u32,
}

/// The control plane of one rack: the segment workers, their bank pairs, and the lane's
/// recovery handshake. It runs off the real-time path, as Lambda's plug-in supervisor will.
pub struct RackControl {
    launch: Launch,
    sample_rate: u32,
    recovery: Arc<RackRecoverySignal>,
    segments: Vec<Segment>,
    next_generation: u64,
    next_worker_id: AtomicU32,
    counters: ControlCounters,
}

impl RackControl {
    /// Starts a rack: one worker per segment, last to first, each pointed at the next, and the
    /// insert that drives the first segment's lane.
    ///
    /// # Errors
    /// Returns an error when a bank cannot be created or a worker does not start.
    ///
    /// # Panics
    /// If `segments` is empty.
    pub fn start(
        launch: Launch,
        segments: &[SegmentSpec],
        layout: LaneLayout,
        sample_rate: u32,
    ) -> io::Result<(Insert, Self)> {
        assert!(!segments.is_empty(), "a rack has at least one segment");
        let mut control = Self {
            launch,
            sample_rate,
            recovery: Arc::new(RackRecoverySignal::new()),
            segments: Vec::with_capacity(segments.len()),
            next_generation: 1,
            next_worker_id: AtomicU32::new(1),
            counters: ControlCounters::default(),
        };
        for spec in segments.iter().rev() {
            let banks = [
                SharedMemoryRegion::create(control.generation())?,
                SharedMemoryRegion::create(control.generation())?,
            ];
            let mut worker = control.start_worker(&banks[0], *spec)?;
            let next = control.segments.last().map(Segment::active_bank_name);
            worker.retarget(next.as_deref())?;
            control.segments.push(Segment {
                spec: *spec,
                banks,
                active: 0,
                worker: Some(worker),
                failures_seen: [0; 2],
            });
        }
        control.segments.reverse();
        let first = &control.segments[0];
        let clock = MonotonicClock::new()?;
        let lane = RackLane::from_live_regions(
            clock,
            [
                SharedMemoryRegion::open(first.banks[0].name())?,
                SharedMemoryRegion::open(first.banks[1].name())?,
            ],
            0,
            Arc::clone(&control.recovery),
        )?;
        Ok((Insert::new(lane, layout), control))
    }

    /// The control counters.
    #[must_use]
    pub fn counters(&self) -> ControlCounters {
        self.counters
    }

    /// The segments, first to last.
    pub fn segment_specs(&self) -> impl Iterator<Item = SegmentSpec> + '_ {
        self.segments.iter().map(|segment| segment.spec)
    }

    /// Makes segment `index`'s worker stop serving without exiting (threads only).
    pub fn hang_segment(&self, index: usize) {
        if let Some(worker) = self.segments[index].worker.as_ref() {
            worker.hang();
        }
    }

    /// Stops segment `index`'s worker, as a crash would.
    pub fn kill_segment(&mut self, index: usize) {
        if let Some(worker) = self.segments[index].worker.take() {
            worker.stop();
        }
    }

    /// One control-plane pass: advances the lane's recovery handshake and replaces later
    /// segments whose upstream worker reported a chain-link failure.
    ///
    /// # Errors
    /// Returns an error when a bank cannot be reset or a replacement worker does not start.
    pub fn service(&mut self) -> io::Result<()> {
        match self.recovery.state() {
            RackRecoveryState::Quiescent => self.replace_first()?,
            RackRecoveryState::ReplacementActive {
                retiring_bank_index,
            } => {
                // The old worker was reaped before the replacement was published.
                let generation = self.generation();
                reset_reaped_region(&mut self.segments[0].banks[retiring_bank_index], generation)?;
                self.segments[0].failures_seen[retiring_bank_index] = 0;
                self.recovery
                    .publish_retired_reset(retiring_bank_index, generation);
            }
            _ => {}
        }
        for upstream in 0..self.segments.len() - 1 {
            let segment = &mut self.segments[upstream];
            let failures = segment.banks[segment.active]
                .bank()
                .header
                .chain_link_failures
                .load(Ordering::Acquire);
            if failures != segment.failures_seen[segment.active] {
                segment.failures_seen[segment.active] = failures;
                self.replace_later(upstream + 1)?;
            }
        }
        Ok(())
    }

    /// Stops every worker.
    pub fn stop(mut self) {
        for segment in &mut self.segments {
            if let Some(worker) = segment.worker.take() {
                worker.stop();
            }
        }
    }

    fn replace_first(&mut self) -> io::Result<()> {
        let replacement = self.start_replacement(0)?;
        self.counters.replacements += 1;
        let generation = self.segments[0].banks[replacement].generation();
        self.segments[0].active = replacement;
        self.recovery.publish_replacement(replacement, generation);
        Ok(())
    }

    fn replace_later(&mut self, index: usize) -> io::Result<()> {
        let retired = self.segments[index].active;
        let replacement = self.start_replacement(index)?;
        self.segments[index].active = replacement;
        let name = self.segments[index].active_bank_name();
        self.segments[index - 1]
            .worker
            .as_mut()
            .ok_or_else(|| io::Error::other("the upstream segment has no worker"))?
            .retarget(Some(&name))?;
        // Nothing maps the retired bank now: its worker was reaped and the upstream link moved.
        let generation = self.generation();
        reset_reaped_region(&mut self.segments[index].banks[retired], generation)?;
        self.segments[index].failures_seen[retired] = 0;
        self.counters.segment_replacements += 1;
        Ok(())
    }

    /// Stops segment `index`'s worker and starts a new one on the other bank, pointed at the
    /// next segment. Returns the new bank's index.
    fn start_replacement(&mut self, index: usize) -> io::Result<usize> {
        if let Some(worker) = self.segments[index].worker.take() {
            worker.stop();
        }
        let replacement = self.segments[index].active ^ 1;
        let generation = self.generation();
        reset_reaped_region(&mut self.segments[index].banks[replacement], generation)?;
        self.segments[index].failures_seen[replacement] = 0;
        let spec = self.segments[index].spec;
        let mut worker = self.start_worker(&self.segments[index].banks[replacement], spec)?;
        let next = self.segments.get(index + 1).map(Segment::active_bank_name);
        worker.retarget(next.as_deref())?;
        self.segments[index].worker = Some(worker);
        Ok(replacement)
    }

    fn start_worker(
        &self,
        bank: &SharedMemoryRegion,
        spec: SegmentSpec,
    ) -> io::Result<WorkerHandle> {
        let worker_id = self.next_worker_id.fetch_add(1, Ordering::Relaxed);
        let mut worker =
            WorkerHandle::start(&self.launch, bank.name(), spec, worker_id, self.sample_rate)?;
        let started = Instant::now();
        while bank
            .bank()
            .header
            .worker_heartbeat_tick
            .load(Ordering::Acquire)
            == 0
        {
            if let WorkerHandle::Process { child, .. } = &mut worker
                && let Some(status) = child.try_wait()?
            {
                return Err(io::Error::other(format!(
                    "the {} worker exited before it opened its bank: {status}",
                    spec.platform.name()
                )));
            }
            if started.elapsed() > READY_TIMEOUT {
                worker.stop();
                return Err(io::Error::other("the worker did not open its bank"));
            }
            thread::sleep(Duration::from_millis(1));
        }
        Ok(worker)
    }

    fn generation(&mut self) -> u64 {
        let generation = self.next_generation;
        self.next_generation += 1;
        generation
    }
}

impl Segment {
    fn active_bank_name(&self) -> String {
        self.banks[self.active].name().to_owned()
    }
}

/// What one read gave.
#[derive(Clone, Copy, Debug)]
pub struct ReadOutcome {
    /// The bank timing of the accepted completion, or `None` when the block played fallback.
    pub timing: Option<BlockTiming>,
}

/// A rack-insert task, shaped like Lambda's: in each block it reads the rack output of the last
/// block, then writes the rack input of this block. The host wakes the worker after the layer
/// barrier. The rack is pipelined by one block, and its dry fallback is delayed to match.
pub struct Insert {
    lane: RackLane,
    fader: LaneFader,
    settings: FaderSettings,
    channels: u32,
}

impl Insert {
    fn new(lane: RackLane, layout: LaneLayout) -> Self {
        Self {
            lane,
            fader: LaneFader::new(),
            settings: FaderSettings {
                source: layout,
                endpoint: layout,
                bypassed: false,
                latency_frames: 0,
                dry_fallback: true,
                transition_frames: MIN_TRANSITION_FRAMES,
            },
            channels: match layout {
                LaneLayout::Mono => 1,
                LaneLayout::Stereo => 2,
            },
        }
    }

    /// The lane, for telemetry and bank inspection.
    #[must_use]
    pub fn lane(&self) -> &RackLane {
        &self.lane
    }

    /// Lambda's read point (the insert task in block N + 1): takes block N's result without
    /// waiting, and renders this block's output into `out`: the result once the lane has
    /// recovered, otherwise `dry` delayed by the rack's latency. `dry` and `out` are interleaved
    /// stereo. Real-time safe.
    pub fn read(&mut self, block: u64, dry: &[f32], frames: usize, out: &mut [f32]) -> ReadOutcome {
        let Self {
            lane,
            fader,
            settings,
            ..
        } = self;
        lane.begin_block(block);
        if let Some(latency) = lane.worker_latency_samples() {
            // One block of pipeline, plus what the rack's workers report.
            settings.latency_frames = frames + latency as usize;
        }
        let wet = lane.consume(block);
        fader.render(
            FaderBlock {
                dry,
                wet: wet.map(|wet| &wet[..]),
                accepted: wet.is_some(),
                fallback: LaneFallback::Dry,
                frames,
            },
            settings,
            out,
        );
        ReadOutcome {
            timing: wet.is_some().then(|| lane.completion_timing()),
        }
    }

    /// Lambda's publish point (the same task, after its read): writes block N's input and
    /// publishes it with the host's read deadline, without waking the worker. `input` is
    /// interleaved stereo; a mono rack sends its left channel. Real-time safe.
    pub fn write(&mut self, block: u64, input: &[f32], frames: usize, read_deadline_tick: u64) {
        let channels = self.channels;
        self.lane.publish(block, Some(read_deadline_tick), |slot| {
            for frame in 0..frames {
                for channel in 0..channels as usize {
                    slot.input_audio[channel][frame] = input[frame * 2 + channel];
                }
            }
            BlockRequest {
                frame_count: u32::try_from(frames).unwrap_or(u32::MAX),
                input_channel_count: channels,
                output_channel_count: channels,
                midi_event_count: 0,
                event_count: 0,
                flags: 0,
                sidechain_slots: 0,
            }
        });
        self.lane.end_block();
    }

    /// The wake after the layer barrier: one non-blocking kernel wake for a published request.
    pub fn wake(&self) {
        self.lane.wake();
    }
}
