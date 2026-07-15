//! MIDI ingress boundary types and CoreMIDI-backed device input via `midir`.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Instant;

use rtrb::{Consumer, Producer, RingBuffer};
use sp_model::{NormalizedValue, ParameterId, Scene, SceneParameterValue, SlotBypass};

/// Maximum number of MIDI events retained for one audio processing block.
pub const MAX_MIDI_EVENTS_PER_BLOCK: usize = 256;

const MIDI_MESSAGE_BYTES: usize = 3;
const SAFETY_MIDI_EVENTS: usize = 64;

/// Identifies a MIDI source without retaining a platform-specific handle.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MidiPortId(String);

impl MidiPortId {
    /// Creates an identifier from a stable backend-provided value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the backend-provided identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A timestamped raw MIDI packet.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MidiEvent {
    /// Event offset in audio frames from the enclosing processing block.
    pub frame_offset: u32,
    /// Monotonic backend timestamp in microseconds, or zero for synthetic events.
    pub timestamp_micros: u64,
    /// Complete raw MIDI message bytes.
    pub bytes: Vec<u8>,
}

impl MidiEvent {
    /// Constructs a MIDI event, rejecting empty packets at the boundary.
    ///
    /// # Errors
    ///
    /// Returns [`MidiEventError::EmptyPacket`] when `bytes` is empty.
    pub fn new(frame_offset: u32, bytes: Vec<u8>) -> Result<Self, MidiEventError> {
        if bytes.is_empty() {
            return Err(MidiEventError::EmptyPacket);
        }
        Ok(Self {
            frame_offset,
            timestamp_micros: 0,
            bytes,
        })
    }
}

/// Validation failure for [`MidiEvent`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MidiEventError {
    /// MIDI packets contain at least one byte.
    EmptyPacket,
}

impl fmt::Display for MidiEventError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("MIDI packet must not be empty")
    }
}

impl std::error::Error for MidiEventError {}

/// Source of timestamped MIDI packets for an engine processing block.
pub trait MidiInput {
    /// Drains packets currently available from the source.
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying MIDI source cannot be read.
    fn drain_events(&mut self) -> Result<Vec<MidiEvent>, Box<dyn std::error::Error + Send + Sync>>;
}

/// A bounded single-producer, single-consumer MIDI queue.
///
/// The queue preallocates all event slots and rejects new events when it is full.
/// Rejection preserves the caller's event and increments [`Self::rejected_count`].
#[derive(Debug)]
pub struct SpscMidiQueue {
    events: [Option<MidiEvent>; MAX_MIDI_EVENTS_PER_BLOCK],
    read_index: usize,
    write_index: usize,
    len: usize,
    rejected_count: u64,
}

impl Default for SpscMidiQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl SpscMidiQueue {
    /// Creates an empty queue with storage for [`MAX_MIDI_EVENTS_PER_BLOCK`] events.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            events: [const { None }; MAX_MIDI_EVENTS_PER_BLOCK],
            read_index: 0,
            write_index: 0,
            len: 0,
            rejected_count: 0,
        }
    }

    /// Attempts to enqueue an event without allocating.
    ///
    /// Returns the unchanged event when the fixed-capacity queue is full.
    ///
    /// # Errors
    ///
    /// Returns the supplied event when the queue is full.
    pub fn try_push(&mut self, event: MidiEvent) -> Result<(), MidiEvent> {
        if self.len == MAX_MIDI_EVENTS_PER_BLOCK {
            self.rejected_count += 1;
            return Err(event);
        }

        self.events[self.write_index] = Some(event);
        self.write_index = (self.write_index + 1) % MAX_MIDI_EVENTS_PER_BLOCK;
        self.len += 1;
        Ok(())
    }

    /// Drains all queued events into `output`, preserving their insertion order.
    ///
    /// The destination may allocate if it has insufficient capacity.
    pub fn drain_into(&mut self, output: &mut Vec<MidiEvent>) {
        while self.len > 0 {
            if let Some(event) = self.events[self.read_index].take() {
                output.push(event);
            }
            self.read_index = (self.read_index + 1) % MAX_MIDI_EVENTS_PER_BLOCK;
            self.len -= 1;
        }
    }

    /// Returns the number of events currently queued.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns whether no events are currently queued.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns the total number of events rejected because the queue was full.
    #[must_use]
    pub const fn rejected_count(&self) -> u64 {
        self.rejected_count
    }
}

/// A parameter selected by MIDI learn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MidiMappingTarget {
    /// Zero-based index of the rack containing the target.
    pub rack_index: usize,
    /// Zero-based index of the plug-in slot within the rack.
    pub slot_index: usize,
    /// Stable plug-in parameter identifier.
    pub parameter_id: u32,
}

/// An immutable snapshot of controller-to-parameter MIDI learn mappings.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MidiLearnTable {
    mappings: BTreeMap<u8, MidiMappingTarget>,
}

impl MidiLearnTable {
    /// Creates a snapshot from controller mappings.
    #[must_use]
    pub fn from_mappings(mappings: BTreeMap<u8, MidiMappingTarget>) -> Self {
        Self { mappings }
    }

    /// Returns the target learned for a controller number.
    #[must_use]
    pub fn target_for(&self, controller: u8) -> Option<MidiMappingTarget> {
        self.mappings.get(&controller).copied()
    }

    /// Replaces this snapshot atomically from the caller's perspective.
    ///
    /// Returns the previous snapshot so callers can retain or discard it deliberately.
    #[must_use]
    pub fn replace(&mut self, replacement: Self) -> Self {
        std::mem::replace(self, replacement)
    }
}

/// Converts a MIDI controller value to a normalized parameter value in `0.0..=1.0`.
#[must_use]
pub fn map_controller_to_normalized(cc_value: u8) -> f32 {
    f32::from(cc_value.min(127)) / 127.0
}

/// Converts a host-tick timestamp to a frame offset inside an audio block.
///
/// Timestamps before the block map to frame zero; timestamps beyond the block map to
/// the final frame. A zero-length block and invalid ticks-per-frame both map to zero.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)] // The non-negative result is clamped to the u32 frame range below.
pub fn sample_offset_for_timestamp(
    event_host_tick: u64,
    block_start_tick: u64,
    ticks_per_frame: f64,
    frames: u32,
) -> u32 {
    if frames == 0 || !ticks_per_frame.is_finite() || ticks_per_frame <= 0.0 {
        return 0;
    }

    let elapsed_ticks = event_host_tick.saturating_sub(block_start_tick) as f64;
    let offset = (elapsed_ticks / ticks_per_frame).floor() as u64;
    offset.min(u64::from(frames - 1)) as u32
}

/// Discovered MIDI input port.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MidiPortInfo {
    /// Stable port identifier.
    pub id: MidiPortId,
    /// Display name.
    pub name: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RealtimeMidiPacket {
    timestamp_micros: u64,
    bytes: [u8; MIDI_MESSAGE_BYTES],
    len: u8,
}

impl RealtimeMidiPacket {
    fn from_message(timestamp_micros: u64, message: &[u8]) -> Option<Self> {
        if message.is_empty() || message.len() > MIDI_MESSAGE_BYTES {
            return None;
        }
        let mut bytes = [0; MIDI_MESSAGE_BYTES];
        bytes[..message.len()].copy_from_slice(message);
        Some(Self {
            timestamp_micros,
            bytes,
            len: u8::try_from(message.len()).ok()?,
        })
    }

    fn is_safety_event(self) -> bool {
        let kind = self.bytes[0] & 0xF0;
        kind == 0x80 || (kind == 0x90 && self.len == 3 && self.bytes[2] == 0)
    }

    fn into_event(self) -> MidiEvent {
        MidiEvent {
            frame_offset: 0,
            timestamp_micros: self.timestamp_micros,
            bytes: self.bytes[..usize::from(self.len)].to_vec(),
        }
    }
}

#[derive(Debug, Default)]
struct IngressTelemetry {
    received: AtomicU64,
    rejected: AtomicU64,
}

fn ingress_queues() -> (
    Producer<RealtimeMidiPacket>,
    Consumer<RealtimeMidiPacket>,
    Producer<RealtimeMidiPacket>,
    Consumer<RealtimeMidiPacket>,
) {
    let (regular_producer, regular_consumer) = RingBuffer::new(MAX_MIDI_EVENTS_PER_BLOCK);
    let (safety_producer, safety_consumer) = RingBuffer::new(SAFETY_MIDI_EVENTS);
    (
        regular_producer,
        regular_consumer,
        safety_producer,
        safety_consumer,
    )
}

/// `CoreMIDI` / system MIDI input backed by `midir`, feeding an [`SpscMidiQueue`].
pub struct MidirInput {
    connection: Option<midir::MidiInputConnection<()>>,
    regular: Consumer<RealtimeMidiPacket>,
    safety: Consumer<RealtimeMidiPacket>,
    telemetry: Arc<IngressTelemetry>,
    port: Option<MidiPortInfo>,
    opened_at: Instant,
}

impl MidirInput {
    /// Creates an unconnected input.
    #[must_use]
    pub fn new() -> Self {
        let (_, regular, _, safety) = ingress_queues();
        Self {
            connection: None,
            regular,
            safety,
            telemetry: Arc::new(IngressTelemetry::default()),
            port: None,
            opened_at: Instant::now(),
        }
    }

    /// Lists available MIDI input ports.
    ///
    /// # Errors
    ///
    /// Returns an error when the platform MIDI client cannot be created.
    pub fn enumerate_ports() -> Result<Vec<MidiPortInfo>, Box<dyn std::error::Error + Send + Sync>>
    {
        let input = midir::MidiInput::new("superposition-enumerate")?;
        Ok(input
            .ports()
            .iter()
            .enumerate()
            .filter_map(|(index, port)| {
                let name = input.port_name(port).ok()?;
                Some(MidiPortInfo {
                    id: MidiPortId::new(format!("midi-{index}")),
                    name,
                })
            })
            .collect())
    }

    /// Opens the first available input port, if any.
    ///
    /// # Errors
    ///
    /// Returns an error when the MIDI client cannot be created. Absence of ports is not an
    /// error; [`Self::connected_port`] returns `None`.
    pub fn open_first_available(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let input = midir::MidiInput::new("superposition")?;
        let ports = input.ports();
        let Some(port) = ports.first() else {
            self.port = None;
            self.connection = None;
            return Ok(());
        };
        let name = input.port_name(port)?;
        let (mut regular_producer, regular, mut safety_producer, safety) = ingress_queues();
        let telemetry = Arc::new(IngressTelemetry::default());
        let callback_telemetry = Arc::clone(&telemetry);
        let connection = input.connect(
            port,
            "superposition-input",
            move |timestamp_micros, message, ()| {
                let Some(packet) = RealtimeMidiPacket::from_message(timestamp_micros, message)
                else {
                    callback_telemetry.rejected.fetch_add(1, Ordering::Relaxed);
                    return;
                };
                callback_telemetry.received.fetch_add(1, Ordering::Relaxed);
                let result = if packet.is_safety_event() {
                    safety_producer.push(packet)
                } else {
                    regular_producer.push(packet)
                };
                if result.is_err() {
                    callback_telemetry.rejected.fetch_add(1, Ordering::Relaxed);
                }
            },
            (),
        )?;
        self.port = Some(MidiPortInfo {
            id: MidiPortId::new("midi-0"),
            name,
        });
        self.connection = Some(connection);
        self.regular = regular;
        self.safety = safety;
        self.telemetry = telemetry;
        self.opened_at = Instant::now();
        Ok(())
    }

    /// Returns the connected port, when open.
    #[must_use]
    pub fn connected_port(&self) -> Option<&MidiPortInfo> {
        self.port.as_ref()
    }

    /// Pushes pending device events into `queue`, preserving note-offs under pressure.
    pub fn pump_into(&mut self, queue: &mut SpscMidiQueue) {
        while let Ok(packet) = self.safety.pop() {
            if queue.try_push(packet.into_event()).is_err() {
                self.telemetry.rejected.fetch_add(1, Ordering::Relaxed);
            }
        }
        while let Ok(packet) = self.regular.pop() {
            if queue.try_push(packet.into_event()).is_err() {
                self.telemetry.rejected.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Returns monotonic microseconds since open for timing reports.
    #[must_use]
    pub fn micros_since_open(&self) -> u64 {
        u64::try_from(self.opened_at.elapsed().as_micros()).unwrap_or(u64::MAX)
    }

    /// Returns whether a device packet has been observed.
    #[must_use]
    pub fn has_received_traffic(&self) -> bool {
        self.telemetry.received.load(Ordering::Relaxed) > 0
    }

    /// Returns the number of invalid or capacity-rejected callback packets.
    #[must_use]
    pub fn rejected_count(&self) -> u64 {
        self.telemetry.rejected.load(Ordering::Relaxed)
    }
}

impl Default for MidirInput {
    fn default() -> Self {
        Self::new()
    }
}

impl MidiInput for MidirInput {
    fn drain_events(&mut self) -> Result<Vec<MidiEvent>, Box<dyn std::error::Error + Send + Sync>> {
        let mut queue = SpscMidiQueue::new();
        self.pump_into(&mut queue);
        let mut events = Vec::new();
        queue.drain_into(&mut events);
        Ok(events)
    }
}

/// Live parameter target updated by scene recall (never opaque state).
#[derive(Clone, Debug, PartialEq)]
pub struct SceneParameterTarget {
    /// Rack containing the parameter.
    pub rack_index: usize,
    /// Slot containing the parameter.
    pub slot_index: usize,
    /// Stable parameter identifier.
    pub parameter_id: ParameterId,
    /// Normalized value at the end of the ramp.
    pub value: NormalizedValue,
}

/// One step of a deterministic scene ramp.
#[derive(Clone, Debug, PartialEq)]
pub struct SceneRampStep {
    /// Targets to apply at this step.
    pub targets: Vec<SceneParameterTarget>,
    /// Elapsed milliseconds from scene trigger.
    pub elapsed_ms: u32,
    /// Total transition duration.
    pub transition_ms: u32,
}

/// Applies a parameter scene without restoring opaque plug-in state.
#[derive(Clone, Debug, Default)]
pub struct ScenePlayer {
    active: Option<ActiveScene>,
}

#[derive(Clone, Debug)]
struct ActiveScene {
    start: Instant,
    transition_ms: u32,
    start_values: Vec<SceneParameterTarget>,
    end_values: Vec<SceneParameterTarget>,
    bypasses: Vec<SlotBypass>,
}

impl ScenePlayer {
    /// Creates an idle player.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Arms a scene recall. Opaque component/controller blobs are never applied here.
    pub fn trigger(
        &mut self,
        scene: &Scene,
        current: Vec<SceneParameterTarget>,
        resolve_slot: impl Fn(&SceneParameterValue) -> Option<(usize, usize)>,
    ) {
        let end_values = scene
            .parameter_values
            .iter()
            .filter_map(|parameter| {
                let (rack_index, slot_index) = resolve_slot(parameter)?;
                Some(SceneParameterTarget {
                    rack_index,
                    slot_index,
                    parameter_id: parameter.parameter_id.clone(),
                    value: parameter.value,
                })
            })
            .collect();
        self.active = Some(ActiveScene {
            start: Instant::now(),
            transition_ms: scene.transition_ms.max(1),
            start_values: current,
            end_values,
            bypasses: scene.bypasses.clone(),
        });
    }

    /// Returns the interpolated parameter targets for the current instant.
    #[must_use]
    pub fn poll(&self) -> Option<SceneRampStep> {
        let active = self.active.as_ref()?;
        let elapsed_ms = u32::try_from(active.start.elapsed().as_millis()).unwrap_or(u32::MAX);
        let transition = active.transition_ms.max(1);
        #[allow(clippy::cast_precision_loss)]
        let progress = (elapsed_ms.min(transition) as f32 / transition as f32).clamp(0.0, 1.0);
        let mut targets = Vec::with_capacity(active.end_values.len());
        for end in &active.end_values {
            let start_value = active
                .start_values
                .iter()
                .find(|start| {
                    start.rack_index == end.rack_index
                        && start.slot_index == end.slot_index
                        && start.parameter_id == end.parameter_id
                })
                .map_or(0.0, |start| start.value.get());
            let value = start_value + (end.value.get() - start_value) * progress;
            targets.push(SceneParameterTarget {
                rack_index: end.rack_index,
                slot_index: end.slot_index,
                parameter_id: end.parameter_id.clone(),
                value: NormalizedValue::new(value).unwrap_or(end.value),
            });
        }
        Some(SceneRampStep {
            targets,
            elapsed_ms,
            transition_ms: active.transition_ms,
        })
    }

    /// Returns bypass overrides captured with the active scene.
    #[must_use]
    pub fn active_bypasses(&self) -> &[SlotBypass] {
        self.active
            .as_ref()
            .map_or(&[], |active| active.bypasses.as_slice())
    }

    /// Returns whether a scene ramp is in progress.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.active.is_some()
    }
}

/// Program-change scene trigger: maps program number → scene index.
#[must_use]
pub fn scene_index_for_program(program: u8, scene_count: usize) -> Option<usize> {
    let index = usize::from(program);
    (index < scene_count).then_some(index)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{
        MAX_MIDI_EVENTS_PER_BLOCK, MidiEvent, MidiEventError, MidiLearnTable, MidiMappingTarget,
        RealtimeMidiPacket, SpscMidiQueue, map_controller_to_normalized,
        sample_offset_for_timestamp,
    };

    #[test]
    fn rejects_empty_packet() {
        assert_eq!(MidiEvent::new(0, vec![]), Err(MidiEventError::EmptyPacket));
    }

    #[test]
    fn queue_rejects_new_events_when_full_and_counts_them() {
        let mut queue = SpscMidiQueue::new();
        for index in 0..MAX_MIDI_EVENTS_PER_BLOCK {
            let frame = u32::try_from(index).expect("test index fits u32");
            queue
                .try_push(MidiEvent::new(frame, vec![0x90]).unwrap())
                .unwrap();
        }

        let rejected = MidiEvent::new(999, vec![0x80]).unwrap();
        assert_eq!(queue.try_push(rejected.clone()), Err(rejected));
        assert_eq!(queue.rejected_count(), 1);
        assert_eq!(queue.len(), MAX_MIDI_EVENTS_PER_BLOCK);
    }

    #[test]
    fn queue_drains_in_insertion_order() {
        let mut queue = SpscMidiQueue::new();
        queue
            .try_push(MidiEvent::new(2, vec![0x90]).unwrap())
            .unwrap();
        queue
            .try_push(MidiEvent::new(4, vec![0x80]).unwrap())
            .unwrap();
        let mut events = Vec::with_capacity(2);

        queue.drain_into(&mut events);

        assert_eq!(
            events
                .iter()
                .map(|event| event.frame_offset)
                .collect::<Vec<_>>(),
            [2, 4]
        );
        assert!(queue.is_empty());
    }

    #[test]
    fn mapping_snapshots_replace_as_a_whole() {
        let target = MidiMappingTarget {
            rack_index: 1,
            slot_index: 2,
            parameter_id: 3,
        };
        let mut mappings = BTreeMap::new();
        mappings.insert(74, target);
        let mut table = MidiLearnTable::from_mappings(mappings);

        let previous = table.replace(MidiLearnTable::default());

        assert_eq!(previous.target_for(74), Some(target));
        assert_eq!(table.target_for(74), None);
    }

    #[test]
    fn controller_values_and_timestamps_are_normalized_and_clamped() {
        assert!((map_controller_to_normalized(0) - 0.0).abs() <= f32::EPSILON);
        assert!((map_controller_to_normalized(127) - 1.0).abs() <= f32::EPSILON);
        assert_eq!(sample_offset_for_timestamp(104, 100, 2.0, 8), 2);
        assert_eq!(sample_offset_for_timestamp(90, 100, 2.0, 8), 0);
        assert_eq!(sample_offset_for_timestamp(200, 100, 2.0, 8), 7);
    }

    #[test]
    fn realtime_packet_accepts_short_midi_and_classifies_note_offs() {
        let note_on = RealtimeMidiPacket::from_message(10, &[0x90, 60, 100]).unwrap();
        let note_off = RealtimeMidiPacket::from_message(11, &[0x80, 60, 0]).unwrap();
        let zero_velocity = RealtimeMidiPacket::from_message(12, &[0x90, 60, 0]).unwrap();

        assert!(!note_on.is_safety_event());
        assert!(note_off.is_safety_event());
        assert!(zero_velocity.is_safety_event());
        assert!(RealtimeMidiPacket::from_message(13, &[0xF0, 1, 2, 0xF7]).is_none());
    }
}
