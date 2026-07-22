//! Fixed-capacity VST3 rack hosting contracts.
//!
//! This module deliberately contains no `vst3` or `vst3-host` types.  The worker owns an
//! implementation of [`RackPluginAdapter`]; the render path owns only the bounded data types in
//! this module.  That keeps the SDK replaceable and makes lifecycle/layout behavior testable
//! without loading a third-party plug-in.

use std::error::Error;
use std::fmt;

use crate::Vst3BundlePath;

/// Maximum number of serial plug-in slots supported by one alpha rack.
pub const MAX_SERIAL_RACK_SLOTS: usize = 8;
/// Maximum frames accepted by the fixed VST3 processing buffers.
pub const MAX_RACK_FRAMES: usize = 256;
/// Maximum MIDI events accepted or emitted by one slot per block.
pub const MAX_RACK_MIDI_EVENTS: usize = 256;
/// Maximum parameter changes accepted or emitted by one slot per block.
pub const MAX_RACK_PARAMETER_CHANGES: usize = 256;
/// Maximum combined output changes retained for one rack block.
pub const MAX_RACK_OUTPUT_CHANGES: usize = 512;

/// One class selected from a VST3 module.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Vst3ClassSelection {
    /// Bundle that owns the selected class.
    pub bundle: Vst3BundlePath,
    /// Stable VST3 class identifier supplied by module enumeration.
    pub class_id: String,
}

impl Vst3ClassSelection {
    /// Creates an explicit module/class selection.
    #[must_use]
    pub fn new(bundle: Vst3BundlePath, class_id: impl Into<String>) -> Self {
        Self {
            bundle,
            class_id: class_id.into(),
        }
    }
}

/// Class metadata returned by an isolated module enumeration operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Vst3ClassDescriptor {
    /// Stable VST3 class identifier.
    pub class_id: String,
    /// User-visible class name.
    pub name: String,
    /// SDK category such as `Audio Module Class`.
    pub category: String,
    /// Vendor supplied version string, if available.
    pub version: String,
}

/// A fixed processing format negotiated before a plug-in is activated.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProcessingFormat {
    /// Fixed sample rate; alpha workers currently use 48 kHz.
    pub sample_rate_hz: f64,
    /// Maximum frames in any process call.
    pub maximum_frames: usize,
}

impl ProcessingFormat {
    /// Creates a valid processing format.
    ///
    /// # Errors
    ///
    /// Returns [`LayoutError::InvalidProcessingFormat`] for an invalid sample rate or block
    /// capacity.
    pub fn new(sample_rate_hz: f64, maximum_frames: usize) -> Result<Self, LayoutError> {
        if !(sample_rate_hz.is_finite() && sample_rate_hz > 0.0) || maximum_frames == 0 {
            return Err(LayoutError::InvalidProcessingFormat {
                sample_rate_hz,
                maximum_frames,
            });
        }
        Ok(Self {
            sample_rate_hz,
            maximum_frames,
        })
    }
}

/// The fixed main-bus arrangement supported by an alpha rack.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MainBusLayout {
    /// Main audio input channel count: 0 for an instrument, 1 for mono, or 2 for stereo.
    pub input_channels: u8,
    /// Main audio output channel count: 1 for mono or 2 for stereo.
    pub output_channels: u8,
    /// Whether the single main event input bus is active.
    pub event_input_active: bool,
}

impl MainBusLayout {
    /// Creates and validates a fixed main-bus layout.
    ///
    /// # Errors
    ///
    /// Returns [`LayoutError::UnsupportedChannelCount`] for layouts outside the alpha contract.
    pub const fn new(
        input_channels: u8,
        output_channels: u8,
        event_input_active: bool,
    ) -> Result<Self, LayoutError> {
        if input_channels > 2 {
            return Err(LayoutError::UnsupportedChannelCount {
                direction: BusDirection::Input,
                channels: input_channels,
            });
        }
        if output_channels != 1 && output_channels != 2 {
            return Err(LayoutError::UnsupportedChannelCount {
                direction: BusDirection::Output,
                channels: output_channels,
            });
        }
        Ok(Self {
            input_channels,
            output_channels,
            event_input_active,
        })
    }
}

/// Direction of a VST3 bus.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BusDirection {
    /// A plug-in input bus.
    Input,
    /// A plug-in output bus.
    Output,
}

/// Media transported by a VST3 bus.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BusMedia {
    /// Planar `f32` audio.
    Audio,
    /// VST events, including MIDI 1.0 messages.
    Event,
}

/// VST3 bus role.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BusRole {
    /// The one routable main bus.
    Main,
    /// An auxiliary/sidechain bus that alpha racks reject.
    Auxiliary,
}

/// One discovered VST3 bus.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Vst3BusDescriptor {
    /// Whether the bus is audio or event media.
    pub media: BusMedia,
    /// Whether the bus is an input or output.
    pub direction: BusDirection,
    /// Whether the bus is main or auxiliary.
    pub role: BusRole,
    /// Audio channels on this bus; event buses always use zero.
    pub channel_count: u8,
}

impl Vst3BusDescriptor {
    /// Creates a discovered audio bus.
    #[must_use]
    pub const fn audio(direction: BusDirection, role: BusRole, channel_count: u8) -> Self {
        Self {
            media: BusMedia::Audio,
            direction,
            role,
            channel_count,
        }
    }

    /// Creates a discovered event bus.
    #[must_use]
    pub const fn event(direction: BusDirection, role: BusRole) -> Self {
        Self {
            media: BusMedia::Event,
            direction,
            role,
            channel_count: 0,
        }
    }
}

/// Complete bus discovery result for a selected VST3 component.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Vst3BusTopology {
    /// Audio input buses in component order.
    pub audio_inputs: Vec<Vst3BusDescriptor>,
    /// Audio output buses in component order.
    pub audio_outputs: Vec<Vst3BusDescriptor>,
    /// Event input buses in component order.
    pub event_inputs: Vec<Vst3BusDescriptor>,
    /// Event output buses in component order.
    pub event_outputs: Vec<Vst3BusDescriptor>,
}

impl Vst3BusTopology {
    /// Validates the alpha fixed-main-bus contract after bus negotiation.
    ///
    /// A component may expose no audio input (instrument), but if it exposes one it must be the
    /// sole main input. Exactly one main audio output is required. At most one main event input
    /// and output are permitted. Any auxiliary bus is rejected instead of silently disabling a
    /// sidechain or switching layouts at runtime.
    ///
    /// # Errors
    ///
    /// Returns a detailed [`LayoutError`] for unsupported or dynamically-changing topology.
    pub fn validate_fixed(&self, expected: MainBusLayout) -> Result<(), LayoutError> {
        validate_audio_buses(
            &self.audio_inputs,
            BusDirection::Input,
            expected.input_channels,
            true,
        )?;
        validate_audio_buses(
            &self.audio_outputs,
            BusDirection::Output,
            expected.output_channels,
            false,
        )?;
        validate_event_buses(
            &self.event_inputs,
            BusDirection::Input,
            expected.event_input_active,
        )?;
        validate_event_buses(&self.event_outputs, BusDirection::Output, false)?;
        Ok(())
    }
}

fn validate_audio_buses(
    buses: &[Vst3BusDescriptor],
    direction: BusDirection,
    expected_channels: u8,
    input_can_be_absent: bool,
) -> Result<(), LayoutError> {
    if buses.iter().any(|bus| {
        bus.media != BusMedia::Audio || bus.direction != direction || bus.role != BusRole::Main
    }) {
        return Err(LayoutError::SidechainOrAuxiliaryBus { direction });
    }
    if buses.is_empty() && input_can_be_absent && expected_channels == 0 {
        return Ok(());
    }
    if buses.len() != 1 {
        return Err(LayoutError::UnexpectedBusCount {
            media: BusMedia::Audio,
            direction,
            expected: 1,
            actual: buses.len(),
        });
    }
    let actual_channels = buses[0].channel_count;
    if actual_channels != expected_channels {
        return Err(LayoutError::NegotiatedChannelMismatch {
            direction,
            expected: expected_channels,
            actual: actual_channels,
        });
    }
    Ok(())
}

fn validate_event_buses(
    buses: &[Vst3BusDescriptor],
    direction: BusDirection,
    should_be_active: bool,
) -> Result<(), LayoutError> {
    if buses.iter().any(|bus| {
        bus.media != BusMedia::Event || bus.direction != direction || bus.role != BusRole::Main
    }) {
        return Err(LayoutError::SidechainOrAuxiliaryBus { direction });
    }
    if buses.len() > 1 {
        return Err(LayoutError::UnexpectedBusCount {
            media: BusMedia::Event,
            direction,
            expected: 1,
            actual: buses.len(),
        });
    }
    if should_be_active && buses.is_empty() {
        return Err(LayoutError::MissingEventInput);
    }
    Ok(())
}

/// An unsupported fixed-bus or processing-format request.
#[derive(Clone, Debug, PartialEq)]
pub enum LayoutError {
    /// Input or output channels exceed the supported alpha contract.
    UnsupportedChannelCount {
        /// The main-bus direction with the invalid count.
        direction: BusDirection,
        /// Requested number of channels.
        channels: u8,
    },
    /// The adapter reported an auxiliary or malformed bus where a single main bus is required.
    SidechainOrAuxiliaryBus {
        /// Direction of the unsupported bus.
        direction: BusDirection,
    },
    /// The number of discovered buses does not match the fixed contract.
    UnexpectedBusCount {
        /// Media type whose bus count is incompatible.
        media: BusMedia,
        /// Direction whose bus count is incompatible.
        direction: BusDirection,
        /// Expected bus count.
        expected: usize,
        /// Actual bus count.
        actual: usize,
    },
    /// A plugin exposes no main event input despite an active event-input request.
    MissingEventInput,
    /// A negotiated main bus did not retain the requested channel count.
    NegotiatedChannelMismatch {
        /// Main-bus direction with an incompatible result.
        direction: BusDirection,
        /// Host requested channel count.
        expected: u8,
        /// Component's negotiated channel count.
        actual: u8,
    },
    /// The worker attempted a malformed processing format.
    InvalidProcessingFormat {
        /// Requested sample rate.
        sample_rate_hz: f64,
        /// Requested maximum frames.
        maximum_frames: usize,
    },
}

impl fmt::Display for LayoutError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedChannelCount {
                direction,
                channels,
            } => write!(
                formatter,
                "unsupported {direction:?} main-bus channel count {channels}"
            ),
            Self::SidechainOrAuxiliaryBus { direction } => write!(
                formatter,
                "sidechain, auxiliary, or malformed {direction:?} bus is unsupported"
            ),
            Self::UnexpectedBusCount {
                media,
                direction,
                expected,
                actual,
            } => write!(
                formatter,
                "unsupported {media:?} {direction:?} bus count {actual}; expected {expected}"
            ),
            Self::MissingEventInput => {
                formatter.write_str("requested event input is not available")
            }
            Self::NegotiatedChannelMismatch {
                direction,
                expected,
                actual,
            } => write!(
                formatter,
                "{direction:?} main bus negotiated {actual} channels; expected {expected}"
            ),
            Self::InvalidProcessingFormat {
                sample_rate_hz,
                maximum_frames,
            } => write!(
                formatter,
                "invalid processing format {sample_rate_hz} Hz / {maximum_frames} frames"
            ),
        }
    }
}

impl Error for LayoutError {}

/// A MIDI 1.0 message represented without SDK types.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MidiMessage {
    /// A note-on message.
    NoteOn {
        /// Zero-based MIDI channel.
        channel: u8,
        /// MIDI note number.
        note: u8,
        /// MIDI velocity.
        velocity: u8,
    },
    /// A note-off message.
    NoteOff {
        /// Zero-based MIDI channel.
        channel: u8,
        /// MIDI note number.
        note: u8,
        /// MIDI release velocity.
        velocity: u8,
    },
    /// A MIDI continuous controller message.
    ControlChange {
        /// Zero-based MIDI channel.
        channel: u8,
        /// MIDI CC number.
        controller: u8,
        /// MIDI CC value.
        value: u8,
    },
    /// A 14-bit pitch bend value.
    PitchBend {
        /// Zero-based MIDI channel.
        channel: u8,
        /// Pitch bend in `0..=16383`.
        value: u16,
    },
    /// Channel pressure / aftertouch.
    ChannelPressure {
        /// Zero-based MIDI channel.
        channel: u8,
        /// MIDI pressure value.
        pressure: u8,
    },
    /// A MIDI program-change message.
    ProgramChange {
        /// Zero-based MIDI channel.
        channel: u8,
        /// MIDI program number.
        program: u8,
    },
}

impl MidiMessage {
    /// Returns whether this message must displace a less-important event when the queue is full.
    #[must_use]
    pub const fn is_note_off(self) -> bool {
        matches!(self, Self::NoteOff { .. })
    }

    const fn is_continuous(self) -> bool {
        matches!(
            self,
            Self::ControlChange { .. } | Self::PitchBend { .. } | Self::ChannelPressure { .. }
        )
    }
}

/// One sample-accurate MIDI event for a processing block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimedMidiMessage {
    /// MIDI message payload.
    pub message: MidiMessage,
    /// Sample offset within the current block.
    pub sample_offset: u16,
}

/// A bounded, allocation-free MIDI event queue.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundedMidiEvents<const CAPACITY: usize> {
    entries: [Option<TimedMidiMessage>; CAPACITY],
    len: usize,
}

impl<const CAPACITY: usize> Default for BoundedMidiEvents<CAPACITY> {
    fn default() -> Self {
        Self {
            entries: [None; CAPACITY],
            len: 0,
        }
    }
}

impl<const CAPACITY: usize> BoundedMidiEvents<CAPACITY> {
    /// Returns the queued event count.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns whether no event is queued.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Clears queued events while retaining the fixed backing storage.
    pub fn clear(&mut self) {
        self.entries[..self.len].fill(None);
        self.len = 0;
    }

    /// Returns queued events in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = &TimedMidiMessage> {
        self.entries[..self.len].iter().flatten()
    }

    /// Adds a MIDI event without allocating.
    ///
    /// On saturation a note-off replaces the earliest continuous event, if one exists. This
    /// protects note release from controller floods. Other events are rejected so the caller can
    /// increment a bounded-drop diagnostic.
    ///
    /// # Errors
    ///
    /// Returns [`QueueFull`] when the storage is full and no continuous event can be displaced.
    pub fn push_preserving_note_off(&mut self, event: TimedMidiMessage) -> Result<(), QueueFull> {
        if self.len < CAPACITY {
            self.entries[self.len] = Some(event);
            self.len += 1;
            return Ok(());
        }
        if event.message.is_note_off()
            && let Some(index) = self.entries[..self.len]
                .iter()
                .position(|entry| entry.is_some_and(|entry| entry.message.is_continuous()))
        {
            self.entries[index] = Some(event);
            return Ok(());
        }
        Err(QueueFull)
    }
}

/// A sample-accurate normalized parameter change.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TimedParameterChange {
    /// Stable VST3 parameter identifier.
    pub parameter_id: u32,
    /// Normalized value in `0.0..=1.0`.
    pub normalized: f64,
    /// Sample offset within the current block.
    pub sample_offset: u16,
}

/// A bounded, coalescing parameter-change queue.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundedParameterChanges<const CAPACITY: usize> {
    entries: [Option<TimedParameterChange>; CAPACITY],
    len: usize,
}

impl<const CAPACITY: usize> Default for BoundedParameterChanges<CAPACITY> {
    fn default() -> Self {
        Self {
            entries: [None; CAPACITY],
            len: 0,
        }
    }
}

impl<const CAPACITY: usize> BoundedParameterChanges<CAPACITY> {
    /// Returns the number of unique queued parameter ids.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns whether no parameter changes are queued.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns queued changes in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = &TimedParameterChange> {
        self.entries[..self.len].iter().flatten()
    }

    /// Clears queued changes while retaining storage.
    pub fn clear(&mut self) {
        self.entries[..self.len].fill(None);
        self.len = 0;
    }

    /// Adds or replaces a normalized parameter value without allocating.
    ///
    /// The latest change for an existing id replaces its prior value, which preserves the final
    /// control value under pressure rather than consuming capacity with redundant automation.
    ///
    /// # Errors
    ///
    /// Returns [`QueueFull`] for a non-finite/out-of-range value or when no new id can fit.
    pub fn push_coalescing(&mut self, change: TimedParameterChange) -> Result<(), QueueFull> {
        if !(change.normalized.is_finite() && (0.0..=1.0).contains(&change.normalized)) {
            return Err(QueueFull);
        }
        if let Some(entry) = self.entries[..self.len]
            .iter_mut()
            .flatten()
            .find(|entry| entry.parameter_id == change.parameter_id)
        {
            *entry = change;
            return Ok(());
        }
        if self.len == CAPACITY {
            return Err(QueueFull);
        }
        self.entries[self.len] = Some(change);
        self.len += 1;
        Ok(())
    }
}

/// Returned when a fixed event/change queue has no remaining capacity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QueueFull;

impl fmt::Display for QueueFull {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("fixed-capacity queue is full")
    }
}

impl Error for QueueFull {}

/// Bit flags reported by a VST3 controller for a parameter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParameterFlags(pub u32);

impl ParameterFlags {
    /// Parameter is automatable by the host.
    pub const CAN_AUTOMATE: u32 = 1 << 0;
    /// Parameter is read-only.
    pub const READ_ONLY: u32 = 1 << 1;
    /// Parameter is a bypass control.
    pub const BYPASS: u32 = 1 << 16;
    /// Parameter represents a program change.
    pub const PROGRAM_CHANGE: u32 = 1 << 15;
    /// Parameter value is expressed as a list rather than a continuous quantity.
    pub const LIST: u32 = 1 << 3;
    /// Parameter is hidden from ordinary generic-editor presentation.
    pub const HIDDEN: u32 = 1 << 4;

    /// Returns whether a raw VST3 flag is present.
    #[must_use]
    pub const fn contains(self, flag: u32) -> bool {
        self.0 & flag != 0
    }
}

/// Complete metadata for one VST3 controller parameter.
#[derive(Clone, Debug, PartialEq)]
pub struct Vst3ParameterInfo {
    /// Stable VST3 parameter identifier.
    pub id: u32,
    /// Long display title.
    pub title: String,
    /// Short display title, if the controller supplies one.
    pub short_title: String,
    /// Unit string such as `Hz`, `dB`, or `%`.
    pub unit: String,
    /// Current normalized controller value.
    pub normalized: f64,
    /// Default normalized controller value.
    pub default_normalized: f64,
    /// Number of discrete intervals; zero represents continuous control.
    pub step_count: i32,
    /// Complete raw/normalized VST3 parameter flags.
    pub flags: ParameterFlags,
}

/// An ordered controller gesture reported by a plug-in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParameterGesture {
    /// Parameter receiving the gesture.
    pub parameter_id: u32,
    /// Gesture lifecycle action.
    pub action: ParameterGestureAction,
    /// Normalized value for a value-change gesture.
    pub normalized: Option<f64>,
}

/// One phase of a plug-in parameter edit gesture.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParameterGestureAction {
    /// The controller began an edit.
    Begin,
    /// The controller reported an in-progress value change.
    Value,
    /// The controller completed an edit.
    End,
}

/// A host notification that affects graph preparation or diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdapterNotification {
    /// The plug-in's reported processing latency changed.
    LatencyChanged {
        /// New latency in samples.
        samples: u32,
    },
    /// The plug-in requested a host restart/reconfiguration.
    RestartRequested {
        /// Raw VST3 restart flags, preserved for the helper control plane.
        flags: u32,
    },
}

/// A bounded output change emitted by a plug-in during one block.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OutputChange {
    /// MIDI emitted by the plug-in.
    Midi(TimedMidiMessage),
    /// A parameter value reported by the plug-in/controller.
    Parameter(TimedParameterChange),
    /// An ordered editor/controller gesture.
    Gesture(ParameterGesture),
    /// A non-audio notification for the worker control plane.
    Notification(AdapterNotification),
}

/// A sink used by adapter implementations to return bounded output changes.
pub trait OutputChangeSink {
    /// Appends a change, returning [`QueueFull`] instead of allocating when saturated.
    ///
    /// # Errors
    ///
    /// Returns [`QueueFull`] when its fixed output-change capacity is exhausted.
    fn push(&mut self, change: OutputChange) -> Result<(), QueueFull>;
}

/// Fixed storage for combined plug-in output changes from one rack block.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundedOutputChanges<const CAPACITY: usize> {
    entries: [Option<OutputChange>; CAPACITY],
    len: usize,
}

impl<const CAPACITY: usize> Default for BoundedOutputChanges<CAPACITY> {
    fn default() -> Self {
        Self {
            entries: [None; CAPACITY],
            len: 0,
        }
    }
}

impl<const CAPACITY: usize> BoundedOutputChanges<CAPACITY> {
    /// Returns queued output changes in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = &OutputChange> {
        self.entries[..self.len].iter().flatten()
    }

    /// Returns the number of retained output changes.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns whether no output change is retained.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Clears output changes while retaining the fixed storage.
    pub fn clear(&mut self) {
        self.entries[..self.len].fill(None);
        self.len = 0;
    }
}

impl<const CAPACITY: usize> OutputChangeSink for BoundedOutputChanges<CAPACITY> {
    fn push(&mut self, change: OutputChange) -> Result<(), QueueFull> {
        if self.len == CAPACITY {
            return Err(QueueFull);
        }
        self.entries[self.len] = Some(change);
        self.len += 1;
        Ok(())
    }
}

/// Read-only planar input passed to one plug-in process call.
pub struct PlanarAudioInput<'a> {
    channels: [&'a [f32]; 2],
    channel_count: u8,
}

impl<'a> PlanarAudioInput<'a> {
    /// Creates an input view with zero, one, or two channels.
    #[must_use]
    pub fn new(channels: [&'a [f32]; 2], channel_count: u8) -> Self {
        Self {
            channels,
            channel_count,
        }
    }

    /// Returns the active channel count.
    #[must_use]
    pub const fn channel_count(&self) -> u8 {
        self.channel_count
    }

    /// Returns an active plane by index.
    #[must_use]
    pub fn channel(&self, index: usize) -> Option<&'a [f32]> {
        (index < usize::from(self.channel_count)).then(|| self.channels[index])
    }

    /// Splits this view into its storage planes and active channel count.
    #[must_use]
    pub fn into_parts(self) -> ([&'a [f32]; 2], u8) {
        (self.channels, self.channel_count)
    }
}

/// Mutable planar output passed to one plug-in process call.
pub struct PlanarAudioOutput<'a> {
    channels: [&'a mut [f32]; 2],
    channel_count: u8,
}

impl<'a> PlanarAudioOutput<'a> {
    /// Creates an output view with one or two active channels.
    #[must_use]
    pub fn new(channels: [&'a mut [f32]; 2], channel_count: u8) -> Self {
        Self {
            channels,
            channel_count,
        }
    }

    /// Returns the active channel count.
    #[must_use]
    pub const fn channel_count(&self) -> u8 {
        self.channel_count
    }

    /// Returns an active mutable plane by index.
    pub fn channel_mut(&mut self, index: usize) -> Option<&mut [f32]> {
        (index < usize::from(self.channel_count)).then(|| &mut *self.channels[index])
    }

    /// Splits this view into its storage planes and active channel count.
    #[must_use]
    pub fn into_parts(self) -> ([&'a mut [f32]; 2], u8) {
        (self.channels, self.channel_count)
    }
}

/// One allocation-free process invocation supplied to a selected VST3 component.
pub struct PluginProcessBlock<'a> {
    /// Active frame count, always less than or equal to the prepared maximum.
    pub frames: usize,
    /// Planar input audio for the component.
    pub input: PlanarAudioInput<'a>,
    /// Planar output audio for the component.
    pub output: PlanarAudioOutput<'a>,
    /// Fixed-capacity MIDI events for this block.
    pub midi: &'a BoundedMidiEvents<MAX_RACK_MIDI_EVENTS>,
    /// Fixed-capacity parameter changes for this block.
    pub parameter_changes: &'a BoundedParameterChanges<MAX_RACK_PARAMETER_CHANGES>,
    /// Fixed-capacity output-change sink for this block.
    pub output_changes: &'a mut dyn OutputChangeSink,
}

/// Separate opaque VST3 state streams.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Vst3StateStreams {
    /// State written by `IComponent::getState`.
    pub component: Vec<u8>,
    /// State written by `IEditController::getState`.
    pub controller: Vec<u8>,
}

/// The SDK-free contract implemented by the worker's VST3 backend.
///
/// The lifecycle methods intentionally mirror the VST3 ordering constraints. The fixed rack
/// calls them in this sequence: select module/class, initialize component, initialize controller,
/// connect component/controller, discover and negotiate buses, activate buses, then start
/// processing. Shutdown performs stop before any deactivation or state restoration.
pub trait RackPluginAdapter {
    /// Backend-specific error type.
    type Error: Error + Send + Sync + 'static;

    /// Selects and instantiates one concrete module class.
    ///
    /// # Errors
    ///
    /// Returns the backend error when selection, module loading, or format setup fails.
    fn select_module_class(
        &mut self,
        selection: &Vst3ClassSelection,
        format: ProcessingFormat,
    ) -> Result<(), Self::Error>;

    /// Initializes the selected `IComponent` against the host context.
    ///
    /// # Errors
    ///
    /// Returns the backend error when component initialization fails.
    fn initialize_component(&mut self) -> Result<(), Self::Error>;

    /// Creates and initializes the `IEditController` where the component uses a separate one.
    ///
    /// # Errors
    ///
    /// Returns the backend error when controller initialization fails.
    fn initialize_controller(&mut self) -> Result<(), Self::Error>;

    /// Connects both `IConnectionPoint` directions when supported.
    ///
    /// # Errors
    ///
    /// Returns the backend error when a required connection cannot be made.
    fn connect_component_controller(&mut self) -> Result<(), Self::Error>;

    /// Discovers currently exposed buses without changing the active layout.
    ///
    /// # Errors
    ///
    /// Returns the backend error when bus metadata cannot be read.
    fn discover_buses(&mut self) -> Result<Vst3BusTopology, Self::Error>;

    /// Requests the fixed main-bus layout while the component is inactive.
    ///
    /// # Errors
    ///
    /// Returns the backend error when inactive bus negotiation fails.
    fn negotiate_main_buses(&mut self, layout: MainBusLayout) -> Result<(), Self::Error>;

    /// Activates only the accepted main audio/event buses while inactive.
    ///
    /// # Errors
    ///
    /// Returns the backend error when a selected main bus cannot be activated.
    fn activate_main_buses(&mut self, layout: MainBusLayout) -> Result<(), Self::Error>;

    /// Starts processing only after preparation and bus activation complete.
    ///
    /// # Errors
    ///
    /// Returns the backend error when the component rejects the processing transition.
    fn start_processing(&mut self) -> Result<(), Self::Error>;

    /// Stops processing before bus changes, state restoration, or teardown.
    ///
    /// # Errors
    ///
    /// Returns the backend error when the component rejects the stop transition.
    fn stop_processing(&mut self) -> Result<(), Self::Error>;

    /// Processes one preallocated planar block.
    ///
    /// # Errors
    ///
    /// Returns the backend error when block processing fails.
    fn process(&mut self, block: PluginProcessBlock<'_>) -> Result<(), Self::Error>;

    /// Returns complete parameter metadata without leaking SDK types.
    ///
    /// # Errors
    ///
    /// Returns the backend error when controller metadata cannot be read.
    fn parameters(&mut self) -> Result<Vec<Vst3ParameterInfo>, Self::Error>;

    /// Delegates normalized value formatting to the plug-in controller.
    ///
    /// # Errors
    ///
    /// Returns the backend error when the parameter is unknown or formatting fails.
    fn format_parameter(
        &mut self,
        parameter_id: u32,
        normalized: f64,
    ) -> Result<String, Self::Error>;

    /// Captures the component and controller's opaque state streams separately.
    ///
    /// # Errors
    ///
    /// Returns the backend error when either state stream cannot be captured.
    fn capture_state(&mut self) -> Result<Vst3StateStreams, Self::Error>;

    /// Restores component state while inactive.
    ///
    /// # Errors
    ///
    /// Returns the backend error when inactive component restore fails.
    fn restore_component_state(&mut self, component: &[u8]) -> Result<(), Self::Error>;

    /// Synchronizes the controller from newly restored component state while inactive.
    ///
    /// # Errors
    ///
    /// Returns the backend error when controller synchronization fails.
    fn synchronize_controller_from_component_state(&mut self) -> Result<(), Self::Error>;

    /// Restores controller-specific state while inactive and after synchronization.
    ///
    /// # Errors
    ///
    /// Returns the backend error when inactive controller restore fails.
    fn restore_controller_state(&mut self, controller: &[u8]) -> Result<(), Self::Error>;

    /// Returns the current processing latency in samples.
    ///
    /// # Errors
    ///
    /// Returns the backend error when latency cannot be queried.
    fn latency_samples(&mut self) -> Result<u32, Self::Error>;

    /// Drains pending latency/restart notifications into a bounded sink.
    ///
    /// # Errors
    ///
    /// Returns the backend error when notification polling fails.
    fn drain_notifications(&mut self, output: &mut dyn OutputChangeSink)
    -> Result<(), Self::Error>;
}

/// Fixed-capacity serial VST3 rack using preallocated planar ping-pong buffers.
pub struct FixedSerialRack<
    A,
    const SLOTS: usize = MAX_SERIAL_RACK_SLOTS,
    const MAX_FRAMES: usize = MAX_RACK_FRAMES,
> where
    A: RackPluginAdapter,
{
    layout: MainBusLayout,
    format: ProcessingFormat,
    slots: [Option<RackSlot<A>>; SLOTS],
    running: bool,
    ping: [[f32; MAX_FRAMES]; 2],
    pong: [[f32; MAX_FRAMES]; 2],
}

struct RackSlot<A> {
    adapter: A,
    selection: Vst3ClassSelection,
    prepared: bool,
}

impl<A, const SLOTS: usize, const MAX_FRAMES: usize> FixedSerialRack<A, SLOTS, MAX_FRAMES>
where
    A: RackPluginAdapter,
{
    /// Creates an empty fixed-capacity rack.
    ///
    /// # Errors
    ///
    /// Returns a layout error for malformed processing or main-bus configuration.
    pub fn new(layout: MainBusLayout, format: ProcessingFormat) -> Result<Self, LayoutError> {
        layout.validate_for_capacity(MAX_FRAMES, format.maximum_frames)?;
        Ok(Self {
            layout,
            format,
            slots: std::array::from_fn(|_| None),
            running: false,
            ping: [[0.0; MAX_FRAMES]; 2],
            pong: [[0.0; MAX_FRAMES]; 2],
        })
    }

    /// Returns the fixed rack main-bus layout.
    #[must_use]
    pub const fn layout(&self) -> MainBusLayout {
        self.layout
    }

    /// Returns the prepared processing format.
    #[must_use]
    pub const fn format(&self) -> ProcessingFormat {
        self.format
    }

    /// Adds one adapter to the first vacant serial slot.
    ///
    /// The adapter remains inactive until [`Self::prepare`] succeeds.
    ///
    /// # Errors
    ///
    /// Returns [`RackError::RunningMutation`] for an active rack or [`RackError::Capacity`] when
    /// no fixed slot remains.
    pub fn insert(
        &mut self,
        adapter: A,
        selection: Vst3ClassSelection,
    ) -> Result<usize, RackError<A::Error>> {
        if self.running {
            return Err(RackError::RunningMutation);
        }
        let Some(index) = self.slots.iter().position(Option::is_none) else {
            return Err(RackError::Capacity);
        };
        self.slots[index] = Some(RackSlot {
            adapter,
            selection,
            prepared: false,
        });
        Ok(index)
    }

    /// Prepares every inserted slot in official VST3 lifecycle order while inactive.
    ///
    /// # Errors
    ///
    /// Returns an adapter error, topology error, or [`RackError::RunningMutation`] if the rack is
    /// active.
    pub fn prepare(&mut self) -> Result<(), RackError<A::Error>> {
        if self.running {
            return Err(RackError::RunningMutation);
        }
        for (index, slot) in self.slots.iter_mut().enumerate() {
            let Some(slot) = slot.as_mut() else {
                continue;
            };
            if slot.prepared {
                continue;
            }
            slot.adapter
                .select_module_class(&slot.selection, self.format)
                .map_err(|source| RackError::Adapter { index, source })?;
            slot.adapter
                .initialize_component()
                .map_err(|source| RackError::Adapter { index, source })?;
            slot.adapter
                .initialize_controller()
                .map_err(|source| RackError::Adapter { index, source })?;
            slot.adapter
                .connect_component_controller()
                .map_err(|source| RackError::Adapter { index, source })?;
            let discovered = slot
                .adapter
                .discover_buses()
                .map_err(|source| RackError::Adapter { index, source })?;
            validate_discovery_for_request(&discovered, self.layout)
                .map_err(|source| RackError::Layout { index, source })?;
            slot.adapter
                .negotiate_main_buses(self.layout)
                .map_err(|source| RackError::Adapter { index, source })?;
            let negotiated = slot
                .adapter
                .discover_buses()
                .map_err(|source| RackError::Adapter { index, source })?;
            negotiated
                .validate_fixed(self.layout)
                .map_err(|source| RackError::Layout { index, source })?;
            slot.adapter
                .activate_main_buses(self.layout)
                .map_err(|source| RackError::Adapter { index, source })?;
            slot.prepared = true;
        }
        Ok(())
    }

    /// Starts every prepared slot from upstream to downstream.
    ///
    /// # Errors
    ///
    /// Returns [`RackError::NotPrepared`] for an incomplete slot or an adapter start error.
    pub fn start(&mut self) -> Result<(), RackError<A::Error>> {
        if self.running {
            return Ok(());
        }
        for (index, slot) in self.slots.iter_mut().enumerate() {
            let Some(slot) = slot.as_mut() else {
                continue;
            };
            if !slot.prepared {
                return Err(RackError::NotPrepared { index });
            }
            slot.adapter
                .start_processing()
                .map_err(|source| RackError::Adapter { index, source })?;
        }
        self.running = true;
        Ok(())
    }

    /// Stops every active slot from downstream to upstream.
    ///
    /// # Errors
    ///
    /// Returns an adapter error when a slot rejects the stop transition.
    pub fn stop(&mut self) -> Result<(), RackError<A::Error>> {
        if !self.running {
            return Ok(());
        }
        for index in (0..SLOTS).rev() {
            let Some(slot) = self.slots[index].as_mut() else {
                continue;
            };
            slot.adapter
                .stop_processing()
                .map_err(|source| RackError::Adapter { index, source })?;
        }
        self.running = false;
        Ok(())
    }

    /// Processes an allocation-free planar serial block through all active slots.
    ///
    /// The input and output slice counts must match the fixed layout. Input planes can be absent
    /// only for a zero-input instrument rack; output planes must always be present. MIDI and
    /// parameter input are copied neither here nor by this type: their fixed queues are borrowed
    /// by every slot. A caller owns the output change queue and can track saturation separately.
    ///
    /// # Errors
    ///
    /// Returns an error for an inactive rack, oversized/short planes, a layout mismatch, or a
    /// plug-in adapter processing failure.
    pub fn process(
        &mut self,
        input: &[&[f32]],
        output: &mut [&mut [f32]],
        frames: usize,
        midi: &BoundedMidiEvents<MAX_RACK_MIDI_EVENTS>,
        parameter_changes: &BoundedParameterChanges<MAX_RACK_PARAMETER_CHANGES>,
        output_changes: &mut dyn OutputChangeSink,
    ) -> Result<(), RackError<A::Error>> {
        if !self.running {
            return Err(RackError::NotRunning);
        }
        if frames == 0 || frames > MAX_FRAMES || frames > self.format.maximum_frames {
            return Err(RackError::InvalidFrameCount { frames });
        }
        if input.len() != usize::from(self.layout.input_channels)
            || output.len() != usize::from(self.layout.output_channels)
        {
            return Err(RackError::ProcessLayoutMismatch {
                input_channels: input.len(),
                output_channels: output.len(),
            });
        }
        if input.iter().any(|plane| plane.len() < frames)
            || output.iter().any(|plane| plane.len() < frames)
        {
            return Err(RackError::ShortAudioPlane { frames });
        }

        self.ping[0][..frames].fill(0.0);
        self.ping[1][..frames].fill(0.0);
        for (channel, source) in input.iter().enumerate() {
            self.ping[channel][..frames].copy_from_slice(&source[..frames]);
        }

        let mut previous_channels = self.layout.input_channels;
        let mut processed_slots = 0usize;
        for index in 0..SLOTS {
            let Some(slot) = self.slots[index].as_mut() else {
                continue;
            };
            let input_channels = if processed_slots == 0 {
                previous_channels
            } else {
                self.layout.output_channels
            };
            let (source, destination) = if processed_slots.is_multiple_of(2) {
                (&self.ping, &mut self.pong)
            } else {
                (&self.pong, &mut self.ping)
            };
            destination[0][..frames].fill(0.0);
            destination[1][..frames].fill(0.0);
            let (source_left, source_right) = source.split_at(1);
            let (destination_left, destination_right) = destination.split_at_mut(1);
            let block = PluginProcessBlock {
                frames,
                input: PlanarAudioInput::new(
                    [&source_left[0][..frames], &source_right[0][..frames]],
                    input_channels,
                ),
                output: PlanarAudioOutput::new(
                    [
                        &mut destination_left[0][..frames],
                        &mut destination_right[0][..frames],
                    ],
                    self.layout.output_channels,
                ),
                midi,
                parameter_changes,
                output_changes,
            };
            slot.adapter
                .process(block)
                .map_err(|source| RackError::Adapter { index, source })?;
            slot.adapter
                .drain_notifications(output_changes)
                .map_err(|source| RackError::Adapter { index, source })?;
            previous_channels = self.layout.output_channels;
            processed_slots += 1;
        }

        let final_audio = if processed_slots.is_multiple_of(2) {
            &self.ping
        } else {
            &self.pong
        };
        for (channel, destination) in output.iter_mut().enumerate() {
            destination[..frames].copy_from_slice(&final_audio[channel][..frames]);
        }
        Ok(())
    }

    /// Captures independent opaque streams from all slots while they are inactive.
    ///
    /// # Errors
    ///
    /// Returns [`RackError::ActiveStateOperation`] for a running rack or an adapter capture
    /// error for any inserted slot.
    pub fn capture_state(
        &mut self,
    ) -> Result<[Option<Vst3StateStreams>; SLOTS], RackError<A::Error>> {
        if self.running {
            return Err(RackError::ActiveStateOperation);
        }
        let mut streams = std::array::from_fn(|_| None);
        for (index, slot) in self.slots.iter_mut().enumerate() {
            let Some(slot) = slot.as_mut() else {
                continue;
            };
            streams[index] = Some(
                slot.adapter
                    .capture_state()
                    .map_err(|source| RackError::Adapter { index, source })?,
            );
        }
        Ok(streams)
    }

    /// Restores opaque state in official inactive component/controller order.
    ///
    /// The rack must be stopped. For every supplied slot stream, this calls component restore,
    /// controller synchronization from component state, and controller-specific restore in that
    /// exact order. It never restarts processing implicitly; the caller controls the muted
    /// activation boundary with [`Self::start`].
    ///
    /// # Errors
    ///
    /// Returns [`RackError::ActiveStateOperation`] for a running rack or an adapter state-restore
    /// error for any provided stream.
    pub fn restore_state(
        &mut self,
        streams: &[Option<Vst3StateStreams>; SLOTS],
    ) -> Result<(), RackError<A::Error>> {
        if self.running {
            return Err(RackError::ActiveStateOperation);
        }
        for (index, state) in streams.iter().enumerate() {
            let (Some(slot), Some(state)) = (self.slots[index].as_mut(), state.as_ref()) else {
                continue;
            };
            slot.adapter
                .restore_component_state(&state.component)
                .map_err(|source| RackError::Adapter { index, source })?;
            slot.adapter
                .synchronize_controller_from_component_state()
                .map_err(|source| RackError::Adapter { index, source })?;
            slot.adapter
                .restore_controller_state(&state.controller)
                .map_err(|source| RackError::Adapter { index, source })?;
        }
        Ok(())
    }

    /// Returns parameter metadata for one prepared slot.
    ///
    /// # Errors
    ///
    /// Returns [`RackError::MissingSlot`] or the selected adapter's metadata error.
    pub fn parameters(
        &mut self,
        index: usize,
    ) -> Result<Vec<Vst3ParameterInfo>, RackError<A::Error>> {
        let slot = self.slot_mut(index)?;
        slot.adapter
            .parameters()
            .map_err(|source| RackError::Adapter { index, source })
    }

    /// Formats a parameter through one slot's controller.
    ///
    /// # Errors
    ///
    /// Returns [`RackError::MissingSlot`] or the selected adapter's formatting error.
    pub fn format_parameter(
        &mut self,
        index: usize,
        parameter_id: u32,
        normalized: f64,
    ) -> Result<String, RackError<A::Error>> {
        let slot = self.slot_mut(index)?;
        slot.adapter
            .format_parameter(parameter_id, normalized)
            .map_err(|source| RackError::Adapter { index, source })
    }

    /// Returns reported latency for one prepared slot.
    ///
    /// # Errors
    ///
    /// Returns [`RackError::MissingSlot`] or the selected adapter's latency-query error.
    pub fn latency_samples(&mut self, index: usize) -> Result<u32, RackError<A::Error>> {
        let slot = self.slot_mut(index)?;
        slot.adapter
            .latency_samples()
            .map_err(|source| RackError::Adapter { index, source })
    }

    fn slot_mut(&mut self, index: usize) -> Result<&mut RackSlot<A>, RackError<A::Error>> {
        self.slots
            .get_mut(index)
            .and_then(Option::as_mut)
            .ok_or(RackError::MissingSlot { index })
    }
}

impl MainBusLayout {
    fn validate_for_capacity(
        self,
        capacity: usize,
        configured_frames: usize,
    ) -> Result<(), LayoutError> {
        if capacity == 0 || configured_frames > capacity {
            return Err(LayoutError::InvalidProcessingFormat {
                sample_rate_hz: 0.0,
                maximum_frames: configured_frames,
            });
        }
        Self::new(
            self.input_channels,
            self.output_channels,
            self.event_input_active,
        )
        .map(|_| ())
    }
}

fn validate_discovery_for_request(
    discovered: &Vst3BusTopology,
    expected: MainBusLayout,
) -> Result<(), LayoutError> {
    for bus in discovered
        .audio_inputs
        .iter()
        .chain(&discovered.audio_outputs)
        .chain(&discovered.event_inputs)
        .chain(&discovered.event_outputs)
    {
        if bus.role == BusRole::Auxiliary {
            return Err(LayoutError::SidechainOrAuxiliaryBus {
                direction: bus.direction,
            });
        }
    }
    if discovered.audio_inputs.len() > 1 {
        return Err(LayoutError::UnexpectedBusCount {
            media: BusMedia::Audio,
            direction: BusDirection::Input,
            expected: 1,
            actual: discovered.audio_inputs.len(),
        });
    }
    if discovered.audio_outputs.len() != 1 {
        return Err(LayoutError::UnexpectedBusCount {
            media: BusMedia::Audio,
            direction: BusDirection::Output,
            expected: 1,
            actual: discovered.audio_outputs.len(),
        });
    }
    if discovered.event_inputs.len() > 1 {
        return Err(LayoutError::UnexpectedBusCount {
            media: BusMedia::Event,
            direction: BusDirection::Input,
            expected: 1,
            actual: discovered.event_inputs.len(),
        });
    }
    if expected.event_input_active && discovered.event_inputs.is_empty() {
        return Err(LayoutError::MissingEventInput);
    }
    if discovered.event_outputs.len() > 1 {
        return Err(LayoutError::UnexpectedBusCount {
            media: BusMedia::Event,
            direction: BusDirection::Output,
            expected: 1,
            actual: discovered.event_outputs.len(),
        });
    }
    Ok(())
}

/// Error produced by fixed serial rack orchestration.
#[derive(Debug)]
pub enum RackError<E> {
    /// The fixed slot capacity has been reached.
    Capacity,
    /// Caller attempted to edit slots while audio processing is active.
    RunningMutation,
    /// A requested slot does not exist.
    MissingSlot {
        /// Missing slot index.
        index: usize,
    },
    /// A slot was started before its inactive preparation completed.
    NotPrepared {
        /// Slot index.
        index: usize,
    },
    /// Processing was requested before the rack started.
    NotRunning,
    /// An active rack attempted a state capture/restore operation.
    ActiveStateOperation,
    /// Frame count exceeds the prepared fixed buffers.
    InvalidFrameCount {
        /// Requested frame count.
        frames: usize,
    },
    /// Process audio planes do not match the fixed main layout.
    ProcessLayoutMismatch {
        /// Provided input plane count.
        input_channels: usize,
        /// Provided output plane count.
        output_channels: usize,
    },
    /// An input or output plane is shorter than the requested frame count.
    ShortAudioPlane {
        /// Requested frame count.
        frames: usize,
    },
    /// Bus discovery or negotiation violates the fixed alpha topology.
    Layout {
        /// Slot index.
        index: usize,
        /// Concrete topology violation.
        source: LayoutError,
    },
    /// Backend adapter failed.
    Adapter {
        /// Slot index.
        index: usize,
        /// Backend error.
        source: E,
    },
}

impl<E: fmt::Display> fmt::Display for RackError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Capacity => formatter.write_str("serial rack has no vacant slot"),
            Self::RunningMutation => formatter.write_str("cannot mutate a running serial rack"),
            Self::MissingSlot { index } => write!(formatter, "serial rack slot {index} is empty"),
            Self::NotPrepared { index } => {
                write!(formatter, "serial rack slot {index} is not prepared")
            }
            Self::NotRunning => formatter.write_str("serial rack is not processing"),
            Self::ActiveStateOperation => {
                formatter.write_str("opaque state operations require an inactive rack")
            }
            Self::InvalidFrameCount { frames } => write!(
                formatter,
                "frame count {frames} exceeds fixed rack capacity"
            ),
            Self::ProcessLayoutMismatch {
                input_channels,
                output_channels,
            } => write!(
                formatter,
                "process planes ({input_channels} input, {output_channels} output) do not match rack layout"
            ),
            Self::ShortAudioPlane { frames } => {
                write!(formatter, "audio plane is shorter than {frames} frames")
            }
            Self::Layout { index, source } => {
                write!(formatter, "slot {index} layout error: {source}")
            }
            Self::Adapter { index, source } => {
                write!(formatter, "slot {index} adapter error: {source}")
            }
        }
    }
}

impl<E: Error + 'static> Error for RackError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Layout { source, .. } => Some(source),
            Self::Adapter { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum FakeError {
        InvalidLifecycle,
    }

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("fake lifecycle failure")
        }
    }

    impl Error for FakeError {}

    struct FakeAdapter {
        log: Vec<&'static str>,
        topology: Vst3BusTopology,
        initialized: bool,
        processing: bool,
    }

    impl FakeAdapter {
        fn stereo() -> Self {
            Self {
                log: Vec::new(),
                topology: Vst3BusTopology {
                    audio_inputs: vec![Vst3BusDescriptor::audio(
                        BusDirection::Input,
                        BusRole::Main,
                        2,
                    )],
                    audio_outputs: vec![Vst3BusDescriptor::audio(
                        BusDirection::Output,
                        BusRole::Main,
                        2,
                    )],
                    event_inputs: vec![Vst3BusDescriptor::event(
                        BusDirection::Input,
                        BusRole::Main,
                    )],
                    event_outputs: Vec::new(),
                },
                initialized: false,
                processing: false,
            }
        }
    }

    impl RackPluginAdapter for FakeAdapter {
        type Error = FakeError;

        fn select_module_class(
            &mut self,
            _selection: &Vst3ClassSelection,
            _format: ProcessingFormat,
        ) -> Result<(), Self::Error> {
            self.log.push("select");
            Ok(())
        }

        fn initialize_component(&mut self) -> Result<(), Self::Error> {
            self.log.push("component");
            self.initialized = true;
            Ok(())
        }

        fn initialize_controller(&mut self) -> Result<(), Self::Error> {
            self.log.push("controller");
            Ok(())
        }

        fn connect_component_controller(&mut self) -> Result<(), Self::Error> {
            self.log.push("connect");
            Ok(())
        }

        fn discover_buses(&mut self) -> Result<Vst3BusTopology, Self::Error> {
            self.log.push("discover");
            Ok(self.topology.clone())
        }

        fn negotiate_main_buses(&mut self, _layout: MainBusLayout) -> Result<(), Self::Error> {
            self.log.push("negotiate");
            Ok(())
        }

        fn activate_main_buses(&mut self, _layout: MainBusLayout) -> Result<(), Self::Error> {
            self.log.push("activate");
            Ok(())
        }

        fn start_processing(&mut self) -> Result<(), Self::Error> {
            if !self.initialized {
                return Err(FakeError::InvalidLifecycle);
            }
            self.log.push("start");
            self.processing = true;
            Ok(())
        }

        fn stop_processing(&mut self) -> Result<(), Self::Error> {
            self.log.push("stop");
            self.processing = false;
            Ok(())
        }

        fn process(&mut self, mut block: PluginProcessBlock<'_>) -> Result<(), Self::Error> {
            if !self.processing {
                return Err(FakeError::InvalidLifecycle);
            }
            self.log.push("process");
            for index in 0..usize::from(block.output.channel_count()) {
                let input = block.input.channel(index).unwrap_or(&[]);
                let output = block.output.channel_mut(index).expect("active output");
                output[..block.frames].fill(0.0);
                let copied = input.len().min(block.frames);
                output[..copied].copy_from_slice(&input[..copied]);
            }
            Ok(())
        }

        fn parameters(&mut self) -> Result<Vec<Vst3ParameterInfo>, Self::Error> {
            Ok(Vec::new())
        }

        fn format_parameter(
            &mut self,
            _parameter_id: u32,
            _normalized: f64,
        ) -> Result<String, Self::Error> {
            Ok(String::new())
        }

        fn capture_state(&mut self) -> Result<Vst3StateStreams, Self::Error> {
            Ok(Vst3StateStreams::default())
        }

        fn restore_component_state(&mut self, _component: &[u8]) -> Result<(), Self::Error> {
            self.log.push("restore-component");
            Ok(())
        }

        fn synchronize_controller_from_component_state(&mut self) -> Result<(), Self::Error> {
            self.log.push("sync-controller");
            Ok(())
        }

        fn restore_controller_state(&mut self, _controller: &[u8]) -> Result<(), Self::Error> {
            self.log.push("restore-controller");
            Ok(())
        }

        fn latency_samples(&mut self) -> Result<u32, Self::Error> {
            Ok(0)
        }

        fn drain_notifications(
            &mut self,
            _output: &mut dyn OutputChangeSink,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    fn selection() -> Vst3ClassSelection {
        Vst3ClassSelection::new(Vst3BundlePath::new("fake.vst3"), "fake-class")
    }

    #[test]
    fn fixed_rack_orders_lifecycle_and_inactive_restore() {
        let layout = MainBusLayout::new(2, 2, true).expect("valid stereo layout");
        let format = ProcessingFormat::new(48_000.0, 128).expect("valid format");
        let mut rack = FixedSerialRack::<FakeAdapter, 1, 128>::new(layout, format)
            .expect("fixed rack construction");
        rack.insert(FakeAdapter::stereo(), selection())
            .expect("slot insertion");
        rack.prepare().expect("inactive preparation");
        rack.start().expect("start after preparation");
        rack.stop().expect("stop before state restore");
        let streams = [Some(Vst3StateStreams {
            component: vec![1],
            controller: vec![2],
        })];
        rack.restore_state(&streams)
            .expect("inactive official-order restoration");

        let slot = rack.slots[0].as_ref().expect("inserted slot");
        assert_eq!(
            slot.adapter.log,
            [
                "select",
                "component",
                "controller",
                "connect",
                "discover",
                "negotiate",
                "discover",
                "activate",
                "start",
                "stop",
                "restore-component",
                "sync-controller",
                "restore-controller",
            ]
        );
    }

    #[test]
    fn fixed_rack_rejects_sidechains_before_activation() {
        let layout = MainBusLayout::new(2, 2, true).expect("valid stereo layout");
        let format = ProcessingFormat::new(48_000.0, 128).expect("valid format");
        let mut adapter = FakeAdapter::stereo();
        adapter.topology.audio_inputs.push(Vst3BusDescriptor::audio(
            BusDirection::Input,
            BusRole::Auxiliary,
            1,
        ));
        let mut rack = FixedSerialRack::<FakeAdapter, 1, 128>::new(layout, format)
            .expect("fixed rack construction");
        rack.insert(adapter, selection()).expect("slot insertion");

        let error = rack.prepare().expect_err("sidechain must be rejected");
        assert!(matches!(
            error,
            RackError::Layout {
                source: LayoutError::SidechainOrAuxiliaryBus { .. },
                ..
            }
        ));
        let slot = rack.slots[0].as_ref().expect("inserted slot");
        assert!(!slot.adapter.log.contains(&"activate"));
    }

    #[test]
    fn note_off_replaces_continuous_event_when_queue_is_full() {
        let mut events = BoundedMidiEvents::<1>::default();
        events
            .push_preserving_note_off(TimedMidiMessage {
                message: MidiMessage::ControlChange {
                    channel: 0,
                    controller: 1,
                    value: 64,
                },
                sample_offset: 0,
            })
            .expect("initial event");
        events
            .push_preserving_note_off(TimedMidiMessage {
                message: MidiMessage::NoteOff {
                    channel: 0,
                    note: 60,
                    velocity: 0,
                },
                sample_offset: 4,
            })
            .expect("note off replaces controller");
        assert!(
            events
                .iter()
                .all(|event| matches!(event.message, MidiMessage::NoteOff { .. }))
        );
    }
}
