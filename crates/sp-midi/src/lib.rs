//! Allocation-free MIDI ingress, learn mapping publication, and deterministic scene ramps.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Instant;

use rtrb::{Consumer, Producer, RingBuffer};
use sp_model::{Scene, SceneParameterTransition, SceneParameterValue};

/// Maximum number of MIDI events delivered to one audio processing block.
pub const MAX_MIDI_EVENTS_PER_BLOCK: usize = 256;
/// Maximum MIDI 1.0 message size accepted on the realtime path.
pub const MIDI_MESSAGE_BYTES: usize = 3;
/// Maximum scene parameter values accepted by the realtime scene player.
pub const MAX_SCENE_PARAMETERS: usize = 256;
const MIDI_MAPPING_SLOTS: usize = 16 * 128;

/// Identifies a MIDI source without retaining a platform-specific handle.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MidiPortId(String);

impl MidiPortId {
    /// Creates an identifier from a backend-provided stable value.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the stable selection value.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A fixed-size, timestamped MIDI 1.0 packet safe to copy in a callback.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MidiEvent {
    /// Event offset in audio frames from the enclosing processing block.
    pub frame_offset: u32,
    /// Monotonic backend timestamp in microseconds, or zero for synthetic events.
    pub timestamp_micros: u64,
    /// Zero-padded MIDI message bytes.
    pub bytes: [u8; MIDI_MESSAGE_BYTES],
    /// Number of valid bytes in [`Self::bytes`].
    pub len: u8,
}

impl MidiEvent {
    /// Constructs an event from a MIDI 1.0 packet.
    ///
    /// The supplied vector is consumed at the non-realtime API boundary. Use
    /// [`Self::from_bytes`] from callback-adjacent code.
    ///
    /// # Errors
    /// Returns [`MidiEventError`] for empty or oversized packets.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "consuming the vector keeps this convenience API off the realtime path"
    )]
    pub fn new(frame_offset: u32, bytes: Vec<u8>) -> Result<Self, MidiEventError> {
        Self::from_bytes(frame_offset, &bytes)
    }

    /// Constructs an event without allocating.
    ///
    /// # Errors
    /// Returns [`MidiEventError`] for empty or oversized packets.
    pub fn from_bytes(frame_offset: u32, bytes: &[u8]) -> Result<Self, MidiEventError> {
        if bytes.is_empty() {
            return Err(MidiEventError::EmptyPacket);
        }
        if bytes.len() > MIDI_MESSAGE_BYTES {
            return Err(MidiEventError::PacketTooLong);
        }
        let mut data = [0; MIDI_MESSAGE_BYTES];
        data[..bytes.len()].copy_from_slice(bytes);
        Ok(Self {
            frame_offset,
            timestamp_micros: 0,
            bytes: data,
            #[allow(
                clippy::cast_possible_truncation,
                reason = "length is bounded by MIDI_MESSAGE_BYTES above"
            )]
            len: bytes.len() as u8,
        })
    }

    /// Returns the valid MIDI bytes.
    #[must_use]
    pub fn message(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }

    /// Returns whether this packet releases an active MIDI note.
    #[must_use]
    pub const fn is_note_off(self) -> bool {
        let kind = self.bytes[0] & 0xf0;
        kind == 0x80 || (kind == 0x90 && self.len == 3 && self.bytes[2] == 0)
    }

    /// Returns whether this event must survive ordinary controller traffic to release sound.
    #[must_use]
    pub const fn is_safety_event(self) -> bool {
        self.is_note_off()
            || (self.len == 3
                && self.bytes[0] & 0xf0 == 0xb0
                && matches!(self.bytes[1], 120 | 121 | 123))
    }

    fn continuous_key(self) -> Option<(u8, u8)> {
        match self.bytes[0] & 0xf0 {
            0xb0 if !self.is_safety_event() => Some((self.bytes[0], self.bytes[1])),
            // Pitch bend and channel pressure have one current value per channel.
            0xd0 | 0xe0 => Some((self.bytes[0], 0)),
            _ => None,
        }
    }
}

/// Validation failure for [`MidiEvent`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MidiEventError {
    /// MIDI packets contain at least one byte.
    EmptyPacket,
    /// Only MIDI 1.0 channel messages of up to three bytes are supported.
    PacketTooLong,
}

impl fmt::Display for MidiEventError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPacket => formatter.write_str("MIDI packet must not be empty"),
            Self::PacketTooLong => {
                formatter.write_str("MIDI packet exceeds the three-byte MIDI 1.0 limit")
            }
        }
    }
}

impl std::error::Error for MidiEventError {}

const EMPTY_MIDI_EVENT: MidiEvent = MidiEvent {
    frame_offset: 0,
    timestamp_micros: 0,
    bytes: [0; MIDI_MESSAGE_BYTES],
    len: 0,
};

/// Fixed-capacity block event storage. It never allocates while accepting or draining events.
#[derive(Clone, Debug)]
pub struct BoundedMidiEvents<const CAPACITY: usize = MAX_MIDI_EVENTS_PER_BLOCK> {
    events: [MidiEvent; CAPACITY],
    len: usize,
}

impl<const CAPACITY: usize> Default for BoundedMidiEvents<CAPACITY> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const CAPACITY: usize> BoundedMidiEvents<CAPACITY> {
    /// Creates empty storage.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            events: [EMPTY_MIDI_EVENT; CAPACITY],
            len: 0,
        }
    }

    /// Appends an event, or returns it unchanged when full.
    ///
    /// # Errors
    /// Returns the event back when the block storage is full.
    pub fn try_push(&mut self, event: MidiEvent) -> Result<(), MidiEvent> {
        if self.len == CAPACITY {
            return Err(event);
        }
        self.events[self.len] = event;
        self.len += 1;
        Ok(())
    }

    /// Removes all events while retaining the backing storage.
    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// Returns the populated events in insertion order.
    #[must_use]
    pub fn as_slice(&self) -> &[MidiEvent] {
        &self.events[..self.len]
    }

    /// Returns the number of populated events.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns whether storage contains no events.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Source of timestamped MIDI packets for an engine processing block.
pub trait MidiInput {
    /// Drains packets currently available into preallocated block storage.
    ///
    /// # Errors
    /// Returns an error when the underlying input can no longer be drained.
    fn drain_into(
        &mut self,
        output: &mut BoundedMidiEvents,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

/// Bounded MIDI queue with a dedicated note-off lane and continuous-control coalescing.
///
/// Each lane has a full block's capacity: a burst of regular controls cannot consume storage
/// reserved for releases, and a burst of releases cannot be displaced by regular controls.
#[derive(Debug)]
pub struct SpscMidiQueue {
    safety: BoundedMidiEvents,
    regular: BoundedMidiEvents,
    safety_sequence: [u64; MAX_MIDI_EVENTS_PER_BLOCK],
    regular_sequence: [u64; MAX_MIDI_EVENTS_PER_BLOCK],
    next_sequence: u64,
    rejected_count: u64,
    coalesced_count: u64,
}

impl Default for SpscMidiQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl SpscMidiQueue {
    /// Creates an empty queue.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            safety: BoundedMidiEvents::new(),
            regular: BoundedMidiEvents::new(),
            safety_sequence: [0; MAX_MIDI_EVENTS_PER_BLOCK],
            regular_sequence: [0; MAX_MIDI_EVENTS_PER_BLOCK],
            next_sequence: 0,
            rejected_count: 0,
            coalesced_count: 0,
        }
    }

    /// Enqueues an event without allocating. Note-offs use a protected lane; redundant CC,
    /// pitch-bend, and channel-pressure values replace their pending predecessor.
    ///
    /// # Errors
    /// Returns the event back when its lane is full.
    pub fn try_push(&mut self, event: MidiEvent) -> Result<(), MidiEvent> {
        if !self.can_accept(event) {
            self.rejected_count += 1;
            return Err(event);
        }
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1);
        if event.is_safety_event() {
            self.safety_sequence[self.safety.len] = sequence;
            return self.safety.try_push(event);
        }
        if let Some(key) = event.continuous_key()
            && let Some(index) = self.regular.events[..self.regular.len]
                .iter()
                .position(|existing| existing.continuous_key() == Some(key))
        {
            self.regular
                .events
                .copy_within(index + 1..self.regular.len, index);
            self.regular_sequence
                .copy_within(index + 1..self.regular.len, index);
            self.regular.events[self.regular.len - 1] = event;
            self.regular_sequence[self.regular.len - 1] = sequence;
            self.coalesced_count += 1;
            return Ok(());
        }
        self.regular_sequence[self.regular.len] = sequence;
        self.regular.try_push(event)
    }

    fn can_accept(&self, event: MidiEvent) -> bool {
        if event.is_safety_event() {
            return self.safety.len < MAX_MIDI_EVENTS_PER_BLOCK;
        }
        self.regular.len < MAX_MIDI_EVENTS_PER_BLOCK
            || event.continuous_key().is_some_and(|key| {
                self.regular.events[..self.regular.len]
                    .iter()
                    .any(|existing| existing.continuous_key() == Some(key))
            })
    }

    /// Drains a block in arrival order without allocating. Protected note-offs cannot be
    /// displaced by regular events. Events beyond `output` capacity remain queued.
    pub fn drain_into<const CAPACITY: usize>(&mut self, output: &mut BoundedMidiEvents<CAPACITY>) {
        let mut safety_index = 0;
        let mut regular_index = 0;
        while output.len < CAPACITY
            && (safety_index < self.safety.len || regular_index < self.regular.len)
        {
            let safety_remaining = self.safety.len - safety_index;
            let output_remaining = CAPACITY - output.len;
            let safety = (safety_remaining >= output_remaining && safety_remaining > 0)
                || regular_index == self.regular.len
                || (safety_index < self.safety.len
                    && self.safety_sequence[safety_index] < self.regular_sequence[regular_index]);
            output.events[output.len] = if safety {
                let event = self.safety.events[safety_index];
                safety_index += 1;
                event
            } else {
                let event = self.regular.events[regular_index];
                regular_index += 1;
                event
            };
            output.len += 1;
        }
        Self::discard_prefix(&mut self.safety, &mut self.safety_sequence, safety_index);
        Self::discard_prefix(&mut self.regular, &mut self.regular_sequence, regular_index);
    }

    fn discard_prefix(
        lane: &mut BoundedMidiEvents,
        sequences: &mut [u64; MAX_MIDI_EVENTS_PER_BLOCK],
        count: usize,
    ) {
        lane.events.copy_within(count..lane.len, 0);
        sequences.copy_within(count..lane.len, 0);
        lane.len -= count;
        lane.events[lane.len..lane.len + count].fill(EMPTY_MIDI_EVENT);
    }

    /// Returns total queued events across both lanes.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.safety.len + self.regular.len
    }
    /// Returns whether both lanes are empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Returns events rejected only after their appropriate protected/coalescing lane was full.
    #[must_use]
    pub const fn rejected_count(&self) -> u64 {
        self.rejected_count
    }
    /// Returns continuous events replaced by a newer pending value.
    #[must_use]
    pub const fn coalesced_count(&self) -> u64 {
        self.coalesced_count
    }
}

/// A parameter selected by MIDI learn.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MidiMappingTarget {
    /// Zero-based rack index containing the parameter.
    pub rack_index: usize,
    /// Zero-based plug-in slot index containing the parameter.
    pub slot_index: usize,
    /// Engine-assigned numeric parameter address.
    pub parameter_id: u32,
    /// Normalized output produced by MIDI value zero.
    pub minimum: f32,
    /// Normalized output produced by MIDI value 127.
    pub maximum: f32,
}

/// Immutable controller-to-parameter mapping snapshot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MidiLearnTable {
    mappings: [Option<MidiMappingTarget>; MIDI_MAPPING_SLOTS],
}

impl Default for MidiLearnTable {
    fn default() -> Self {
        #[allow(
            clippy::large_stack_arrays,
            reason = "fixed mapping table is preallocated once off the realtime path"
        )]
        Self {
            mappings: [None; MIDI_MAPPING_SLOTS],
        }
    }
}

impl MidiLearnTable {
    /// Creates a bounded immutable snapshot. Invalid channels/controllers are ignored.
    #[must_use]
    pub fn from_mappings(mappings: BTreeMap<(u8, u8), MidiMappingTarget>) -> Self {
        let mut table = Self::default();
        for ((channel, controller), target) in mappings {
            table = table.with_mapping(channel, controller, target);
        }
        table
    }
    /// Returns the target learned for a one-based channel and controller number.
    #[must_use]
    pub fn target_for(&self, channel: u8, controller: u8) -> Option<MidiMappingTarget> {
        mapping_index(channel, controller).and_then(|index| self.mappings[index])
    }
    /// Returns a changed immutable snapshot; the original remains valid for realtime readers.
    #[must_use]
    pub fn with_mapping(mut self, channel: u8, controller: u8, target: MidiMappingTarget) -> Self {
        if let Some(index) = mapping_index(channel, controller) {
            self.mappings[index] = Some(target);
        }
        self
    }
}

fn mapping_index(channel: u8, controller: u8) -> Option<usize> {
    if !(1..=16).contains(&channel) || controller > 127 {
        return None;
    }
    Some((usize::from(channel) - 1) * 128 + usize::from(controller))
}

/// Control-thread MIDI Learn state. Arm a target, feed it MIDI events, then publish its resulting
/// immutable table through [`MidiMappingPublisher`].
#[derive(Clone, Debug, Default)]
pub struct MidiLearnController {
    table: MidiLearnTable,
    armed: Option<MidiMappingTarget>,
}
impl MidiLearnController {
    /// Creates an empty, disarmed MIDI Learn controller.
    #[must_use]
    pub const fn new() -> Self {
        #[allow(
            clippy::large_stack_arrays,
            reason = "fixed mapping table is preallocated once off the realtime path"
        )]
        Self {
            table: MidiLearnTable {
                mappings: [None; MIDI_MAPPING_SLOTS],
            },
            armed: None,
        }
    }
    /// Starts disarmed with an existing persisted mapping snapshot.
    #[must_use]
    pub const fn from_table(table: MidiLearnTable) -> Self {
        Self { table, armed: None }
    }
    /// Arms MIDI Learn for `target`; the next CC captured by [`Self::observe`] creates its mapping.
    pub fn arm(&mut self, target: MidiMappingTarget) {
        self.armed = Some(target);
    }
    /// Cancels the pending MIDI Learn target without changing the published table.
    pub fn cancel(&mut self) {
        self.armed = None;
    }
    /// Reports whether MIDI Learn is awaiting a controller event.
    #[must_use]
    pub const fn is_armed(&self) -> bool {
        self.armed.is_some()
    }
    /// Learns the first CC received after arming and returns the new immutable snapshot.
    pub fn observe(&mut self, event: MidiEvent) -> Option<MidiLearnTable> {
        let target = self.armed?;
        if event.len != 3 || event.bytes[0] & 0xf0 != 0xb0 {
            return None;
        }
        let channel = (event.bytes[0] & 0x0f) + 1;
        self.table = self.table.with_mapping(channel, event.bytes[1], target);
        self.armed = None;
        Some(self.table)
    }
    /// Returns the current immutable mapping snapshot.
    #[must_use]
    pub const fn table(&self) -> MidiLearnTable {
        self.table
    }
}

/// SPSC immutable mapping handoff. The callback endpoint consumes snapshots by value, avoiding a
/// lock, allocation, or callback-thread reference-count retirement.
pub struct MidiMappingPublisher {
    producer: Producer<MidiLearnTable>,
}
/// Callback-side endpoint for receiving complete MIDI Learn mapping replacements.
pub struct MidiMappingReceiver {
    consumer: Consumer<MidiLearnTable>,
}
impl MidiMappingPublisher {
    /// Creates control and realtime endpoints.
    #[must_use]
    pub fn new() -> (Self, MidiMappingReceiver) {
        let (producer, consumer) = RingBuffer::new(2);
        (Self { producer }, MidiMappingReceiver { consumer })
    }
    /// Publishes a complete replacement snapshot without modifying a callback-visible table.
    ///
    /// # Errors
    /// Returns the table back when the single-slot ring is full.
    #[allow(
        clippy::result_large_err,
        reason = "returning the fixed table preserves the caller's snapshot without allocating"
    )]
    pub fn publish(&mut self, table: MidiLearnTable) -> Result<(), MidiLearnTable> {
        match self.producer.push(table) {
            Ok(()) => Ok(()),
            Err(rtrb::PushError::Full(table)) => Err(table),
        }
    }
}
impl MidiMappingReceiver {
    /// Applies the newest available immutable mapping at a block boundary.
    pub fn receive_latest(&mut self, active: &mut MidiLearnTable) {
        while let Ok(snapshot) = self.consumer.pop() {
            *active = snapshot;
        }
    }
}

/// Converts a MIDI controller value to a normalized parameter value in `0.0..=1.0`.
#[must_use]
pub fn map_controller_to_normalized(cc_value: u8) -> f32 {
    f32::from(cc_value) / 127.0
}

/// Maps one MIDI controller value through a learned target's persisted range.
#[must_use]
pub fn map_controller_to_target(cc_value: u8, target: MidiMappingTarget) -> f32 {
    target.minimum + (target.maximum - target.minimum) * map_controller_to_normalized(cc_value)
}

/// Converts a host-tick timestamp to a frame offset inside an audio block.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub fn sample_offset_for_timestamp(
    event_host_tick: u64,
    block_start_tick: u64,
    ticks_per_frame: f64,
    frames: u32,
) -> u32 {
    if frames == 0 || !ticks_per_frame.is_finite() || ticks_per_frame <= 0.0 {
        return 0;
    }
    let offset =
        (event_host_tick.saturating_sub(block_start_tick) as f64 / ticks_per_frame).floor() as u64;
    offset.min(u64::from(frames - 1)) as u32
}

/// Discovered MIDI input port. `id` is a reconnectable name-plus-occurrence identity because
/// `midir` does not expose `CoreMIDI` unique endpoint IDs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MidiPortInfo {
    /// Stable identity used to reopen this port after reconnecting.
    pub id: MidiPortId,
    /// User-visible port name reported by the system MIDI backend.
    pub name: String,
}

fn port_id(name: &str, occurrence: usize) -> MidiPortId {
    MidiPortId::new(format!("midir:{name}:{occurrence}"))
}

fn next_port_id(occurrences: &mut BTreeMap<String, usize>, name: &str) -> MidiPortId {
    let occurrence = occurrences.entry(name.to_owned()).or_default();
    let id = port_id(name, *occurrence);
    *occurrence += 1;
    id
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RealtimeMidiPacket {
    sequence: u64,
    timestamp_micros: u64,
    bytes: [u8; MIDI_MESSAGE_BYTES],
    len: u8,
}
impl RealtimeMidiPacket {
    fn from_message(sequence: u64, timestamp_micros: u64, message: &[u8]) -> Option<Self> {
        MidiEvent::from_bytes(0, message).ok().map(|event| Self {
            sequence,
            timestamp_micros,
            bytes: event.bytes,
            len: event.len,
        })
    }
    fn into_event(self) -> MidiEvent {
        MidiEvent {
            frame_offset: 0,
            timestamp_micros: self.timestamp_micros,
            bytes: self.bytes,
            len: self.len,
        }
    }
}
#[derive(Debug, Default)]
struct IngressTelemetry {
    received: AtomicU64,
    rejected: AtomicU64,
    latest_cc: AtomicU64,
}

impl IngressTelemetry {
    fn observe_cc(&self, message: &[u8]) {
        if message.len() != 3 || message[0] & 0xf0 != 0xb0 {
            return;
        }
        let generation = (self.latest_cc.load(Ordering::Relaxed) >> 32)
            .wrapping_add(1)
            .max(1);
        let packed = (generation << 32)
            | u64::from(message[0])
            | (u64::from(message[1]) << 8)
            | (u64::from(message[2]) << 16);
        self.latest_cc.store(packed, Ordering::Release);
    }
}

/// Main-thread view of the newest MIDI CC delivered by an open input.
pub struct MidiCcMonitor {
    telemetry: Arc<IngressTelemetry>,
    observed: u64,
}

impl MidiCcMonitor {
    /// Returns a newly observed CC once, coalescing faster controller traffic to the latest value.
    pub fn take_latest(&mut self) -> Option<MidiEvent> {
        let packed = self.telemetry.latest_cc.load(Ordering::Acquire);
        if packed == 0 || packed == self.observed {
            return None;
        }
        self.observed = packed;
        Some(MidiEvent {
            frame_offset: 0,
            timestamp_micros: 0,
            #[allow(
                clippy::cast_possible_truncation,
                reason = "deliberate extraction of the three packed status/data bytes"
            )]
            bytes: [packed as u8, (packed >> 8) as u8, (packed >> 16) as u8],
            len: 3,
        })
    }
}
fn ingress_queues() -> (
    Producer<RealtimeMidiPacket>,
    Consumer<RealtimeMidiPacket>,
    Producer<RealtimeMidiPacket>,
    Consumer<RealtimeMidiPacket>,
) {
    let (regular_producer, regular_consumer) = RingBuffer::new(MAX_MIDI_EVENTS_PER_BLOCK);
    let (safety_producer, safety_consumer) = RingBuffer::new(MAX_MIDI_EVENTS_PER_BLOCK);
    (
        regular_producer,
        regular_consumer,
        safety_producer,
        safety_consumer,
    )
}

/// `CoreMIDI` / system MIDI input backed by `midir`, feeding fixed callback-safe queues.
pub struct MidirInput {
    connection: Option<midir::MidiInputConnection<()>>,
    regular: Consumer<RealtimeMidiPacket>,
    safety: Consumer<RealtimeMidiPacket>,
    pending: SpscMidiQueue,
    telemetry: Arc<IngressTelemetry>,
    port: Option<MidiPortInfo>,
    selected_port: Option<MidiPortId>,
    opened_at: Instant,
}
impl MidirInput {
    /// Creates an unconnected MIDI input with preallocated ingress queues.
    #[must_use]
    pub fn new() -> Self {
        let (_, regular, _, safety) = ingress_queues();
        Self {
            connection: None,
            regular,
            safety,
            pending: SpscMidiQueue::new(),
            telemetry: Arc::new(IngressTelemetry::default()),
            port: None,
            selected_port: None,
            opened_at: Instant::now(),
        }
    }
    /// Lists ports with a selectable identity stable across ordinary reconnects.
    ///
    /// # Errors
    /// Returns an error when the system MIDI client cannot be created or queried.
    pub fn enumerate_ports() -> Result<Vec<MidiPortInfo>, Box<dyn std::error::Error + Send + Sync>>
    {
        let input = midir::MidiInput::new("superposition-enumerate")?;
        let mut occurrences = BTreeMap::<String, usize>::new();
        Ok(input
            .ports()
            .iter()
            .filter_map(|port| {
                let name = input.port_name(port).ok()?;
                let id = next_port_id(&mut occurrences, &name);
                Some(MidiPortInfo { id, name })
            })
            .collect())
    }
    /// Opens an explicitly selected port identity.
    ///
    /// # Errors
    /// Returns an error when the port is missing or the connection fails.
    pub fn open_port(
        &mut self,
        selected: &MidiPortId,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let input = midir::MidiInput::new("superposition")?;
        let mut occurrences = BTreeMap::<String, usize>::new();
        let selected_port = input.ports().into_iter().find_map(|port| {
            let name = input.port_name(&port).ok()?;
            let id = next_port_id(&mut occurrences, &name);
            (id == *selected).then_some((port, MidiPortInfo { id, name }))
        });
        let Some((port, info)) = selected_port else {
            self.connection = None;
            self.port = None;
            return Err(Box::new(MidiPortOpenError::NotFound(selected.clone())));
        };
        self.connect(input, &port, info)
    }
    /// Opens the first port and records its identity for later [`Self::reconnect`].
    ///
    /// # Errors
    /// Returns an error when no port exists or the connection fails.
    pub fn open_first_available(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let Some(port) = Self::enumerate_ports()?.into_iter().next() else {
            self.connection = None;
            self.port = None;
            return Ok(());
        };
        self.open_port(&port.id)
    }
    /// Reopens the selected port after disconnect or device reappearance.
    ///
    /// # Errors
    /// Returns an error when no port was selected or reopening fails.
    pub fn reconnect(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let selected = self
            .selected_port
            .clone()
            .ok_or(MidiPortOpenError::NoSelection)?;
        self.open_port(&selected)
    }
    fn connect(
        &mut self,
        input: midir::MidiInput,
        port: &midir::MidiInputPort,
        info: MidiPortInfo,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (mut regular_producer, regular, mut safety_producer, safety) = ingress_queues();
        let telemetry = Arc::new(IngressTelemetry::default());
        let callback_telemetry = Arc::clone(&telemetry);
        let mut next_packet_sequence = 0u64;
        let connection = input.connect(
            port,
            "superposition-input",
            move |timestamp_micros, message, ()| {
                let Some(packet) = RealtimeMidiPacket::from_message(
                    next_packet_sequence,
                    timestamp_micros,
                    message,
                ) else {
                    callback_telemetry.rejected.fetch_add(1, Ordering::Relaxed);
                    return;
                };
                next_packet_sequence = next_packet_sequence.wrapping_add(1);
                callback_telemetry.received.fetch_add(1, Ordering::Relaxed);
                callback_telemetry.observe_cc(message);
                let result = if packet.into_event().is_safety_event() {
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
        self.selected_port = Some(info.id.clone());
        self.port = Some(info);
        self.connection = Some(connection);
        self.regular = regular;
        self.safety = safety;
        self.pending = SpscMidiQueue::new();
        self.telemetry = telemetry;
        self.opened_at = Instant::now();
        Ok(())
    }
    /// Returns the currently open MIDI port, if the device is connected.
    #[must_use]
    pub fn connected_port(&self) -> Option<&MidiPortInfo> {
        self.port.as_ref()
    }
    /// Returns the selected identity retained for reconnect attempts.
    #[must_use]
    pub fn selected_port(&self) -> Option<&MidiPortId> {
        self.selected_port.as_ref()
    }
    /// Creates a coalescing main-thread CC observer for MIDI Learn.
    #[must_use]
    pub fn cc_monitor(&self) -> MidiCcMonitor {
        MidiCcMonitor {
            telemetry: Arc::clone(&self.telemetry),
            observed: 0,
        }
    }
    /// Moves pending ingress packets into a fixed queue in callback arrival order.
    pub fn pump_into(&mut self, queue: &mut SpscMidiQueue) {
        Self::pump_queues(&mut self.regular, &mut self.safety, queue);
    }
    fn pump_queues(
        regular: &mut Consumer<RealtimeMidiPacket>,
        safety: &mut Consumer<RealtimeMidiPacket>,
        queue: &mut SpscMidiQueue,
    ) {
        loop {
            let next = match (regular.peek(), safety.peek()) {
                (Ok(regular), Ok(safety)) => {
                    if regular.sequence < safety.sequence {
                        regular
                    } else {
                        safety
                    }
                }
                (Ok(regular), Err(_)) => regular,
                (Err(_), Ok(safety)) => safety,
                (Err(_), Err(_)) => break,
            };
            let event = next.into_event();
            if !queue.can_accept(event) {
                if let Ok(safety_packet) = safety.peek()
                    && queue.can_accept(safety_packet.into_event())
                {
                    let packet = safety.pop().expect("peeked safety packet is present");
                    queue
                        .try_push(packet.into_event())
                        .expect("queue has capacity");
                    continue;
                }
                break;
            }
            if event.is_safety_event() {
                let packet = safety.pop().expect("peeked safety packet is present");
                queue
                    .try_push(packet.into_event())
                    .expect("queue has capacity");
            } else {
                let packet = regular.pop().expect("peeked regular packet is present");
                queue
                    .try_push(packet.into_event())
                    .expect("queue has capacity");
            }
        }
    }
    /// Returns elapsed monotonic microseconds since the current port was opened.
    #[must_use]
    pub fn micros_since_open(&self) -> u64 {
        u64::try_from(self.opened_at.elapsed().as_micros()).unwrap_or(u64::MAX)
    }
    /// Returns whether the current connection has delivered at least one packet.
    #[must_use]
    pub fn has_received_traffic(&self) -> bool {
        self.telemetry.received.load(Ordering::Relaxed) > 0
    }
    /// Returns malformed or capacity-rejected ingress packet count.
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
    fn drain_into(
        &mut self,
        output: &mut BoundedMidiEvents,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Self::pump_queues(&mut self.regular, &mut self.safety, &mut self.pending);
        self.pending.drain_into(output);
        Ok(())
    }
}
/// Failure to select or reconnect a MIDI port.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MidiPortOpenError {
    /// Reconnect was requested before selecting any port.
    NoSelection,
    /// The selected port is not presently available.
    NotFound(MidiPortId),
}
impl fmt::Display for MidiPortOpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSelection => f.write_str("no MIDI port has been selected"),
            Self::NotFound(id) => write!(f, "MIDI port `{}` is unavailable", id.as_str()),
        }
    }
}
impl std::error::Error for MidiPortOpenError {}

/// Realtime parameter target prepared off the audio thread. It intentionally uses numeric
/// addresses, not `String` identifiers, so a block can be rendered without allocation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SceneParameterTarget {
    /// Zero-based rack index containing the target.
    pub rack_index: usize,
    /// Zero-based plug-in slot index containing the target.
    pub slot_index: usize,
    /// Engine-assigned numeric parameter address.
    pub parameter_id: u32,
    /// Normalized value to apply at this block position.
    pub value: f32,
}
const EMPTY_SCENE_TARGET: SceneParameterTarget = SceneParameterTarget {
    rack_index: 0,
    slot_index: 0,
    parameter_id: 0,
    value: 0.0,
};
/// Fixed output for one scene block.
#[derive(Clone, Debug)]
pub struct SceneRampBlock {
    targets: [SceneParameterTarget; MAX_SCENE_PARAMETERS],
    len: usize,
    /// Samples elapsed from the trigger point at this block's start.
    pub elapsed_samples: u64,
    /// Total ramp duration in samples.
    pub transition_samples: u64,
}
impl Default for SceneRampBlock {
    fn default() -> Self {
        Self::new()
    }
}
impl SceneRampBlock {
    /// Creates an empty reusable scene ramp output buffer.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            targets: [EMPTY_SCENE_TARGET; MAX_SCENE_PARAMETERS],
            len: 0,
            elapsed_samples: 0,
            transition_samples: 0,
        }
    }
    /// Clears targets while retaining fixed backing storage.
    pub fn clear(&mut self) {
        self.len = 0;
    }
    /// Returns the targets populated by the most recent render.
    #[must_use]
    pub fn targets(&self) -> &[SceneParameterTarget] {
        &self.targets[..self.len]
    }
}
#[derive(Clone, Debug)]
struct ActiveScene {
    start_sample: u64,
    transition_samples: u64,
    len: usize,
    starts: [f32; MAX_SCENE_PARAMETERS],
    ends: [SceneParameterTarget; MAX_SCENE_PARAMETERS],
    steps: [bool; MAX_SCENE_PARAMETERS],
}
/// Applies prepared parameter scenes with sample-clock determinism and automatic completion retirement.
#[derive(Clone, Debug, Default)]
pub struct ScenePlayer {
    active: Option<ActiveScene>,
}
impl ScenePlayer {
    /// Creates an idle scene player.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    /// Prepares a scene off the realtime path. The resolver must produce numeric engine addresses.
    pub fn trigger(
        &mut self,
        scene: &Scene,
        current: &[SceneParameterTarget],
        start_sample: u64,
        sample_rate: u32,
        resolve: impl Fn(&SceneParameterValue) -> Option<(usize, usize, u32)>,
    ) {
        let mut ends = [EMPTY_SCENE_TARGET; MAX_SCENE_PARAMETERS];
        let mut steps = [false; MAX_SCENE_PARAMETERS];
        let mut len = 0;
        for parameter in scene.parameter_values.iter().take(MAX_SCENE_PARAMETERS) {
            let Some((rack_index, slot_index, parameter_id)) = resolve(parameter) else {
                continue;
            };
            let end = SceneParameterTarget {
                rack_index,
                slot_index,
                parameter_id,
                value: parameter.value.get(),
            };
            ends[len] = end;
            steps[len] = parameter.transition == SceneParameterTransition::Step;
            len += 1;
        }
        self.trigger_targets_with_steps(
            &ends[..len],
            &steps[..len],
            current,
            scene.transition_ms,
            start_sample,
            sample_rate,
        );
    }

    /// Starts a prepared numeric scene without resolving model identifiers on the realtime path.
    pub fn trigger_targets(
        &mut self,
        targets: &[SceneParameterTarget],
        current: &[SceneParameterTarget],
        transition_ms: u32,
        start_sample: u64,
        sample_rate: u32,
    ) {
        self.trigger_targets_with_steps(
            targets,
            &[],
            current,
            transition_ms,
            start_sample,
            sample_rate,
        );
    }

    /// Starts a scene with per-target step markers. Missing current values never invent zero.
    pub fn trigger_targets_with_steps(
        &mut self,
        targets: &[SceneParameterTarget],
        steps: &[bool],
        current: &[SceneParameterTarget],
        transition_ms: u32,
        start_sample: u64,
        sample_rate: u32,
    ) {
        self.trigger_targets_with_lookup(
            targets,
            steps,
            transition_ms,
            start_sample,
            sample_rate,
            |end| {
                current
                    .iter()
                    .find(|value| scene_target_key(**value) == scene_target_key(end))
                    .map(|value| value.value)
                    .filter(|value| value.is_finite())
            },
        );
    }

    /// Starts a scene using a sorted current-value cache for bounded callback lookup.
    pub fn trigger_targets_sorted_with_steps(
        &mut self,
        targets: &[SceneParameterTarget],
        steps: &[bool],
        current: &[SceneParameterTarget],
        transition_ms: u32,
        start_sample: u64,
        sample_rate: u32,
    ) {
        self.trigger_targets_with_lookup(
            targets,
            steps,
            transition_ms,
            start_sample,
            sample_rate,
            |end| {
                current
                    .binary_search_by_key(&scene_target_key(end), |target| {
                        scene_target_key(*target)
                    })
                    .ok()
                    .map(|index| current[index].value)
                    .filter(|value| value.is_finite())
            },
        );
    }

    fn trigger_targets_with_lookup(
        &mut self,
        targets: &[SceneParameterTarget],
        steps: &[bool],
        transition_ms: u32,
        start_sample: u64,
        sample_rate: u32,
        lookup: impl Fn(SceneParameterTarget) -> Option<f32>,
    ) {
        let mut ends = [EMPTY_SCENE_TARGET; MAX_SCENE_PARAMETERS];
        let mut starts = [0.0; MAX_SCENE_PARAMETERS];
        let mut step_flags = [false; MAX_SCENE_PARAMETERS];
        let len = targets.len().min(MAX_SCENE_PARAMETERS);
        for (index, end) in targets.iter().copied().take(len).enumerate() {
            ends[index] = end;
            let known = lookup(end);
            starts[index] = known.unwrap_or(end.value);
            step_flags[index] = steps.get(index).copied().unwrap_or(false) || known.is_none();
        }
        let transition_samples = u64::from(transition_ms)
            .saturating_mul(u64::from(sample_rate))
            .div_ceil(1_000)
            .max(1);
        self.active = Some(ActiveScene {
            start_sample,
            transition_samples,
            len,
            starts,
            ends,
            steps: step_flags,
        });
    }
    /// Renders one block using absolute sample time. No allocation occurs, and completion retires
    /// the active scene after its final values have been emitted.
    pub fn render_block(
        &mut self,
        block_start_sample: u64,
        _frames: u32,
        output: &mut SceneRampBlock,
    ) {
        output.clear();
        let Some(active) = self.active.as_ref() else {
            return;
        };
        let elapsed = block_start_sample.saturating_sub(active.start_sample);
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            reason = "normalized ramp progress never needs more than f32 precision"
        )]
        let progress = (elapsed.min(active.transition_samples) as f64
            / active.transition_samples as f64) as f32;
        output.elapsed_samples = elapsed;
        output.transition_samples = active.transition_samples;
        for index in 0..active.len {
            let end = active.ends[index];
            if active.steps[index] && elapsed < active.transition_samples {
                continue;
            }
            output.targets[output.len] = SceneParameterTarget {
                value: active.starts[index] + (end.value - active.starts[index]) * progress,
                ..end
            };
            output.len += 1;
        }
        if elapsed >= active.transition_samples {
            self.active = None;
        }
    }
    /// Returns whether a scene ramp is currently active.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.active.is_some()
    }
}
const fn scene_target_key(target: SceneParameterTarget) -> (usize, usize, u32) {
    (target.rack_index, target.slot_index, target.parameter_id)
}
/// Program-change scene trigger: maps program number → scene index.
#[must_use]
pub fn scene_index_for_program(program: u8, scene_count: usize) -> Option<usize> {
    let index = usize::from(program);
    (index < scene_count).then_some(index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    #[test]
    fn rejects_non_realtime_packets() {
        assert_eq!(MidiEvent::new(0, vec![]), Err(MidiEventError::EmptyPacket));
        assert_eq!(
            MidiEvent::from_bytes(0, &[1, 2, 3, 4]),
            Err(MidiEventError::PacketTooLong)
        );
    }
    #[test]
    fn queue_protects_note_offs_and_coalesces_controls() {
        let mut queue = SpscMidiQueue::new();
        for value in 0..MAX_MIDI_EVENTS_PER_BLOCK {
            queue
                .try_push(
                    MidiEvent::from_bytes(0, &[0xb0, 74, u8::try_from(value).unwrap()]).unwrap(),
                )
                .unwrap();
        }
        queue
            .try_push(MidiEvent::from_bytes(0, &[0x80, 60, 0]).unwrap())
            .unwrap();
        let mut output: BoundedMidiEvents = BoundedMidiEvents::new();
        queue.drain_into(&mut output);
        assert_eq!(output.as_slice()[0].message(), &[0xb0, 74, 255]);
        assert!(output.as_slice()[1].is_note_off());
        assert_eq!(
            queue.coalesced_count(),
            (MAX_MIDI_EVENTS_PER_BLOCK - 1) as u64
        );
    }
    #[test]
    fn channel_mode_resets_survive_regular_overflow() {
        for controller in [120, 121, 123] {
            let mut queue = SpscMidiQueue::new();
            for _ in 0..MAX_MIDI_EVENTS_PER_BLOCK {
                queue
                    .try_push(MidiEvent::from_bytes(0, &[0x90, 60, 100]).unwrap())
                    .unwrap();
            }
            let reset = MidiEvent::from_bytes(0, &[0xb0, controller, 0]).unwrap();
            assert!(reset.is_safety_event());
            queue.try_push(reset).unwrap();
            let mut block: BoundedMidiEvents = BoundedMidiEvents::new();
            queue.drain_into(&mut block);
            assert_eq!(block.as_slice().last().unwrap().message(), reset.message());
            assert_eq!(queue.len(), 1);
        }
    }
    #[test]
    fn port_ids_count_each_name_independently() {
        let mut occurrences = BTreeMap::new();
        let ids =
            ["Keyboard", "Pad", "Keyboard", "Pad"].map(|name| next_port_id(&mut occurrences, name));
        assert_eq!(ids[0].as_str(), "midir:Keyboard:0");
        assert_eq!(ids[1].as_str(), "midir:Pad:0");
        assert_eq!(ids[2].as_str(), "midir:Keyboard:1");
        assert_eq!(ids[3].as_str(), "midir:Pad:1");
    }
    #[test]
    fn note_retriggers_keep_arrival_order_with_equal_timestamps() {
        let mut queue = SpscMidiQueue::new();
        for bytes in [&[0x90, 60, 100][..], &[0x80, 60, 0], &[0x90, 60, 80]] {
            let mut event = MidiEvent::from_bytes(0, bytes).unwrap();
            event.timestamp_micros = 42;
            queue.try_push(event).unwrap();
        }
        let mut output: BoundedMidiEvents = BoundedMidiEvents::new();
        queue.drain_into(&mut output);
        assert_eq!(
            output
                .as_slice()
                .iter()
                .map(MidiEvent::message)
                .collect::<Vec<_>>(),
            vec![&[0x90, 60, 100][..], &[0x80, 60, 0], &[0x90, 60, 80]]
        );
    }
    #[test]
    fn full_block_delivers_note_off_before_regular_backlog() {
        let mut queue = SpscMidiQueue::new();
        for _ in 0..MAX_MIDI_EVENTS_PER_BLOCK {
            queue
                .try_push(MidiEvent::from_bytes(0, &[0x90, 60, 100]).unwrap())
                .unwrap();
        }
        assert!(
            queue
                .try_push(MidiEvent::from_bytes(0, &[0x90, 61, 100]).unwrap())
                .is_err()
        );
        queue
            .try_push(MidiEvent::from_bytes(0, &[0x80, 60, 0]).unwrap())
            .unwrap();
        let mut first: BoundedMidiEvents = BoundedMidiEvents::new();
        queue.drain_into(&mut first);
        assert_eq!(first.len(), MAX_MIDI_EVENTS_PER_BLOCK);
        assert!(first.as_slice().last().unwrap().is_note_off());
        assert_eq!(queue.len(), 1);
        let mut second: BoundedMidiEvents = BoundedMidiEvents::new();
        queue.drain_into(&mut second);
        assert_eq!(second.as_slice()[0].message(), &[0x90, 60, 100]);
        assert!(queue.is_empty());
        assert_eq!(queue.rejected_count(), 1);
    }
    #[test]
    fn input_retains_ingress_beyond_one_block_and_merges_equal_timestamps() {
        let mut input = MidirInput::new();
        let (mut regular, regular_consumer, mut safety, safety_consumer) = ingress_queues();
        input.regular = regular_consumer;
        input.safety = safety_consumer;
        for sequence in 0..MAX_MIDI_EVENTS_PER_BLOCK {
            regular
                .push(
                    RealtimeMidiPacket::from_message(sequence as u64, 10, &[0x90, 60, 100])
                        .unwrap(),
                )
                .unwrap();
        }
        safety
            .push(
                RealtimeMidiPacket::from_message(
                    MAX_MIDI_EVENTS_PER_BLOCK as u64,
                    10,
                    &[0x80, 60, 0],
                )
                .unwrap(),
            )
            .unwrap();
        let mut first = BoundedMidiEvents::new();
        input.drain_into(&mut first).unwrap();
        assert_eq!(first.len(), MAX_MIDI_EVENTS_PER_BLOCK);
        assert!(first.as_slice().last().unwrap().is_note_off());
        let mut second = BoundedMidiEvents::new();
        input.drain_into(&mut second).unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second.as_slice()[0].message(), &[0x90, 60, 100]);
        assert_eq!(input.rejected_count(), 0);
    }
    #[test]
    fn input_preserves_interleaved_note_order_with_equal_timestamps() {
        let mut input = MidirInput::new();
        let (mut regular, regular_consumer, mut safety, safety_consumer) = ingress_queues();
        input.regular = regular_consumer;
        input.safety = safety_consumer;
        regular
            .push(RealtimeMidiPacket::from_message(0, 10, &[0x90, 60, 100]).unwrap())
            .unwrap();
        safety
            .push(RealtimeMidiPacket::from_message(1, 10, &[0x80, 60, 0]).unwrap())
            .unwrap();
        regular
            .push(RealtimeMidiPacket::from_message(2, 10, &[0x90, 60, 80]).unwrap())
            .unwrap();
        let mut output: BoundedMidiEvents = BoundedMidiEvents::new();
        input.drain_into(&mut output).unwrap();
        assert_eq!(
            output
                .as_slice()
                .iter()
                .map(MidiEvent::message)
                .collect::<Vec<_>>(),
            vec![&[0x90, 60, 100][..], &[0x80, 60, 0], &[0x90, 60, 80]]
        );
    }
    #[test]
    fn mapping_snapshots_publish_whole_replacements() {
        let target = MidiMappingTarget {
            rack_index: 1,
            slot_index: 2,
            parameter_id: 3,
            minimum: 0.25,
            maximum: 0.75,
        };
        let mut mappings = BTreeMap::new();
        mappings.insert((2, 74), target);
        let table = MidiLearnTable::from_mappings(mappings);
        let (mut publisher, mut receiver) = MidiMappingPublisher::new();
        publisher.publish(table).unwrap();
        let mut active = MidiLearnTable::default();
        receiver.receive_latest(&mut active);
        assert_eq!(active.target_for(2, 74), Some(target));
        assert!((map_controller_to_target(127, target) - 0.75).abs() < f32::EPSILON);
    }
    #[test]
    fn learn_arms_and_captures_cc() {
        let target = MidiMappingTarget {
            rack_index: 0,
            slot_index: 1,
            parameter_id: 7,
            minimum: 0.0,
            maximum: 1.0,
        };
        let mut learn = MidiLearnController::new();
        learn.arm(target);
        assert_eq!(
            learn.observe(MidiEvent::from_bytes(0, &[0x90, 60, 100]).unwrap()),
            None
        );
        assert_eq!(
            learn
                .observe(MidiEvent::from_bytes(0, &[0xb0, 19, 80]).unwrap())
                .unwrap()
                .target_for(1, 19),
            Some(target)
        );
    }
    #[test]
    fn controller_values_and_timestamps_are_normalized_and_clamped() {
        assert!((map_controller_to_normalized(127) - 1.0).abs() <= f32::EPSILON);
        assert_eq!(sample_offset_for_timestamp(104, 100, 2.0, 8), 2);
        assert_eq!(sample_offset_for_timestamp(200, 100, 2.0, 8), 7);
    }
    #[test]
    fn scene_is_sample_deterministic_and_retires() {
        let mut player = ScenePlayer::new();
        let scene = Scene {
            id: sp_model::SceneId("scene".into()),
            name: "scene".into(),
            gains: vec![],
            mutes: vec![],
            rack_bypasses: vec![],
            bypasses: vec![],
            parameter_values: vec![SceneParameterValue {
                rack_id: sp_model::RackId("rack".into()),
                slot_id: sp_model::PluginInstanceId("slot".into()),
                parameter_id: sp_model::ParameterId("parameter".into()),
                value: sp_model::NormalizedValue::new(1.0).unwrap(),
                transition: SceneParameterTransition::Ramp,
            }],
            transition_ms: 10,
        };
        player.trigger(
            &scene,
            &[SceneParameterTarget {
                rack_index: 0,
                slot_index: 0,
                parameter_id: 1,
                value: 0.0,
            }],
            100,
            1_000,
            |_| Some((0, 0, 1)),
        );
        let mut output = SceneRampBlock::new();
        player.render_block(105, 1, &mut output);
        assert!((output.targets()[0].value - 0.5).abs() < f32::EPSILON);
        player.render_block(110, 1, &mut output);
        assert!((output.targets()[0].value - 1.0).abs() < f32::EPSILON);
        assert!(!player.is_active());
    }

    #[test]
    fn step_parameter_waits_for_commit_and_unknown_start_never_ramps_from_zero() {
        let ramp = SceneParameterTarget {
            rack_index: 0,
            slot_index: 0,
            parameter_id: 1,
            value: 0.8,
        };
        let step = SceneParameterTarget {
            parameter_id: 2,
            value: 1.0,
            ..ramp
        };
        let mut player = ScenePlayer::new();
        player.trigger_targets_sorted_with_steps(
            &[ramp, step],
            &[false, true],
            &[SceneParameterTarget { value: 0.2, ..ramp }],
            10,
            0,
            1_000,
        );
        let mut block = SceneRampBlock::new();
        player.render_block(5, 1, &mut block);
        assert_eq!(block.targets().len(), 1);
        assert_eq!(block.targets()[0].parameter_id, 1);
        assert!((block.targets()[0].value - 0.5).abs() < f32::EPSILON);
        player.render_block(10, 1, &mut block);
        assert_eq!(block.targets().len(), 2);
        assert_eq!(block.targets()[1].value.to_bits(), 1.0_f32.to_bits());
    }

    #[test]
    fn model_scene_trigger_honors_step_transition() {
        let scene = Scene {
            id: sp_model::SceneId("scene".into()),
            name: "scene".into(),
            gains: vec![],
            mutes: vec![],
            rack_bypasses: vec![],
            bypasses: vec![],
            parameter_values: vec![SceneParameterValue {
                rack_id: sp_model::RackId("rack".into()),
                slot_id: sp_model::PluginInstanceId("slot".into()),
                parameter_id: sp_model::ParameterId("parameter".into()),
                value: sp_model::NormalizedValue::new(1.0).unwrap(),
                transition: SceneParameterTransition::Step,
            }],
            transition_ms: 10,
        };
        let mut player = ScenePlayer::new();
        player.trigger(
            &scene,
            &[SceneParameterTarget {
                rack_index: 0,
                slot_index: 0,
                parameter_id: 1,
                value: 0.2,
            }],
            0,
            1_000,
            |_| Some((0, 0, 1)),
        );
        let mut block = SceneRampBlock::new();
        player.render_block(5, 1, &mut block);
        assert!(block.targets().is_empty());
        player.render_block(10, 1, &mut block);
        assert_eq!(block.targets()[0].value.to_bits(), 1.0_f32.to_bits());
    }
}
