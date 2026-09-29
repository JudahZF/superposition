//! Bounded serial rack runtime used by the worker processing thread.
//!
//! This module deliberately knows neither VST3 SDK nor control-wire types. The worker binds the
//! local [`PluginFacade`] to the helper-safe `sp-vst3` facade, and the control thread supplies
//! validated commands from `sp-protocol`. Keeping this seam local prevents VST3 types and mutable
//! plug-in instances from crossing either process boundary.

use std::{
    array, fmt, mem,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};

use sp_shared_memory::{
    BlockEvent, BlockRequest, BlockSlot, MAX_CHANNELS, MAX_FRAMES, MAX_PLUGINS_PER_RACK, MidiEvent,
};

/// Sentinel used when no plug-in call is currently executing.
pub const NO_CURRENT_SLOT: u32 = u32::MAX;
/// Consecutive good blocks a plug-in inserted into a running chain must produce before its
/// output is heard. A good block processed without error and produced only finite samples.
pub const WARMUP_BLOCKS: u8 = 3;
/// Fade length when a slot's output starts or stops being heard: after warmup, and on bypass.
pub const SLOT_FADE_FRAMES: usize = 16;
/// Largest plug-in latency a bypassed or warming slot can match exactly. Beyond this, the slot
/// passes silence instead of misaligned audio.
pub const MAX_BYPASS_DELAY_FRAMES: usize = 65_536;
// Each slot writes a whole block before reading delayed frames, so the ring holds one extra.
const BYPASS_DELAY_CAPACITY_FRAMES: usize = MAX_BYPASS_DELAY_FRAMES + MAX_FRAMES;
/// Fed to an active sidechain input when the request carries no audio for its slot.
static SILENT_SIDECHAIN: [[f32; MAX_FRAMES]; MAX_CHANNELS] = [[0.0; MAX_FRAMES]; MAX_CHANNELS];

/// Preallocated planar audio used between serial plug-in calls.
#[derive(Clone)]
pub struct PlanarBlock {
    channels: [[f32; MAX_FRAMES]; MAX_CHANNELS],
}

impl PlanarBlock {
    /// Creates a fully zeroed, fixed-capacity planar block.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            channels: [[0.0; MAX_FRAMES]; MAX_CHANNELS],
        }
    }

    /// Returns the fixed planar sample storage.
    #[must_use]
    pub const fn channels(&self) -> &[[f32; MAX_FRAMES]; MAX_CHANNELS] {
        &self.channels
    }

    /// Returns mutable fixed planar sample storage.
    pub fn channels_mut(&mut self) -> &mut [[f32; MAX_FRAMES]; MAX_CHANNELS] {
        &mut self.channels
    }

    fn copy_from_slot(&mut self, slot: &BlockSlot, frame_count: usize) {
        for channel in 0..MAX_CHANNELS {
            self.channels[channel][..frame_count]
                .copy_from_slice(&slot.input_audio[channel][..frame_count]);
        }
    }

    fn clear(&mut self, frame_count: usize) {
        for channel in &mut self.channels {
            channel[..frame_count].fill(0.0);
        }
    }

    fn copy_to_slot(&self, slot: &mut BlockSlot, output_channels: usize, frame_count: usize) {
        for channel in 0..output_channels {
            slot.output_audio[channel][..frame_count]
                .copy_from_slice(&self.channels[channel][..frame_count]);
        }
    }

    fn is_finite(&self, channels: usize, frame_count: usize) -> bool {
        self.channels[..channels].iter().all(|channel| {
            channel[..frame_count]
                .iter()
                .all(|sample| sample.is_finite())
        })
    }
}

/// Planar history of the audio entering one slot position. A bypassed or warming plug-in's
/// output is replaced by this input, delayed by the plug-in's latency, so the chain keeps the
/// same timing whether the plug-in is heard or not.
struct BypassDelay {
    channels: [Box<[f32]>; MAX_CHANNELS],
    write: usize,
}

impl BypassDelay {
    fn new() -> Self {
        Self {
            channels: array::from_fn(|_| {
                vec![0.0; BYPASS_DELAY_CAPACITY_FRAMES].into_boxed_slice()
            }),
            write: 0,
        }
    }

    fn write(&mut self, input: &PlanarBlock, frame_count: usize) {
        for (history, input) in self.channels.iter_mut().zip(input.channels()) {
            for (frame, sample) in input[..frame_count].iter().enumerate() {
                history[(self.write + frame) % BYPASS_DELAY_CAPACITY_FRAMES] = *sample;
            }
        }
        self.write = (self.write + frame_count) % BYPASS_DELAY_CAPACITY_FRAMES;
    }

    /// Reads the block just written, `latency` frames late.
    fn read(&self, output: &mut PlanarBlock, latency: usize, frame_count: usize) {
        for (history, output) in self.channels.iter().zip(output.channels_mut()) {
            if latency > MAX_BYPASS_DELAY_FRAMES {
                output[..frame_count].fill(0.0);
                continue;
            }
            let start = self.write + BYPASS_DELAY_CAPACITY_FRAMES - frame_count - latency;
            for (frame, sample) in output[..frame_count].iter_mut().enumerate() {
                *sample = history[(start + frame) % BYPASS_DELAY_CAPACITY_FRAMES];
            }
        }
    }
}

impl Default for PlanarBlock {
    fn default() -> Self {
        Self::new()
    }
}

/// Blends `input` into the plug-in `output` in place, `position` frames into a
/// [`SLOT_FADE_FRAMES`] fade toward the output (`to_output`) or toward the input.
#[allow(
    clippy::cast_precision_loss,
    reason = "fade positions are bounded far below f32 precision limits"
)]
fn crossfade(
    output: &mut PlanarBlock,
    input: &PlanarBlock,
    channels: usize,
    frame_count: usize,
    position: usize,
    to_output: bool,
) {
    for (output, input) in output.channels_mut()[..channels]
        .iter_mut()
        .zip(&input.channels()[..channels])
    {
        for frame in 0..frame_count {
            let amount = ((position + frame) as f32 / SLOT_FADE_FRAMES as f32).min(1.0);
            let weight = if to_output { amount } else { 1.0 - amount };
            output[frame] = input[frame] + (output[frame] - input[frame]) * weight;
        }
    }
}

fn adapt_channels(
    block: &mut PlanarBlock,
    from: usize,
    to: usize,
    frames: usize,
) -> Result<(), PluginRuntimeError> {
    match (from, to) {
        (_, 0) | (1, 1) | (2, 2) => Ok(()),
        (0, 1 | 2) => {
            for channel in &mut block.channels_mut()[..to] {
                channel[..frames].fill(0.0);
            }
            Ok(())
        }
        (1, 2) => {
            let (left, right) = block.channels_mut().split_at_mut(1);
            right[0][..frames].copy_from_slice(&left[0][..frames]);
            Ok(())
        }
        (2, 1) => {
            let channels = block.channels_mut();
            let (left, right) = channels.split_at_mut(1);
            for frame in 0..frames {
                left[0][frame] = (left[0][frame] + right[0][frame]) * 0.5;
            }
            Ok(())
        }
        _ => Err(PluginRuntimeError::IncompatibleChain),
    }
}

/// Alpha main-bus topology for one loaded slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PluginTopology {
    /// Number of channels expected on the plug-in main input bus (zero through two).
    pub input_channels: u32,
    /// Number of channels produced on the plug-in main output bus (one or two).
    pub output_channels: u32,
}

impl PluginTopology {
    /// Returns whether this topology meets the fixed Alpha main-bus limit.
    #[must_use]
    pub const fn is_valid(self) -> bool {
        self.input_channels <= 2 && self.output_channels != 0 && self.output_channels <= 2
    }
}

/// Stable configuration retained for an occupied serial rack slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PluginSlotConfiguration {
    /// Main-bus topology negotiated while the slot was inactive.
    pub topology: PluginTopology,
    /// Whether this plug-in's output is ignored. It still processes its input, and the chain
    /// receives that input delayed by the plug-in's latency.
    pub bypassed: bool,
    /// Whether the plug-in is activated for processing.
    pub active: bool,
    /// Whether the plug-in's stereo sidechain input is active. It reads this slot's aux region
    /// when the request marks it, and silence otherwise.
    pub sidechain: bool,
}

impl PluginSlotConfiguration {
    /// Creates a loaded, active, non-bypassed slot configuration for worker-runtime tests.
    #[cfg(test)]
    #[must_use]
    pub const fn active(topology: PluginTopology) -> Self {
        Self {
            topology,
            bypassed: false,
            active: true,
            sidechain: false,
        }
    }
}

/// Bounded event/audio dimensions supplied to a helper-owned plug-in facade.
#[derive(Clone, Copy)]
pub struct PluginProcessRequest<'a> {
    /// Zero-based serial slot currently being processed.
    pub slot_index: usize,
    /// Valid frame count in the fixed planar buffers.
    pub frame_count: usize,
    /// Negotiated main-input channel count for the current slot.
    pub input_channels: usize,
    /// Negotiated main-output channel count for the current slot.
    pub output_channels: usize,
    /// Planar stereo sidechain, present exactly when the slot's sidechain input is active.
    pub sidechain: Option<&'a [[f32; MAX_FRAMES]; MAX_CHANNELS]>,
    /// Fixed-capacity MIDI events for this block.
    pub midi_events: &'a [MidiEvent],
    /// Fixed-capacity automation events for this block.
    pub automation_events: &'a [BlockEvent],
}

/// Opaque plug-in state carried on the control plane.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PluginState {
    /// Component state or a complete component-and-controller envelope.
    pub component: Vec<u8>,
    /// Separate controller state when the backend provides raw streams.
    pub controller: Vec<u8>,
}

/// Safe, helper-local abstraction supplied by the VST3 adapter.
///
/// All methods execute on the dedicated processing thread or a quiescent control handoff. Native
/// editor operations use a separate main-thread handle. No VST3 SDK type escapes this trait. State
/// restoration is a control-plane operation: callers must first deactivate the slot and never use
/// it for scenes.
pub trait PluginFacade {
    /// Processes a bounded audio block using preallocated planar buffers and event slices.
    fn process(
        &mut self,
        input: &PlanarBlock,
        output: &mut PlanarBlock,
        request: PluginProcessRequest<'_>,
    ) -> Result<(), PluginRuntimeError>;

    /// Activates or deactivates processing while the processing thread is quiescent.
    fn set_active(&mut self, active: bool) -> Result<(), PluginRuntimeError>;

    /// Sets one normalized parameter value.
    fn set_parameter(
        &mut self,
        parameter_id: u32,
        normalized: f64,
    ) -> Result<(), PluginRuntimeError>;

    /// Returns bounded metadata for one controller parameter.
    fn parameter_metadata(
        &mut self,
        _parameter_id: u32,
    ) -> Result<sp_protocol::payload::ParameterMetadata, PluginRuntimeError> {
        Err(PluginRuntimeError::Plugin(
            "parameter metadata is unavailable".to_owned(),
        ))
    }

    /// Reads the current controller value for one parameter.
    fn read_parameter(&mut self, _parameter_id: u32) -> Result<f64, PluginRuntimeError> {
        Err(PluginRuntimeError::Plugin(
            "parameter reads are unavailable".to_owned(),
        ))
    }

    /// Captures opaque component and controller state separately.
    fn capture_state(&mut self) -> Result<PluginState, PluginRuntimeError>;

    /// Restores separate opaque state in the official inactive order.
    fn restore_state(&mut self, state: &PluginState) -> Result<(), PluginRuntimeError>;

    /// Returns the plug-in's current processing latency in samples.
    fn latency_samples(&self) -> u32;

    /// Drains the adapter's bounded restart flags since the previous observation.
    fn take_restart_flags(&mut self) -> u32;
}

/// Failure reported by a helper-local plug-in facade or serial-chain validation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PluginRuntimeError {
    /// The requested slot is outside the fixed eight-slot capacity.
    InvalidSlot,
    /// A requested operation requires an occupied slot.
    EmptySlot,
    /// A plug-in's declared topology is outside the supported Alpha limits.
    InvalidTopology,
    /// Consecutive active plug-in buses are incompatible.
    IncompatibleChain,
    /// The shared-memory request was malformed.
    InvalidRequest,
    /// The helper-safe VST3 facade rejected an operation.
    Plugin(String),
}

impl fmt::Display for PluginRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSlot => {
                formatter.write_str("slot index is outside the fixed rack capacity")
            }
            Self::EmptySlot => formatter.write_str("rack slot is not occupied"),
            Self::InvalidTopology => {
                formatter.write_str("plug-in main-bus topology is unsupported")
            }
            Self::IncompatibleChain => {
                formatter.write_str("serial plug-in bus topologies are incompatible")
            }
            Self::InvalidRequest => formatter.write_str("shared-memory block request is invalid"),
            Self::Plugin(detail) => formatter.write_str(detail),
        }
    }
}

impl std::error::Error for PluginRuntimeError {}

struct PluginSlot<P> {
    plugin: P,
    configuration: PluginSlotConfiguration,
    /// Consecutive good blocks, counted up to [`WARMUP_BLOCKS`]. Below that the plug-in runs
    /// beside the chain and its output is ignored.
    good_blocks: u8,
    /// Whether the chain last switched to this plug-in's output. Each switch fades over
    /// [`SLOT_FADE_FRAMES`]; `fade_position` counts frames since it.
    audible: bool,
    fade_position: usize,
}

impl<P> PluginSlot<P> {
    const fn heard(&self) -> bool {
        !self.configuration.bypassed && self.good_blocks >= WARMUP_BLOCKS
    }
}

/// Fixed-capacity, serial plug-in chain owned exclusively by a worker processing thread.
///
/// This type has three fixed planar buffers and one preallocated bypass delay per slot. It
/// performs no allocation, locking, filesystem access, control IPC, or `AppKit` work while
/// processing a shared-memory block.
pub struct RackProcessor<P> {
    slots: [Option<PluginSlot<P>>; MAX_PLUGINS_PER_RACK],
    delays: Box<[BypassDelay; MAX_PLUGINS_PER_RACK]>,
    ping_a: PlanarBlock,
    ping_b: PlanarBlock,
    passthrough: PlanarBlock,
    rack_bypassed: bool,
    current_slot: Arc<AtomicU32>,
}

impl<P> RackProcessor<P> {
    /// Iterates occupied slots for control-thread service registration.
    pub fn plugins(&self) -> impl Iterator<Item = (usize, &P)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| slot.as_ref().map(|slot| (index, &slot.plugin)))
    }

    /// Creates an empty serial rack with preallocated ping-pong buffers.
    #[must_use]
    pub fn new() -> Self {
        Self {
            slots: array::from_fn(|_| None),
            delays: Box::new(array::from_fn(|_| BypassDelay::new())),
            ping_a: PlanarBlock::new(),
            ping_b: PlanarBlock::new(),
            passthrough: PlanarBlock::new(),
            rack_bypassed: false,
            current_slot: Arc::new(AtomicU32::new(NO_CURRENT_SLOT)),
        }
    }

    /// Replaces one slot with an already loaded helper-owned plug-in, heard from the next block.
    ///
    /// Loading itself occurs on another thread before the instance crosses into this
    /// processing-thread-owned rack.
    ///
    /// # Errors
    ///
    /// Returns an error for an out-of-range slot or unsupported bus topology.
    pub fn replace_slot(
        &mut self,
        slot_index: usize,
        plugin: P,
        configuration: PluginSlotConfiguration,
    ) -> Result<Option<P>, PluginRuntimeError> {
        if slot_index >= MAX_PLUGINS_PER_RACK {
            return Err(PluginRuntimeError::InvalidSlot);
        }
        if !configuration.topology.is_valid() {
            return Err(PluginRuntimeError::InvalidTopology);
        }
        Ok(self.slots[slot_index]
            .replace(PluginSlot {
                plugin,
                configuration,
                good_blocks: WARMUP_BLOCKS,
                audible: !configuration.bypassed,
                fade_position: SLOT_FADE_FRAMES,
            })
            .map(|slot| slot.plugin))
    }

    /// Keeps an occupied slot unheard until it produces [`WARMUP_BLOCKS`] consecutive good
    /// blocks. Until then it processes its input beside the chain.
    ///
    /// # Errors
    ///
    /// Returns an error for an out-of-range or unoccupied slot.
    pub fn start_warmup(&mut self, slot_index: usize) -> Result<(), PluginRuntimeError> {
        let slot = self
            .slots
            .get_mut(slot_index)
            .ok_or(PluginRuntimeError::InvalidSlot)?
            .as_mut()
            .ok_or(PluginRuntimeError::EmptySlot)?;
        slot.good_blocks = 0;
        slot.audible = false;
        slot.fade_position = SLOT_FADE_FRAMES;
        Ok(())
    }

    /// Removes an occupied slot and returns its helper-owned instance.
    ///
    /// # Errors
    ///
    /// Returns an error for an out-of-range slot.
    pub fn remove_slot(&mut self, slot_index: usize) -> Result<Option<P>, PluginRuntimeError> {
        let slot = self
            .slots
            .get_mut(slot_index)
            .ok_or(PluginRuntimeError::InvalidSlot)?;
        Ok(slot.take().map(|slot| slot.plugin))
    }

    /// Moves plug-ins so that slot `i` holds the plug-in previously at `from[i]`. Plug-ins keep
    /// their state and warmup progress; bypass delay history stays with the slot position.
    ///
    /// # Errors
    ///
    /// Returns an error unless `from` is a permutation of the slot positions.
    pub fn reorder(
        &mut self,
        from: &[usize; MAX_PLUGINS_PER_RACK],
    ) -> Result<(), PluginRuntimeError> {
        let mut seen = [false; MAX_PLUGINS_PER_RACK];
        for &source in from {
            if source >= MAX_PLUGINS_PER_RACK || mem::replace(&mut seen[source], true) {
                return Err(PluginRuntimeError::InvalidSlot);
            }
        }
        let mut done = [false; MAX_PLUGINS_PER_RACK];
        for start in 0..MAX_PLUGINS_PER_RACK {
            let mut index = start;
            while !mem::replace(&mut done[index], true) {
                let next = from[index];
                if next == start {
                    break;
                }
                self.slots.swap(index, next);
                index = next;
            }
        }
        Ok(())
    }

    /// Changes bypass while retaining the loaded plug-in and its state.
    ///
    /// # Errors
    ///
    /// Returns an error for an out-of-range or unoccupied slot.
    pub fn set_bypassed(
        &mut self,
        slot_index: usize,
        bypassed: bool,
    ) -> Result<(), PluginRuntimeError> {
        let slot = self
            .slots
            .get_mut(slot_index)
            .ok_or(PluginRuntimeError::InvalidSlot)?
            .as_mut()
            .ok_or(PluginRuntimeError::EmptySlot)?;
        slot.configuration.bypassed = bypassed;
        Ok(())
    }

    /// Changes bypass for the entire serial rack while retaining every loaded slot and state.
    pub fn set_rack_bypassed(&mut self, bypassed: bool) {
        self.rack_bypassed = bypassed;
    }

    /// Returns the best-effort current processing slot, if any.
    #[must_use]
    pub fn current_slot(&self) -> Option<usize> {
        let current = self.current_slot.load(Ordering::Acquire);
        (current != NO_CURRENT_SLOT).then_some(current as usize)
    }

    /// Returns the occupied slot configuration, if present.
    #[must_use]
    pub fn slot_configuration(&self, slot_index: usize) -> Option<PluginSlotConfiguration> {
        self.slots
            .get(slot_index)
            .and_then(Option::as_ref)
            .map(|slot| slot.configuration)
    }
}

impl<P: PluginFacade> RackProcessor<P> {
    /// Activates or deactivates an occupied slot on the processing-thread command boundary.
    ///
    /// # Errors
    ///
    /// Returns an error for an out-of-range or unoccupied slot, or when the facade rejects the
    /// requested lifecycle transition.
    pub fn set_active(
        &mut self,
        slot_index: usize,
        active: bool,
    ) -> Result<(), PluginRuntimeError> {
        let slot = self.slot_mut(slot_index)?;
        slot.plugin.set_active(active)?;
        slot.configuration.active = active;
        Ok(())
    }

    /// Updates one normalized parameter on an occupied plug-in slot.
    ///
    /// # Errors
    ///
    /// Returns an error for an out-of-range or unoccupied slot, a non-finite normalized value, or
    /// a facade-level parameter failure.
    pub fn set_parameter(
        &mut self,
        slot_index: usize,
        parameter_id: u32,
        normalized: f64,
    ) -> Result<(), PluginRuntimeError> {
        if !normalized.is_finite() || !(0.0..=1.0).contains(&normalized) {
            return Err(PluginRuntimeError::Plugin(
                "normalized parameter value must be finite and in 0.0..=1.0".to_owned(),
            ));
        }
        self.slot_mut(slot_index)?
            .plugin
            .set_parameter(parameter_id, normalized)
    }

    /// Returns bounded metadata for one parameter on an occupied slot.
    pub fn parameter_metadata(
        &mut self,
        slot_index: usize,
        parameter_id: u32,
    ) -> Result<sp_protocol::payload::ParameterMetadata, PluginRuntimeError> {
        self.slot_mut(slot_index)?
            .plugin
            .parameter_metadata(parameter_id)
    }

    /// Reads one normalized parameter value from an occupied slot.
    pub fn read_parameter(
        &mut self,
        slot_index: usize,
        parameter_id: u32,
    ) -> Result<f64, PluginRuntimeError> {
        self.slot_mut(slot_index)?
            .plugin
            .read_parameter(parameter_id)
    }

    /// Captures separate component/controller state from an occupied slot.
    ///
    /// # Errors
    ///
    /// Returns an error for an out-of-range or unoccupied slot, or a facade capture failure.
    pub fn capture_state(&mut self, slot_index: usize) -> Result<PluginState, PluginRuntimeError> {
        self.slot_mut(slot_index)?.plugin.capture_state()
    }

    /// Restores separate component/controller state to an inactive occupied slot.
    ///
    /// # Errors
    ///
    /// Returns an error for an out-of-range or unoccupied slot, when the slot remains active, or
    /// a facade restore failure.
    pub fn restore_state(
        &mut self,
        slot_index: usize,
        state: &PluginState,
    ) -> Result<(), PluginRuntimeError> {
        let slot = self.slot_mut(slot_index)?;
        if slot.configuration.active {
            return Err(PluginRuntimeError::Plugin(
                "opaque state restore requires an inactive plug-in slot".to_owned(),
            ));
        }
        slot.plugin.restore_state(state)
    }

    /// Returns the summed latency of active slots. Bypassed and warming slots count, because
    /// their delayed pass-through keeps each plug-in's latency.
    #[must_use]
    pub fn latency_samples(&self) -> u32 {
        if self.rack_bypassed {
            return 0;
        }
        self.slots
            .iter()
            .flatten()
            .filter(|slot| slot.configuration.active)
            .fold(0_u32, |total, slot| {
                total.saturating_add(slot.plugin.latency_samples())
            })
    }

    /// Drain restart notifications with their plug-in slot identity intact.
    pub fn take_slot_restart_flags(&mut self) -> [u32; MAX_PLUGINS_PER_RACK] {
        std::array::from_fn(|index| {
            self.slots[index]
                .as_mut()
                .map_or(0, |slot| slot.plugin.take_restart_flags())
        })
    }

    /// Processes one shared-memory request through the active serial chain.
    ///
    /// The processing thread calls this only after it owns the slot. The local ping-pong buffers
    /// avoid per-plug-in allocation and ensure every stage receives the previous stage's output.
    /// Every active plug-in processes its input. A bypassed or warming plug-in's output is
    /// discarded, and the next stage receives this stage's input delayed by its latency.
    ///
    /// # Errors
    ///
    /// Returns an error when request metadata or active chain topology is invalid, or when a
    /// heard plug-in rejects a bounded processing call. An unheard plug-in's failure only
    /// restarts its warmup.
    pub fn process_block(&mut self, slot: &mut BlockSlot) -> Result<(), PluginRuntimeError> {
        let request = block_request(slot)?;
        let frame_count =
            usize::try_from(request.frame_count).map_err(|_| PluginRuntimeError::InvalidRequest)?;
        let mut carried_channels = usize::try_from(request.input_channel_count)
            .map_err(|_| PluginRuntimeError::InvalidRequest)?;
        let requested_outputs = usize::try_from(request.output_channel_count)
            .map_err(|_| PluginRuntimeError::InvalidRequest)?;
        let midi_count = usize::try_from(request.midi_event_count)
            .map_err(|_| PluginRuntimeError::InvalidRequest)?;
        let automation_count =
            usize::try_from(request.event_count).map_err(|_| PluginRuntimeError::InvalidRequest)?;

        for event in &slot.events[..automation_count] {
            if event.event_type != sp_protocol::BLOCK_EVENT_SLOT_BYPASS || event.flags == 0 {
                continue;
            }
            let target =
                usize::try_from(event.flags - 1).map_err(|_| PluginRuntimeError::InvalidSlot)?;
            if let Some(plugin_slot) = self.slots.get_mut(target).and_then(Option::as_mut) {
                plugin_slot.configuration.bypassed = event.value >= 0.5;
            }
        }

        self.ping_a.copy_from_slot(slot, frame_count);
        if self.rack_bypassed {
            for channel in 0..requested_outputs {
                if carried_channels == 0 {
                    self.ping_a.channels_mut()[channel][..frame_count].fill(0.0);
                } else if channel >= carried_channels {
                    let source = carried_channels - 1;
                    let (before_output, output_and_after) =
                        self.ping_a.channels_mut().split_at_mut(channel);
                    output_and_after[0][..frame_count]
                        .copy_from_slice(&before_output[source][..frame_count]);
                }
            }
            self.ping_a
                .copy_to_slot(slot, requested_outputs, frame_count);
            return Ok(());
        }
        for slot_index in 0..MAX_PLUGINS_PER_RACK {
            let sidechain = (request.sidechain_slots & (1 << slot_index) != 0)
                .then_some(&slot.sidechain_audio[slot_index]);
            carried_channels = self.process_slot(
                slot_index,
                carried_channels,
                frame_count,
                sidechain,
                &slot.midi_events[..midi_count],
                &slot.events[..automation_count],
            )?;
        }
        adapt_channels(
            &mut self.ping_a,
            carried_channels,
            requested_outputs,
            frame_count,
        )?;
        self.ping_a
            .copy_to_slot(slot, requested_outputs, frame_count);
        Ok(())
    }

    /// Runs one slot on `ping_a` and returns the channel count it carries on. An unheard slot
    /// leaves its delayed input in `ping_a`; a slot changing between the two fades. `sidechain`
    /// is this slot's aux region when the request marks it.
    fn process_slot(
        &mut self,
        slot_index: usize,
        carried_channels: usize,
        frame_count: usize,
        sidechain: Option<&[[f32; MAX_FRAMES]; MAX_CHANNELS]>,
        midi_events: &[MidiEvent],
        automation_events: &[BlockEvent],
    ) -> Result<usize, PluginRuntimeError> {
        let Some(plugin_slot) = self.slots[slot_index].as_mut() else {
            return Ok(carried_channels);
        };
        if !plugin_slot.configuration.active {
            return Ok(carried_channels);
        }
        // Record every slot's input so bypass can engage at any block with full history.
        let delay = &mut self.delays[slot_index];
        delay.write(&self.ping_a, frame_count);
        let heard = plugin_slot.heard();
        delay.read(
            &mut self.passthrough,
            plugin_slot.plugin.latency_samples() as usize,
            frame_count,
        );

        let topology = plugin_slot.configuration.topology;
        let input_channels = usize::try_from(topology.input_channels)
            .map_err(|_| PluginRuntimeError::InvalidTopology)?;
        let output_channels = usize::try_from(topology.output_channels)
            .map_err(|_| PluginRuntimeError::InvalidTopology)?;
        adapt_channels(
            &mut self.ping_a,
            carried_channels,
            input_channels,
            frame_count,
        )?;
        self.ping_b.clear(frame_count);
        let current_slot =
            u32::try_from(slot_index).map_err(|_| PluginRuntimeError::InvalidSlot)?;
        self.current_slot.store(current_slot, Ordering::Release);
        let result = plugin_slot.plugin.process(
            &self.ping_a,
            &mut self.ping_b,
            PluginProcessRequest {
                slot_index,
                frame_count,
                input_channels,
                output_channels,
                sidechain: plugin_slot
                    .configuration
                    .sidechain
                    .then(|| sidechain.unwrap_or(&SILENT_SIDECHAIN)),
                midi_events,
                automation_events,
            },
        );
        self.current_slot.store(NO_CURRENT_SLOT, Ordering::Release);
        let usable = if heard {
            result?;
            true
        } else {
            let good = result.is_ok() && self.ping_b.is_finite(output_channels, frame_count);
            plugin_slot.good_blocks = if good {
                plugin_slot.good_blocks.saturating_add(1).min(WARMUP_BLOCKS)
            } else {
                0
            };
            good
        };
        if heard != plugin_slot.audible {
            plugin_slot.audible = heard;
            // A failed block has no output to fade from.
            plugin_slot.fade_position = if usable { 0 } else { SLOT_FADE_FRAMES };
        }
        if plugin_slot.fade_position < SLOT_FADE_FRAMES {
            adapt_channels(
                &mut self.passthrough,
                carried_channels,
                output_channels,
                frame_count,
            )?;
            crossfade(
                &mut self.ping_b,
                &self.passthrough,
                output_channels,
                frame_count,
                plugin_slot.fade_position,
                heard,
            );
            plugin_slot.fade_position =
                (plugin_slot.fade_position + frame_count).min(SLOT_FADE_FRAMES);
            mem::swap(&mut self.ping_a, &mut self.ping_b);
            return Ok(output_channels);
        }
        if heard {
            mem::swap(&mut self.ping_a, &mut self.ping_b);
            Ok(output_channels)
        } else {
            mem::swap(&mut self.ping_a, &mut self.passthrough);
            Ok(carried_channels)
        }
    }

    fn slot_mut(&mut self, slot_index: usize) -> Result<&mut PluginSlot<P>, PluginRuntimeError> {
        self.slots
            .get_mut(slot_index)
            .ok_or(PluginRuntimeError::InvalidSlot)?
            .as_mut()
            .ok_or(PluginRuntimeError::EmptySlot)
    }
}

impl<P> Default for RackProcessor<P> {
    fn default() -> Self {
        Self::new()
    }
}

fn block_request(slot: &BlockSlot) -> Result<BlockRequest, PluginRuntimeError> {
    let request = BlockRequest {
        frame_count: slot.metadata.frame_count,
        input_channel_count: slot.metadata.input_channel_count,
        output_channel_count: slot.metadata.output_channel_count,
        midi_event_count: slot.metadata.midi_event_count,
        event_count: slot.metadata.event_count,
        flags: slot.metadata.flags,
        sidechain_slots: slot.metadata.sidechain_slots,
    };
    request
        .is_valid()
        .then_some(request)
        .ok_or(PluginRuntimeError::InvalidRequest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sp_shared_memory::BlockTicket;

    struct FakePlugin {
        gain: f32,
        latency: u32,
        active: bool,
        restart_flags: u32,
        state: PluginState,
        processed_blocks: u32,
        /// First left sidechain sample of the latest block, when a sidechain was supplied.
        sidechain: Option<f32>,
    }

    impl Default for FakePlugin {
        fn default() -> Self {
            Self {
                gain: 1.0,
                latency: 0,
                active: false,
                restart_flags: 0,
                state: PluginState::default(),
                processed_blocks: 0,
                sidechain: None,
            }
        }
    }

    impl PluginFacade for FakePlugin {
        fn process(
            &mut self,
            input: &PlanarBlock,
            output: &mut PlanarBlock,
            request: PluginProcessRequest<'_>,
        ) -> Result<(), PluginRuntimeError> {
            self.processed_blocks += 1;
            self.sidechain = request.sidechain.map(|planes| planes[0][0]);
            for channel in 0..request.output_channels {
                let source = if request.input_channels == 0 {
                    0
                } else {
                    channel.min(request.input_channels - 1)
                };
                for frame in 0..request.frame_count {
                    output.channels_mut()[channel][frame] =
                        input.channels()[source][frame] * self.gain;
                }
            }
            Ok(())
        }

        fn set_active(&mut self, active: bool) -> Result<(), PluginRuntimeError> {
            self.active = active;
            Ok(())
        }

        #[allow(clippy::cast_possible_truncation)]
        fn set_parameter(
            &mut self,
            _parameter_id: u32,
            normalized: f64,
        ) -> Result<(), PluginRuntimeError> {
            // The production boundary accepts only finite normalized values in 0.0..=1.0.
            self.gain = normalized as f32;
            Ok(())
        }

        fn capture_state(&mut self) -> Result<PluginState, PluginRuntimeError> {
            Ok(self.state.clone())
        }

        fn restore_state(&mut self, state: &PluginState) -> Result<(), PluginRuntimeError> {
            self.state = state.clone();
            Ok(())
        }

        fn latency_samples(&self) -> u32 {
            self.latency
        }

        fn take_restart_flags(&mut self) -> u32 {
            mem::take(&mut self.restart_flags)
        }
    }

    fn requested_slot() -> BlockSlot {
        let mut slot = BlockSlot::new();
        slot.input_audio[0][0] = 0.25;
        slot.input_audio[1][0] = -0.5;
        slot.publish_request(
            BlockTicket {
                generation: 1,
                sequence: 1,
            },
            BlockRequest {
                frame_count: 1,
                input_channel_count: 2,
                output_channel_count: 2,
                midi_event_count: 0,
                event_count: 0,
                flags: 0,
                sidechain_slots: 0,
            },
        )
        .expect("valid fixed request");
        slot
    }

    fn requested_slot_with_event(event: BlockEvent) -> BlockSlot {
        let mut slot = BlockSlot::new();
        slot.input_audio[0][0] = 0.25;
        slot.input_audio[1][0] = -0.5;
        slot.events[0] = event;
        slot.publish_request(
            BlockTicket {
                generation: 1,
                sequence: 1,
            },
            BlockRequest {
                frame_count: 1,
                input_channel_count: 2,
                output_channel_count: 2,
                midi_event_count: 0,
                event_count: 1,
                flags: 0,
                sidechain_slots: 0,
            },
        )
        .expect("valid event request");
        slot
    }

    #[test]
    fn serial_processing_uses_preallocated_ping_pong_buffers() {
        let mut processor = RackProcessor::new();
        let topology = PluginTopology {
            input_channels: 2,
            output_channels: 2,
        };
        processor
            .replace_slot(
                0,
                FakePlugin {
                    gain: 2.0,
                    ..FakePlugin::default()
                },
                PluginSlotConfiguration::active(topology),
            )
            .expect("first fixed slot");
        processor
            .replace_slot(
                1,
                FakePlugin {
                    gain: 0.5,
                    ..FakePlugin::default()
                },
                PluginSlotConfiguration::active(topology),
            )
            .expect("second fixed slot");
        let mut slot = requested_slot();

        processor
            .process_block(&mut slot)
            .expect("serial processing");

        assert!((slot.output_audio[0][0] - 0.25).abs() < f32::EPSILON);
        assert!((slot.output_audio[1][0] + 0.5).abs() < f32::EPSILON);
        assert_eq!(processor.current_slot(), None);
    }

    #[test]
    fn sidechained_slot_reads_its_region_only_when_the_request_marks_it() {
        let mut processor = RackProcessor::new();
        let configuration = PluginSlotConfiguration::active(PluginTopology {
            input_channels: 2,
            output_channels: 2,
        });
        processor
            .replace_slot(0, FakePlugin::default(), configuration)
            .expect("plain slot");
        processor
            .replace_slot(
                1,
                FakePlugin::default(),
                PluginSlotConfiguration {
                    sidechain: true,
                    ..configuration
                },
            )
            .expect("sidechained slot");
        let received = |processor: &RackProcessor<FakePlugin>| {
            processor
                .plugins()
                .map(|(_, plugin)| plugin.sidechain)
                .collect::<Vec<_>>()
        };
        for (sidechain_slots, expected) in [(0b10, Some(0.75)), (0, Some(0.0))] {
            let mut slot = BlockSlot::new();
            slot.sidechain_audio[0][0][0] = 0.5;
            slot.sidechain_audio[1][0][0] = 0.75;
            slot.publish_request(
                BlockTicket {
                    generation: 1,
                    sequence: 1,
                },
                BlockRequest {
                    frame_count: 1,
                    input_channel_count: 2,
                    output_channel_count: 2,
                    midi_event_count: 0,
                    event_count: 0,
                    flags: 0,
                    sidechain_slots,
                },
            )
            .expect("valid sidechain request");
            processor.process_block(&mut slot).expect("processing");
            assert_eq!(received(&processor), [None, expected]);
        }
    }

    #[test]
    fn state_restore_requires_an_inactive_slot() {
        let mut processor = RackProcessor::new();
        processor
            .replace_slot(
                0,
                FakePlugin::default(),
                PluginSlotConfiguration::active(PluginTopology {
                    input_channels: 2,
                    output_channels: 2,
                }),
            )
            .expect("fixed slot");
        assert!(processor.restore_state(0, &PluginState::default()).is_err());
        processor.set_active(0, false).expect("deactivate");
        processor
            .restore_state(0, &PluginState::default())
            .expect("inactive restore");
    }

    #[test]
    fn restart_flags_keep_slot_identity_and_drain_once() {
        let mut processor = RackProcessor::new();
        let configuration = PluginSlotConfiguration::active(PluginTopology {
            input_channels: 2,
            output_channels: 2,
        });
        for (slot, flags) in [(0, 1), (3, 8)] {
            processor
                .replace_slot(
                    slot,
                    FakePlugin {
                        restart_flags: flags,
                        ..FakePlugin::default()
                    },
                    configuration,
                )
                .expect("fixed slot");
        }
        let flags = processor.take_slot_restart_flags();
        assert_eq!(flags[0], 1);
        assert_eq!(flags[3], 8);
        assert!(
            flags
                .iter()
                .enumerate()
                .all(|(slot, flags)| { slot == 0 || slot == 3 || *flags == 0 })
        );
        assert_eq!(
            processor.take_slot_restart_flags(),
            [0; MAX_PLUGINS_PER_RACK]
        );
    }

    #[test]
    fn block_bypass_event_targets_only_its_declared_slot() {
        let mut processor = RackProcessor::new();
        let topology = PluginTopology {
            input_channels: 2,
            output_channels: 2,
        };
        for (slot, gain) in [2.0, 3.0].into_iter().enumerate() {
            processor
                .replace_slot(
                    slot,
                    FakePlugin {
                        gain,
                        ..FakePlugin::default()
                    },
                    PluginSlotConfiguration::active(topology),
                )
                .expect("fixed slot");
        }
        let mut slot = requested_slot_with_event(BlockEvent {
            frame_offset: 0,
            event_type: sp_protocol::BLOCK_EVENT_SLOT_BYPASS,
            key: 0,
            value: 1.0,
            flags: 2,
        });

        // Each one-frame block repeats the bypass event; wait out the fade.
        for _ in 0..=SLOT_FADE_FRAMES {
            processor.process_block(&mut slot).expect("targeted bypass");
        }

        assert!((slot.output_audio[0][0] - 0.5).abs() < f32::EPSILON);
        assert!((slot.output_audio[1][0] + 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn bypassed_slot_processes_input_but_passes_it_on_delayed_by_its_latency() {
        let mut processor = RackProcessor::new();
        processor
            .replace_slot(
                0,
                FakePlugin {
                    gain: 2.0,
                    latency: 1,
                    ..FakePlugin::default()
                },
                PluginSlotConfiguration {
                    bypassed: true,
                    ..PluginSlotConfiguration::active(PluginTopology {
                        input_channels: 2,
                        output_channels: 2,
                    })
                },
            )
            .expect("fixed slot");
        let mut slot = requested_slot();

        processor.process_block(&mut slot).expect("first block");
        assert!(
            slot.output_audio[0][0].abs() < f32::EPSILON,
            "delayed by one frame"
        );
        processor.process_block(&mut slot).expect("second block");
        assert!(
            (slot.output_audio[0][0] - 0.25).abs() < f32::EPSILON,
            "input, not output"
        );

        assert_eq!(processor.latency_samples(), 1, "bypass keeps the latency");
        let plugin = processor.plugins().next().expect("slot").1;
        assert_eq!(
            plugin.processed_blocks, 2,
            "bypassed plug-in still processes"
        );
    }

    /// The requests here are one frame long, so each fade frame is one block.
    #[test]
    fn inserted_slot_fades_in_only_after_warmup_blocks() {
        let mut processor = RackProcessor::new();
        processor
            .replace_slot(
                0,
                FakePlugin {
                    gain: 2.0,
                    ..FakePlugin::default()
                },
                PluginSlotConfiguration::active(PluginTopology {
                    input_channels: 2,
                    output_channels: 2,
                }),
            )
            .expect("fixed slot");
        processor.start_warmup(0).expect("warmup");
        let mut slot = requested_slot();

        for _ in 0..WARMUP_BLOCKS {
            processor.process_block(&mut slot).expect("warming block");
            assert!(
                (slot.output_audio[0][0] - 0.25).abs() < f32::EPSILON,
                "unheard"
            );
        }
        for frame in 0..SLOT_FADE_FRAMES {
            processor.process_block(&mut slot).expect("fading block");
            #[allow(clippy::cast_precision_loss, reason = "small fade frame index")]
            let expected = 0.25 + 0.25 * frame as f32 / SLOT_FADE_FRAMES as f32;
            assert!(
                (slot.output_audio[0][0] - expected).abs() < 1e-6,
                "fading in"
            );
        }
        processor.process_block(&mut slot).expect("joined block");
        assert!(
            (slot.output_audio[0][0] - 0.5).abs() < f32::EPSILON,
            "heard"
        );
    }

    #[test]
    fn mono_slot_downmixes_input_and_duplicates_output_without_allocation() {
        let mut processor = RackProcessor::new();
        processor
            .replace_slot(
                0,
                FakePlugin::default(),
                PluginSlotConfiguration::active(PluginTopology {
                    input_channels: 1,
                    output_channels: 1,
                }),
            )
            .expect("fixed slot");
        let mut slot = requested_slot();

        processor.process_block(&mut slot).expect("mono adaptation");
        assert!((slot.output_audio[0][0] + 0.125).abs() < f32::EPSILON);
        assert!((slot.output_audio[1][0] + 0.125).abs() < f32::EPSILON);
    }
}
