//! One lane's output: the latency-matched dry path and the fades between dry and wet.
//!
//! A lane plays its worker's output (wet) only after [`WET_RECOVERY_BLOCKS`] valid blocks in a
//! row. Until then, and whenever a block is missing, it plays its input delayed by the lane's
//! latency (dry), or silence when the lane has no dry fallback or the latency is over
//! [`MAX_DRY_DELAY_FRAMES`]. Every change of source is a linear fade.
//!
//! Construction allocates the dry ring. [`LaneFader::render`] does not allocate, lock, or
//! block.

use crate::{LANE_CHANNELS, LANE_SAMPLES, MAX_LANE_FRAMES};

/// Largest latency the preallocated dry path can match exactly. A lane with a higher latency
/// plays silence on its dry paths.
pub const MAX_DRY_DELAY_FRAMES: usize = 65_536;
/// Minimum click-bounded transition length.
pub const MIN_TRANSITION_FRAMES: usize = 64;
/// Maximum click-bounded transition length.
pub const MAX_TRANSITION_FRAMES: usize = 128;
/// Fade length when a lane's worker is replaced, both to dry and to the new chain.
pub const JOIN_FADE_FRAMES: usize = 16;
/// Number of consecutive valid wet blocks required after a fallback before wet recovery.
pub const WET_RECOVERY_BLOCKS: u8 = 3;
// The fader writes an entire block before it reads delayed frames, so the ring needs one full
// block beyond the maximum supported delay: about 0.5 MB per stereo lane, allocated when the
// fader is built.
const DRY_DELAY_CAPACITY_FRAMES: usize = MAX_DRY_DELAY_FRAMES + MAX_LANE_FRAMES;

/// Channel layout of a lane's dry input or of its wet output. Buffers are always interleaved
/// stereo; a mono layout uses the left lane.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaneLayout {
    /// One channel, in the left lane.
    Mono,
    /// Two channels.
    Stereo,
}

/// What a lane plays while its wet output is not accepted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaneFallback {
    /// The input delayed by the lane's latency.
    Dry,
    /// Silence (instruments, or a lane without dry fallback).
    Silence,
}

/// Lane settings for one block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FaderSettings {
    /// Layout of the dry input.
    pub source: LaneLayout,
    /// Layout in which the wet output is read.
    pub endpoint: LaneLayout,
    /// Play the latency-matched dry path instead of wet output.
    pub bypassed: bool,
    /// The lane's latency: the dry path's delay.
    pub latency_frames: usize,
    /// Whether the lane may play dry audio while wet output warms up.
    pub dry_fallback: bool,
    /// Length of an ordinary dry/wet fade.
    pub transition_frames: usize,
}

/// One block of a lane's signals.
#[derive(Clone, Copy, Debug)]
pub struct FaderBlock<'a> {
    /// Interleaved stereo input of this block: at least `frames * 2` samples.
    pub dry: &'a [f32],
    /// Interleaved stereo worker output of this block, if the lane has one.
    pub wet: Option<&'a [f32]>,
    /// Whether the lane's gate accepted `wet` (or may dispatch) this block.
    pub accepted: bool,
    /// What plays while wet output is not accepted.
    pub fallback: LaneFallback,
    /// Frames in this block, at most [`MAX_LANE_FRAMES`].
    pub frames: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RenderSource {
    Wet,
    Dry,
    Silence,
    /// The last frame a replaced plug-in chain played, held and faded out during a worker
    /// handoff so the join to the new source is continuous.
    Held,
}

#[derive(Clone, Copy, Debug)]
struct Transition {
    from: RenderSource,
    position: usize,
    frames: usize,
}

impl Transition {
    const fn idle() -> Self {
        Self {
            from: RenderSource::Silence,
            position: 0,
            frames: 0,
        }
    }

    const fn active(self) -> bool {
        self.position < self.frames
    }
}

/// The dry delay and recovery fades of one lane.
pub struct LaneFader {
    dry: Box<[f32]>,
    dry_write_frame: usize,
    transition: Transition,
    rendered: RenderSource,
    valid_wet_blocks: u8,
    /// A new worker joined; its wet output fades in over [`JOIN_FADE_FRAMES`].
    joining: bool,
    last_wet: Box<[f32]>,
    /// Last frame this lane rendered.
    last_frame: [f32; LANE_CHANNELS],
}

impl LaneFader {
    /// Creates a silent fader. Allocates the dry ring; call it off the real-time thread.
    #[must_use]
    pub fn new() -> Self {
        Self {
            dry: vec![0.0; DRY_DELAY_CAPACITY_FRAMES * LANE_CHANNELS].into_boxed_slice(),
            dry_write_frame: 0,
            transition: Transition::idle(),
            rendered: RenderSource::Silence,
            valid_wet_blocks: 0,
            joining: false,
            last_wet: vec![0.0; LANE_SAMPLES].into_boxed_slice(),
            last_frame: [0.0; LANE_CHANNELS],
        }
    }

    /// Clears all history for a new lane, which starts silent and fades in. No allocation.
    pub fn reset(&mut self) {
        self.dry.fill(0.0);
        self.dry_write_frame = 0;
        self.transition = Transition::idle();
        self.rendered = RenderSource::Silence;
        self.valid_wet_blocks = 0;
        self.joining = true;
        self.last_wet.fill(0.0);
        self.last_frame = [0.0; LANE_CHANNELS];
    }

    /// The lane's worker was replaced: fade from the last frame to dry (or silence without dry
    /// fallback), then to the new worker's output once it delivers [`WET_RECOVERY_BLOCKS`]
    /// valid blocks. Both fades last [`JOIN_FADE_FRAMES`]. No allocation.
    pub fn restart(&mut self, dry_fallback: bool) {
        self.valid_wet_blocks = 0;
        self.joining = true;
        self.transition = Transition {
            from: RenderSource::Held,
            position: 0,
            frames: JOIN_FADE_FRAMES,
        };
        self.rendered = if dry_fallback {
            RenderSource::Dry
        } else {
            RenderSource::Silence
        };
    }

    /// Whether the lane plays its worker's output (after any fade in progress).
    #[must_use]
    pub fn is_wet(&self) -> bool {
        self.rendered == RenderSource::Wet
    }

    /// Renders one block into `out` (interleaved stereo, `frames * 2` samples): advances the
    /// dry path, chooses the source, and fades from the previous one. Real-time safe.
    ///
    /// # Panics
    /// If `frames` is over [`MAX_LANE_FRAMES`] or `block.dry` or `out` is shorter than
    /// `frames * 2`.
    pub fn render(&mut self, block: FaderBlock<'_>, settings: &FaderSettings, out: &mut [f32]) {
        self.advance_dry(block.dry, block.frames);
        let target = self.select_source(block, settings);
        self.begin_source_change(target, settings.transition_frames);
        self.capture_wet(settings.endpoint, block.wet, block.frames);
        self.render_frames(block, settings, target, out);
    }

    fn select_source(&mut self, block: FaderBlock<'_>, settings: &FaderSettings) -> RenderSource {
        let valid_wet = block.wet.is_some() && block.accepted;
        if settings.bypassed {
            return RenderSource::Dry;
        }
        if valid_wet {
            self.valid_wet_blocks = self.valid_wet_blocks.saturating_add(1);
            if self.rendered == RenderSource::Wet || self.valid_wet_blocks >= WET_RECOVERY_BLOCKS {
                RenderSource::Wet
            } else if settings.dry_fallback {
                RenderSource::Dry
            } else {
                RenderSource::Silence
            }
        } else {
            self.valid_wet_blocks = 0;
            match block.fallback {
                LaneFallback::Silence => RenderSource::Silence,
                LaneFallback::Dry => RenderSource::Dry,
            }
        }
    }

    /// Fades to a new source. A joining worker's chain uses the shorter join fade.
    fn begin_source_change(&mut self, target: RenderSource, transition_frames: usize) {
        if self.rendered == target || self.transition.active() {
            return;
        }
        self.transition = Transition {
            from: self.rendered,
            position: 0,
            frames: if self.joining && target == RenderSource::Wet {
                JOIN_FADE_FRAMES
            } else {
                transition_frames
            },
        };
    }

    fn capture_wet(&mut self, endpoint: LaneLayout, wet: Option<&[f32]>, frames: usize) {
        let Some(samples) = wet else {
            return;
        };
        for frame in 0..frames {
            let destination = frame * LANE_CHANNELS;
            match endpoint {
                LaneLayout::Mono => {
                    let sample = samples.get(destination).copied().unwrap_or(0.0);
                    self.last_wet[destination] = sample;
                    self.last_wet[destination + 1] = sample;
                }
                LaneLayout::Stereo => {
                    self.last_wet[destination] = samples.get(destination).copied().unwrap_or(0.0);
                    self.last_wet[destination + 1] =
                        samples.get(destination + 1).copied().unwrap_or(0.0);
                }
            }
        }
    }

    fn render_frames(
        &mut self,
        block: FaderBlock<'_>,
        settings: &FaderSettings,
        target: RenderSource,
        out: &mut [f32],
    ) {
        let transition = self.transition;
        for frame in 0..block.frames {
            let (from_l, from_r) = self.source_frame(block, settings, transition.from, frame);
            let (to_l, to_r) = self.source_frame(block, settings, target, frame);
            let (left, right) = if transition.active() {
                #[allow(
                    clippy::cast_precision_loss,
                    reason = "transition positions are bounded well below f32 precision limits"
                )]
                let amount = (transition.position + frame).min(transition.frames) as f32
                    / transition.frames as f32;
                (
                    from_l + (to_l - from_l) * amount,
                    from_r + (to_r - from_r) * amount,
                )
            } else {
                (to_l, to_r)
            };
            out[frame * 2] = left;
            out[frame * 2 + 1] = right;
        }
        if block.frames > 0 {
            let last = (block.frames - 1) * 2;
            self.last_frame = [out[last], out[last + 1]];
        }
        if transition.active() {
            let updated = transition.position.saturating_add(block.frames);
            if updated >= transition.frames {
                self.transition = Transition::idle();
                self.rendered = target;
            } else {
                self.transition.position = updated;
            }
        } else {
            self.rendered = target;
        }
        if self.rendered == RenderSource::Wet {
            self.joining = false;
        }
    }

    fn source_frame(
        &self,
        block: FaderBlock<'_>,
        settings: &FaderSettings,
        source: RenderSource,
        frame: usize,
    ) -> (f32, f32) {
        match source {
            RenderSource::Silence => (0.0, 0.0),
            RenderSource::Held => (self.last_frame[0], self.last_frame[1]),
            RenderSource::Dry => self.dry_frame(settings, frame, block.frames),
            RenderSource::Wet => {
                let index = frame * LANE_CHANNELS;
                // Without this block's output, a fade from wet reads the last one.
                let samples = block.wet.unwrap_or(&self.last_wet);
                let sample = |index: usize| samples.get(index).copied().unwrap_or(0.0);
                match (block.wet, settings.endpoint) {
                    (Some(_), LaneLayout::Mono) => (sample(index), sample(index)),
                    _ => (sample(index), sample(index + 1)),
                }
            }
        }
    }

    fn advance_dry(&mut self, input: &[f32], frames: usize) {
        let mut write = self.dry_write_frame;
        for frame in 0..frames {
            let index = frame * 2;
            let write_index = write * 2;
            self.dry[write_index] = input[index];
            self.dry[write_index + 1] = input[index + 1];
            write = (write + 1) % DRY_DELAY_CAPACITY_FRAMES;
        }
        self.dry_write_frame = write;
    }

    fn dry_frame(&self, settings: &FaderSettings, frame: usize, frames: usize) -> (f32, f32) {
        if settings.latency_frames > MAX_DRY_DELAY_FRAMES {
            return (0.0, 0.0);
        }
        let written = (self.dry_write_frame + DRY_DELAY_CAPACITY_FRAMES - frames + frame)
            % DRY_DELAY_CAPACITY_FRAMES;
        let read = (written + DRY_DELAY_CAPACITY_FRAMES - settings.latency_frames)
            % DRY_DELAY_CAPACITY_FRAMES;
        let base = read * 2;
        let left = self.dry[base];
        let right = self.dry[base + 1];
        match settings.source {
            LaneLayout::Stereo => (left, right),
            LaneLayout::Mono => (left, left),
        }
    }
}

impl Default for LaneFader {
    fn default() -> Self {
        Self::new()
    }
}
