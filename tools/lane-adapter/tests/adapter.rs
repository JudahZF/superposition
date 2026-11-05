//! Dispatch and recovery of the Lambda-shaped adapter.
//!
//! Most tests run the echo workers on threads and wait for each block's completion, so they are
//! deterministic. One runs native worker processes. The Wine test needs the Windows worker and
//! the bridge, so it is ignored; from the dev shell:
//!
//! ```text
//! cargo xtask lane-soak --build-only
//! SP_LANE_WINE_WORKER=<target>/x86_64-pc-windows-gnu/release/sp-lane-worker.exe \
//! SP_LANE_WINE_BRIDGE=<workspace>/target/wine-bridge \
//!     cargo test -p sp-lane-adapter --test adapter -- --ignored
//! ```

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use sp_lane_adapter::{
    Insert, Launch, Platform, ProcessLaunch, RackControl, SegmentSpec, WineLaunch,
    schedule::{Placement, Schedule},
};
use sp_rack_lane::LaneLayout;
use sp_shared_memory::SlotState;
use sp_shared_memory_os::{MonotonicClock, RackRecoveryState};

const RATE: u32 = 48_000;
/// How long a block waits for its worker before the next read.
const SETTLE: Duration = Duration::from_secs(2);
/// The read deadline the tests publish: generous, so only a stopped worker misses.
const DEADLINE: Duration = Duration::from_millis(50);

fn segment(platform: Platform, gain: f32) -> SegmentSpec {
    SegmentSpec {
        platform,
        gain,
        load: Duration::ZERO,
    }
}

/// One rack driven block by block: read, write, wake, then wait for the worker.
struct Rig {
    insert: Insert,
    control: RackControl,
    clock: MonotonicClock,
    layout: LaneLayout,
    frames: usize,
    block: u64,
    /// The input value of each block so far.
    values: Vec<f32>,
    output: Vec<f32>,
}

impl Rig {
    fn start(launch: Launch, segments: &[SegmentSpec], layout: LaneLayout, frames: usize) -> Self {
        let (insert, control) =
            RackControl::start(launch, segments, layout, RATE).expect("the rack starts");
        Self {
            insert,
            control,
            clock: MonotonicClock::new().expect("clock"),
            layout,
            frames,
            block: 0,
            values: Vec::new(),
            output: vec![0.0; frames * 2],
        }
    }

    /// One block: read the previous block's result, publish this block, wake the worker, wait
    /// up to `settle` for its completion, and service the control plane.
    fn run(&mut self, settle: Duration) {
        #[allow(clippy::cast_precision_loss, reason = "a small test signal")]
        let value = (self.block + 1) as f32 / 64.0;
        self.values.push(value);
        let input: Vec<f32> = (0..self.frames).flat_map(|_| [value, -value]).collect();
        self.insert
            .read(self.block, &input, self.frames, &mut self.output);
        let deadline = self.clock.now_ticks() + self.clock.duration_to_ticks(DEADLINE);
        self.insert.write(self.block, &input, self.frames, deadline);
        if let Some(live) = self.insert.lane().live() {
            let slot = self.slot_state(live.bank_index, live.slot_index);
            assert_eq!(
                slot.1,
                Some(deadline),
                "the request carries the host's read deadline"
            );
        }
        self.insert.wake();
        let started = Instant::now();
        while let Some(live) = self.insert.lane().live() {
            if self.slot_state(live.bank_index, live.slot_index).0 == Ok(SlotState::Complete)
                || started.elapsed() > settle
            {
                break;
            }
            std::thread::sleep(Duration::from_micros(100));
        }
        self.control.service().expect("control plane");
        self.block += 1;
    }

    fn slot_state(
        &self,
        bank: usize,
        slot: usize,
    ) -> (
        Result<SlotState, sp_shared_memory::ProtocolError>,
        Option<u64>,
    ) {
        let metadata = &self
            .insert
            .lane()
            .banks()
            .bank(bank)
            .expect("bank")
            .bank()
            .slot(slot)
            .expect("slot")
            .metadata;
        (metadata.state(), metadata.deadline_tick())
    }

    /// Whether the last block played the previous block's input times `gain` on every frame.
    fn played(&self, gain: f32) -> bool {
        let Some(previous) = self
            .values
            .len()
            .checked_sub(2)
            .map(|index| self.values[index])
        else {
            return false;
        };
        let previous = previous * gain;
        let right = match self.layout {
            LaneLayout::Mono => previous,
            LaneLayout::Stereo => -previous,
        };
        self.output
            .chunks_exact(2)
            .all(|frame| (frame[0] - previous).abs() < 1e-6 && (frame[1] - right).abs() < 1e-6)
    }

    fn run_until(&mut self, gain: f32, blocks: usize) -> bool {
        for _ in 0..blocks {
            self.run(SETTLE);
            if self.played(gain) {
                return true;
            }
        }
        false
    }
}

#[test]
fn mono_and_stereo_racks_play_their_workers_output_at_every_block_size() {
    for frames in [12, 24, 32, 64, 128] {
        for layout in [LaneLayout::Mono, LaneLayout::Stereo] {
            let mut rig = Rig::start(
                Launch::Threads,
                &[segment(Platform::Native, 0.5)],
                layout,
                frames,
            );
            for _ in 0..20 {
                rig.run(SETTLE);
            }
            assert!(rig.played(0.5), "{frames} frames, {layout:?}: wet output");
            let telemetry = rig.insert.lane().telemetry();
            assert_eq!(telemetry.deadline_misses, 0);
            assert_eq!(telemetry.published, 20);
            assert_eq!(telemetry.completed, 19, "the last request is still live");
            let timing = rig.insert.lane().completion_timing();
            assert!(timing.request_published_tick <= timing.worker_claimed_tick);
            assert!(timing.worker_claimed_tick <= timing.completion_published_tick);
            rig.control.stop();
        }
    }
}

/// A rack in the first layer publishes at the block start and has a whole block; a rack in the
/// last layer is woken after the pool's budget and has the rest. Both requests carry the next
/// block start as their read deadline.
#[test]
fn racks_in_the_first_and_last_layer_get_their_windows() {
    let clock = MonotonicClock::new().expect("clock");
    let period = Duration::from_millis(20);
    let frames = 32;
    let mut inserts = Vec::new();
    let mut controls = Vec::new();
    for layout in [LaneLayout::Stereo, LaneLayout::Mono] {
        let (insert, control) = RackControl::start(
            Launch::Threads,
            &[segment(Platform::Native, 0.5)],
            layout,
            RATE,
        )
        .expect("rack");
        inserts.push(insert);
        controls.push(control);
    }
    let placements = [Placement::FIRST_LAYER, Placement::last_layer(0.7)];
    let mut schedule = Schedule::new(clock, period);
    let input = vec![0.25_f32; frames * 2];
    let mut output = vec![0.0_f32; frames * 2];
    let mut accepted = [0_u32; 2];
    for _ in 0..12 {
        let deadline = schedule.next_block_start() + schedule.period_ticks();
        schedule.run_block(
            &mut inserts,
            &placements,
            &input,
            frames,
            &mut output,
            |index, outcome, _| accepted[index] += u32::from(outcome.timing.is_some()),
        );
        for (insert, placement) in inserts.iter().zip(placements) {
            let live = insert.lane().live().expect("a live request");
            let metadata = &insert
                .lane()
                .banks()
                .bank(live.bank_index)
                .expect("bank")
                .bank()
                .slot(live.slot_index)
                .expect("slot")
                .metadata;
            assert_eq!(metadata.deadline_tick(), Some(deadline));
            let window = clock.ticks_to_duration(u64::from(metadata.deadline_offset_ticks));
            let expected = placement.window_of(period);
            assert!(
                window <= expected && window + Duration::from_millis(2) > expected,
                "window {window:?}, expected about {expected:?}"
            );
        }
        for control in &mut controls {
            control.service().expect("control plane");
        }
    }
    assert_eq!(
        accepted,
        [11, 11],
        "every block after the first reads a result"
    );
    for insert in &inserts {
        assert_eq!(insert.lane().telemetry().deadline_misses, 0);
    }
    for control in controls {
        control.stop();
    }
}

/// A hung worker misses its read: the rack closes and plays its delayed dry input until the
/// supervisor replaces the worker on the other bank, then plays wet again.
#[test]
fn a_hung_worker_closes_the_rack_until_its_replacement_plays() {
    let mut rig = Rig::start(
        Launch::Threads,
        &[segment(Platform::Native, 0.5)],
        LaneLayout::Stereo,
        32,
    );
    assert!(rig.run_until(0.5, 20), "the rack plays wet");
    rig.control.hang_segment(0);
    rig.run(Duration::from_millis(5));
    rig.run(SETTLE);
    let telemetry = rig.insert.lane().telemetry();
    assert_eq!(telemetry.deadline_misses, 1);
    assert_eq!(telemetry.missed_unclaimed, 1);
    assert!(
        rig.run_until(1.0, 10),
        "the closed rack plays its dry input"
    );
    assert!(
        rig.run_until(0.5, 30),
        "the replacement's output plays again"
    );
    assert_eq!(rig.control.counters().replacements, 1);
    assert_eq!(rig.insert.lane().telemetry().deadline_misses, 1);
    assert_eq!(
        rig.insert
            .lane()
            .recovery()
            .map(|recovery| recovery.state()),
        Some(RackRecoveryState::Idle),
        "the handoff finished and the retired bank is ready again"
    );
    rig.control.stop();
}

fn later_segment_failure(launch: Launch, segments: &[SegmentSpec], failed: usize) {
    let full: f32 = segments.iter().map(|segment| segment.gain).product();
    let before: f32 = segments[..failed]
        .iter()
        .map(|segment| segment.gain)
        .product();
    let mut rig = Rig::start(launch, segments, LaneLayout::Stereo, 32);
    assert!(rig.run_until(full, 40), "the whole chain plays");
    rig.control.kill_segment(failed);
    assert!(
        rig.run_until(before, 10),
        "the segments before the failed one keep playing"
    );
    assert!(rig.run_until(full, 40), "the replaced segment plays again");
    assert_eq!(rig.control.counters().segment_replacements, 1);
    assert_eq!(rig.control.counters().replacements, 0);
    assert_eq!(
        rig.insert.lane().telemetry().deadline_misses,
        0,
        "the rack never missed its read"
    );
    rig.control.stop();
}

#[test]
fn a_later_segment_failure_is_bypassed_and_replaced() {
    later_segment_failure(
        Launch::Threads,
        &[
            segment(Platform::Native, 0.5),
            segment(Platform::Windows, 0.25),
            segment(Platform::Native, 2.0),
        ],
        1,
    );
}

#[test]
fn worker_processes_chain_and_replace_a_later_segment() {
    later_segment_failure(
        Launch::Processes(ProcessLaunch {
            native_worker: PathBuf::from(env!("CARGO_BIN_EXE_sp-lane-worker")),
            wine: None,
        }),
        &[
            segment(Platform::Native, 0.5),
            segment(Platform::Native, 0.25),
        ],
        1,
    );
}

/// A Windows rack, and a mixed rack whose Windows segment dies, under real Wine.
#[test]
#[ignore = "needs Wine, the Windows worker, and the bridge (see the module docs)"]
fn wine_and_mixed_racks_dispatch_and_recover() {
    let path = |name: &str| PathBuf::from(std::env::var_os(name).expect(name));
    let prefix = std::env::temp_dir().join("sp-lane-adapter-wine");
    let launch = Launch::Processes(ProcessLaunch {
        native_worker: PathBuf::from(env!("CARGO_BIN_EXE_sp-lane-worker")),
        wine: Some(WineLaunch {
            wine: "wine".into(),
            worker_exe: path("SP_LANE_WINE_WORKER"),
            bridge_dir: path("SP_LANE_WINE_BRIDGE"),
            prefix,
        }),
    });
    let mut rig = Rig::start(
        launch.clone(),
        &[segment(Platform::Windows, 0.5)],
        LaneLayout::Mono,
        32,
    );
    assert!(rig.run_until(0.5, 40), "the Windows rack plays");
    rig.control.stop();
    later_segment_failure(
        launch,
        &[
            segment(Platform::Native, 0.5),
            segment(Platform::Windows, 0.25),
            segment(Platform::Native, 2.0),
        ],
        1,
    );
}
