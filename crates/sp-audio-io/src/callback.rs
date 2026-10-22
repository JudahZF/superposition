//! Real-time callback contracts shared by every device backend.

use std::{error::Error, fmt};

/// Supported fixed callback sizes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockFrames {
    /// A callback contains 32 frames.
    Frames32,
    /// A callback contains 64 frames.
    Frames64,
    /// A callback contains 128 stereo frames.
    Frames128,
    /// A callback contains 256 stereo frames.
    Frames256,
}

impl BlockFrames {
    /// Returns this callback size as a frame count.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        match self {
            Self::Frames32 => 32,
            Self::Frames64 => 64,
            Self::Frames128 => 128,
            Self::Frames256 => 256,
        }
    }

    /// Returns the callback size for a supported frame count.
    #[must_use]
    pub fn from_u32(frames: u32) -> Option<Self> {
        match frames {
            32 => Some(Self::Frames32),
            64 => Some(Self::Frames64),
            128 => Some(Self::Frames128),
            256 => Some(Self::Frames256),
            _ => None,
        }
    }

    /// Returns the stereo sample count of one interleaved callback buffer.
    #[must_use]
    pub const fn interleaved_sample_count(self) -> usize {
        match self {
            Self::Frames32 => 64,
            Self::Frames64 => 128,
            Self::Frames128 => 256,
            Self::Frames256 => 512,
        }
    }
}

/// Whether a renderer supplied sound or intentionally left its output silent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderDisposition {
    /// The renderer filled the whole supplied buffer with output samples.
    Rendered,
    /// The callback must leave the complete supplied buffer silent.
    Silence,
}

/// A validated mutable interleaved stereo `f32` output buffer.
pub struct InterleavedStereoF32<'a> {
    samples: &'a mut [f32],
    frames: BlockFrames,
}

impl<'a> InterleavedStereoF32<'a> {
    /// Validates an interleaved sample slice for the supplied callback size.
    ///
    /// # Errors
    ///
    /// Returns [`InterleavedStereoF32Error`] when `samples` does not contain exactly two
    /// samples for every requested frame.
    pub fn new(
        samples: &'a mut [f32],
        frames: BlockFrames,
    ) -> Result<Self, InterleavedStereoF32Error> {
        let expected_samples = frames.interleaved_sample_count();
        if samples.len() != expected_samples {
            return Err(InterleavedStereoF32Error::SampleCount {
                expected: expected_samples,
                actual: samples.len(),
            });
        }
        Ok(Self { samples, frames })
    }

    /// Returns the validated callback size.
    #[must_use]
    pub const fn frames(&self) -> BlockFrames {
        self.frames
    }

    /// Returns the mutable interleaved sample storage.
    pub fn samples_mut(&mut self) -> &mut [f32] {
        self.samples
    }
}

/// A failure to construct [`InterleavedStereoF32`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterleavedStereoF32Error {
    /// The provided storage did not have exactly two samples per frame.
    SampleCount {
        /// The required sample count.
        expected: usize,
        /// The supplied sample count.
        actual: usize,
    },
}

impl fmt::Display for InterleavedStereoF32Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SampleCount { expected, actual } => write!(
                formatter,
                "expected {expected} interleaved stereo samples, got {actual}"
            ),
        }
    }
}

impl Error for InterleavedStereoF32Error {}

/// Supplies samples to the [`ActiveOutput`] real-time render callback.
///
/// Implementations execute on the backend's real-time thread. They must not allocate, lock,
/// log, wait, panic, or otherwise perform an operation that can block. Returning
/// [`RenderDisposition::Rendered`] promises that every sample in `output` has been written.
/// A panic cannot unwind across this non-unwinding C callback boundary and aborts the process;
/// the backend deliberately does not run panic recovery machinery on the real-time thread.
pub trait OutputRenderer: Send + 'static {
    /// Renders a single validated interleaved stereo callback buffer.
    fn render(&mut self, output: InterleavedStereoF32<'_>) -> RenderDisposition;
}

/// One borrowed frame-interleaved hardware callback block.
pub struct MultiChannelDuplexF32<'a> {
    input: &'a [f32],
    output: &'a mut [f32],
    input_channels: u16,
    output_channels: u16,
    frames: BlockFrames,
}

impl<'a> MultiChannelDuplexF32<'a> {
    /// Wraps frame-major callback buffers. Returns `None` unless each slice holds exactly
    /// `frames` × its channel count samples. Does not allocate.
    #[must_use]
    pub fn new(
        input: &'a [f32],
        output: &'a mut [f32],
        input_channels: u16,
        output_channels: u16,
        frames: BlockFrames,
    ) -> Option<Self> {
        let frame_count = frames.as_u32() as usize;
        (input.len() == frame_count * usize::from(input_channels)
            && output.len() == frame_count * usize::from(output_channels))
        .then_some(Self {
            input,
            output,
            input_channels,
            output_channels,
            frames,
        })
    }

    /// Captured samples in frame-major channel order. Empty on an output-only route.
    pub fn input(&self) -> &[f32] {
        self.input
    }
    /// Playback samples in frame-major channel order.
    pub fn output_mut(&mut self) -> &mut [f32] {
        self.output
    }
    /// The number of channels in each captured frame.
    pub const fn input_channels(&self) -> u16 {
        self.input_channels
    }
    /// The number of channels in each playback frame.
    pub const fn output_channels(&self) -> u16 {
        self.output_channels
    }
    /// The number of frames in this callback.
    pub const fn frames(&self) -> BlockFrames {
        self.frames
    }
    /// Splits the block for renderers that need simultaneous input and output access.
    pub fn into_parts(self) -> (&'a [f32], &'a mut [f32], u16, u16, BlockFrames) {
        (
            self.input,
            self.output,
            self.input_channels,
            self.output_channels,
            self.frames,
        )
    }
}

/// Renders all requested physical playback channels from captured hardware channels.
/// Called on the backend's real-time thread; implementations must not allocate, lock, or block.
pub trait MultiChannelDuplexRenderer: Send + 'static {
    /// Processes one fixed-size block.
    fn render(&mut self, block: MultiChannelDuplexF32<'_>) -> RenderDisposition;
}

/// A coherent fixed-counter callback telemetry snapshot.
///
/// The backend render callback is the only sequence writer; control-thread code only reads snapshots.
/// A lock-free writer guard rejects a concurrent or reentrant callback before Rust is called, so
/// each accepted callback has exactly one serialized writer. Readers retry until they observe one
/// completed sequence, so every returned value satisfies
/// `callbacks == rendered + silenced + invalid_*`. The 128/256 histogram counts only valid
/// buffers and therefore sums to `rendered + silenced`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CallbackTelemetry {
    /// Number of serialized callbacks accepted by the telemetry writer guard.
    pub callbacks: u64,
    /// Number of callbacks whose renderer returned [`RenderDisposition::Rendered`].
    pub rendered: u64,
    /// Number of callbacks left silent by the renderer.
    pub silenced: u64,
    /// Number of callbacks rejected for an unexpected frame count.
    pub invalid_frames: u64,
    /// Number of callbacks rejected for a null or non-single buffer list.
    pub invalid_buffers: u64,
    /// Number of callbacks rejected for a non-stereo buffer.
    pub invalid_channels: u64,
    /// Number of callbacks rejected for a non-exact byte count.
    pub invalid_bytes: u64,
    /// Valid 32-frame callback count.
    pub frame_histogram_32: u64,
    /// Valid 64-frame callback count.
    pub frame_histogram_64: u64,
    /// Valid 128-frame callback count.
    pub frame_histogram_128: u64,
    /// Valid 256-frame callback count.
    pub frame_histogram_256: u64,
}

impl CallbackTelemetry {
    /// Returns whether the snapshot satisfies the C writer's classification invariants.
    #[must_use]
    pub const fn is_coherent(self) -> bool {
        self.callbacks
            == self
                .rendered
                .wrapping_add(self.silenced)
                .wrapping_add(self.invalid_frames)
                .wrapping_add(self.invalid_buffers)
                .wrapping_add(self.invalid_channels)
                .wrapping_add(self.invalid_bytes)
            && self
                .frame_histogram_32
                .wrapping_add(self.frame_histogram_64)
                .wrapping_add(self.frame_histogram_128)
                .wrapping_add(self.frame_histogram_256)
                == self.rendered.wrapping_add(self.silenced)
    }
}
