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
        copy_wet(
            samples,
            endpoint == LaneLayout::Mono,
            &mut self.last_wet[..frames * LANE_CHANNELS],
        );
    }

    /// Renders a block: each source is written whole, and only a fade blends per frame.
    fn render_frames(
        &mut self,
        block: FaderBlock<'_>,
        settings: &FaderSettings,
        target: RenderSource,
        out: &mut [f32],
    ) {
        let transition = self.transition;
        let out = &mut out[..block.frames * LANE_CHANNELS];
        self.fill_source(block, settings, target, out);
        if transition.active() {
            let mut from = [0.0; LANE_SAMPLES];
            let from = &mut from[..out.len()];
            self.fill_source(block, settings, transition.from, from);
            for (frame, (to, from)) in out
                .chunks_exact_mut(LANE_CHANNELS)
                .zip(from.chunks_exact(LANE_CHANNELS))
                .enumerate()
            {
                #[allow(
                    clippy::cast_precision_loss,
                    reason = "transition positions are bounded well below f32 precision limits"
                )]
                let amount = (transition.position + frame).min(transition.frames) as f32
                    / transition.frames as f32;
                to[0] = from[0] + (to[0] - from[0]) * amount;
                to[1] = from[1] + (to[1] - from[1]) * amount;
            }
        }
        if let [.., left, right] = *out {
            self.last_frame = [left, right];
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

    /// Writes one whole block of `source` into `out` (`frames * 2` samples).
    fn fill_source(
        &self,
        block: FaderBlock<'_>,
        settings: &FaderSettings,
        source: RenderSource,
        out: &mut [f32],
    ) {
        match source {
            RenderSource::Silence => out.fill(0.0),
            RenderSource::Held => {
                for frame in out.chunks_exact_mut(LANE_CHANNELS) {
                    frame.copy_from_slice(&self.last_frame);
                }
            }
            RenderSource::Dry => self.fill_dry(settings, block.frames, out),
            // Without this block's output, a fade from wet reads the last one, which
            // `capture_wet` already stored in stereo.
            RenderSource::Wet => match block.wet {
                Some(samples) => copy_wet(samples, settings.endpoint == LaneLayout::Mono, out),
                None => copy_wet(&self.last_wet, false, out),
            },
        }
    }

    /// Writes the block's input into the dry ring, in at most two contiguous copies.
    fn advance_dry(&mut self, input: &[f32], frames: usize) {
        let input = &input[..frames * LANE_CHANNELS];
        let write = self.dry_write_frame;
        let (head, tail) = input.split_at(frames.min(DRY_DELAY_CAPACITY_FRAMES - write) * 2);
        self.dry[write * 2..write * 2 + head.len()].copy_from_slice(head);
        self.dry[..tail.len()].copy_from_slice(tail);
        self.dry_write_frame = (write + frames) % DRY_DELAY_CAPACITY_FRAMES;
    }

    /// Reads the block written last, delayed by the lane's latency, in at most two segments.
    fn fill_dry(&self, settings: &FaderSettings, frames: usize, out: &mut [f32]) {
        if settings.latency_frames > MAX_DRY_DELAY_FRAMES {
            out.fill(0.0);
            return;
        }
        let read = (self.dry_write_frame + 2 * DRY_DELAY_CAPACITY_FRAMES
            - frames
            - settings.latency_frames)
            % DRY_DELAY_CAPACITY_FRAMES;
        let head_frames = frames.min(DRY_DELAY_CAPACITY_FRAMES - read);
        let (out_head, out_tail) = out.split_at_mut(head_frames * 2);
        let head = &self.dry[read * 2..read * 2 + out_head.len()];
        let tail = &self.dry[..out_tail.len()];
        match settings.source {
            LaneLayout::Stereo => {
                out_head.copy_from_slice(head);
                out_tail.copy_from_slice(tail);
            }
            LaneLayout::Mono => {
                for (out, ring) in [(out_head, head), (out_tail, tail)] {
                    for (frame, stored) in out
                        .chunks_exact_mut(LANE_CHANNELS)
                        .zip(ring.chunks_exact(LANE_CHANNELS))
                    {
                        frame.fill(stored[0]);
                    }
                }
            }
        }
    }
}

/// Copies interleaved wet output into `out`, reading samples past the end of `samples` as
/// silence. A mono endpoint plays its left lane on both channels.
fn copy_wet(samples: &[f32], mono: bool, out: &mut [f32]) {
    if mono {
        let mut frames = out.chunks_exact_mut(LANE_CHANNELS);
        // The source goes first: `zip` must not take a frame once the source has run out.
        for (source, frame) in samples.chunks(LANE_CHANNELS).zip(frames.by_ref()) {
            frame.fill(source[0]);
        }
        frames.for_each(|frame| frame.fill(0.0));
    } else {
        let available = samples.len().min(out.len());
        out[..available].copy_from_slice(&samples[..available]);
        out[available..].fill(0.0);
    }
}

impl Default for LaneFader {
    fn default() -> Self {
        Self::new()
    }
}
