#![allow(
    missing_docs,
    clippy::comparison_chain,
    clippy::missing_errors_doc,
    clippy::needless_pass_by_value
)]
//! Explicitly bounded, versioned binary control messages for isolated rack workers.
//!
//! This wire format is not shared memory and is never used by an audio callback. Every message is
//! a four-byte little-endian body length followed by a fixed header. Headers are decoded and
//! validated before allocating payload bytes. [`ControlRequestGate`] additionally rejects stale
//! rack/bank generations and replayed IDs before it reads a request payload for dispatch. `SetSlotBypass` and `SetRackBypass` each carry exactly one byte: `0` clears bypass and `1` enables it. Slot bypass requires a slot identity; rack bypass forbids one.

use std::{
    fmt,
    io::{self, Read, Write},
};

pub const CONTROL_PROTOCOL_MAGIC: u32 = u32::from_le_bytes(*b"SPC1");
pub const CONTROL_PROTOCOL_VERSION: u16 = 2;
pub const CONTROL_REQUEST_HEADER_BYTES: usize = 48;
pub const CONTROL_RESPONSE_HEADER_BYTES: usize = 52;
pub const MAX_CONTROL_FRAME_BYTES: usize = 1_048_576;
pub const MAX_CONTROL_PAYLOAD_BYTES: usize = 1_040_000;
pub const MAX_CONTROL_STATE_BYTES: usize = MAX_CONTROL_PAYLOAD_BYTES;
pub const MAX_CONTROL_ERROR_RECORD_BYTES: usize = 512;
pub const MAX_CONTROL_ERROR_MESSAGE_BYTES: usize = MAX_CONTROL_ERROR_RECORD_BYTES - 4;
pub const CONTROL_BANK_COUNT: u8 = 2;
pub const CONTROL_RACK_COUNT: u8 = 8;
pub const MAX_PLUGIN_REFERENCE_BYTES: usize = 4_096;
pub const MAX_RACK_REBUILD_BYTES: usize = 32_768;
/// Maximum control requests the worker may queue while preserving bounded memory.
pub const MAX_PENDING_CONTROL_REQUESTS: usize = 32;

const PREFIX_BYTES: usize = 4;
const ERROR_HEADER_BYTES: usize = 4;
const NO_SLOT: u64 = 0;

/// Worker control operations. The helper-side VST3 adapter interprets the bounded opaque payload.
#[repr(u16)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlOperation {
    PreloadPlugin = 1,
    LoadPlugin = 2,
    ActivateSlot = 3,
    DeactivateSlot = 4,
    UnloadSlot = 5,
    RebuildRack = 6,
    ReorderRack = 7,
    ParameterMetadata = 8,
    ReadParameter = 9,
    WriteParameter = 10,
    BeginParameterGesture = 11,
    EndParameterGesture = 12,
    CaptureState = 13,
    RestoreState = 14,
    NotifyLatency = 15,
    NotifyRestart = 16,
    OpenNativeEditor = 17,
    CloseNativeEditor = 18,
    FocusNativeEditor = 19,
    ResizeNativeEditor = 20,
    Shutdown = 21,
    QueryHealth = 22,
    QuerySlotAttribution = 23,
    SetSlotBypass = 24,
    SetRackBypass = 25,
}

impl ControlOperation {
    pub const ALL: [Self; 25] = [
        Self::PreloadPlugin,
        Self::LoadPlugin,
        Self::ActivateSlot,
        Self::DeactivateSlot,
        Self::UnloadSlot,
        Self::RebuildRack,
        Self::ReorderRack,
        Self::ParameterMetadata,
        Self::ReadParameter,
        Self::WriteParameter,
        Self::BeginParameterGesture,
        Self::EndParameterGesture,
        Self::CaptureState,
        Self::RestoreState,
        Self::NotifyLatency,
        Self::NotifyRestart,
        Self::OpenNativeEditor,
        Self::CloseNativeEditor,
        Self::FocusNativeEditor,
        Self::ResizeNativeEditor,
        Self::Shutdown,
        Self::QueryHealth,
        Self::QuerySlotAttribution,
        Self::SetSlotBypass,
        Self::SetRackBypass,
    ];

    pub const fn from_wire(value: u16) -> Result<Self, ControlProtocolError> {
        match value {
            1 => Ok(Self::PreloadPlugin),
            2 => Ok(Self::LoadPlugin),
            3 => Ok(Self::ActivateSlot),
            4 => Ok(Self::DeactivateSlot),
            5 => Ok(Self::UnloadSlot),
            6 => Ok(Self::RebuildRack),
            7 => Ok(Self::ReorderRack),
            8 => Ok(Self::ParameterMetadata),
            9 => Ok(Self::ReadParameter),
            10 => Ok(Self::WriteParameter),
            11 => Ok(Self::BeginParameterGesture),
            12 => Ok(Self::EndParameterGesture),
            13 => Ok(Self::CaptureState),
            14 => Ok(Self::RestoreState),
            15 => Ok(Self::NotifyLatency),
            16 => Ok(Self::NotifyRestart),
            17 => Ok(Self::OpenNativeEditor),
            18 => Ok(Self::CloseNativeEditor),
            19 => Ok(Self::FocusNativeEditor),
            20 => Ok(Self::ResizeNativeEditor),
            21 => Ok(Self::Shutdown),
            22 => Ok(Self::QueryHealth),
            23 => Ok(Self::QuerySlotAttribution),
            24 => Ok(Self::SetSlotBypass),
            25 => Ok(Self::SetRackBypass),
            _ => Err(ControlProtocolError::UnknownOperation(value)),
        }
    }

    #[must_use]
    pub const fn wire_code(self) -> u16 {
        self as u16
    }

    #[must_use]
    pub const fn slot_rule(self) -> SlotRule {
        match self {
            Self::RebuildRack
            | Self::ReorderRack
            | Self::Shutdown
            | Self::QueryHealth
            | Self::QuerySlotAttribution
            | Self::SetRackBypass => SlotRule::Forbidden,
            Self::NotifyRestart => SlotRule::Optional,
            _ => SlotRule::Required,
        }
    }

    /// Inclusive lower/upper payload bytes for this operation.
    #[must_use]
    pub const fn payload_bounds(self) -> (usize, usize) {
        match self {
            Self::PreloadPlugin | Self::LoadPlugin => (1, MAX_PLUGIN_REFERENCE_BYTES),
            Self::RebuildRack => (1, MAX_RACK_REBUILD_BYTES),
            // count plus complete current and requested sparse membership lists.
            Self::ReorderRack => (3, 17),
            Self::ParameterMetadata
            | Self::ReadParameter
            | Self::BeginParameterGesture
            | Self::EndParameterGesture
            | Self::NotifyLatency
            | Self::ResizeNativeEditor => (8, 8),
            Self::WriteParameter => (16, 16),
            Self::CaptureState | Self::OpenNativeEditor => (0, 16),
            Self::RestoreState => (1, MAX_CONTROL_STATE_BYTES),
            Self::NotifyRestart => (4, 16),
            Self::SetSlotBypass | Self::SetRackBypass => (1, 1),
            Self::ActivateSlot
            | Self::DeactivateSlot
            | Self::UnloadSlot
            | Self::CloseNativeEditor
            | Self::FocusNativeEditor
            | Self::Shutdown
            | Self::QueryHealth
            | Self::QuerySlotAttribution => (0, 0),
        }
    }

    fn validate_payload(self, length: usize) -> Result<(), ControlProtocolError> {
        let (minimum, maximum) = self.payload_bounds();
        if length < minimum {
            return Err(ControlProtocolError::PayloadTooShort {
                operation: self,
                minimum,
                received: length,
            });
        }
        if length > maximum {
            return Err(ControlProtocolError::PayloadTooLarge {
                operation: self,
                maximum,
                received: length,
            });
        }
        Ok(())
    }

    fn validate_payload_bytes(self, payload: &[u8]) -> Result<(), ControlProtocolError> {
        self.validate_payload(payload.len())?;
        if matches!(self, Self::SetSlotBypass | Self::SetRackBypass) && !matches!(payload, [0 | 1])
        {
            return Err(ControlProtocolError::InvalidPayload);
        }
        if self == Self::ReorderRack {
            validate_reorder_payload(payload)?;
        }
        if matches!(
            self,
            Self::ParameterMetadata
                | Self::ReadParameter
                | Self::BeginParameterGesture
                | Self::EndParameterGesture
        ) {
            crate::payload::ParameterId::decode(payload)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SlotRule {
    Required,
    Forbidden,
    Optional,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ControlRequestId(u64);

impl ControlRequestId {
    pub const fn new(value: u64) -> Result<Self, ControlProtocolError> {
        if value == 0 {
            Err(ControlProtocolError::InvalidRequestId)
        } else {
            Ok(Self(value))
        }
    }
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RackIdentity {
    index: u8,
    generation: u64,
}

impl RackIdentity {
    pub const fn new(index: u8, generation: u64) -> Result<Self, ControlProtocolError> {
        if index >= CONTROL_RACK_COUNT {
            Err(ControlProtocolError::InvalidRackIndex(index))
        } else if generation == 0 {
            Err(ControlProtocolError::InvalidRackGeneration)
        } else {
            Ok(Self { index, generation })
        }
    }
    #[must_use]
    pub const fn index(self) -> u8 {
        self.index
    }
    #[must_use]
    pub const fn generation(self) -> u64 {
        self.generation
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BankIdentity {
    index: u8,
    generation: u64,
}

impl BankIdentity {
    pub const fn new(index: u8, generation: u64) -> Result<Self, ControlProtocolError> {
        if index >= CONTROL_BANK_COUNT {
            Err(ControlProtocolError::InvalidBankIndex(index))
        } else if generation == 0 {
            Err(ControlProtocolError::InvalidBankGeneration)
        } else {
            Ok(Self { index, generation })
        }
    }
    #[must_use]
    pub const fn index(self) -> u8 {
        self.index
    }
    #[must_use]
    pub const fn generation(self) -> u64 {
        self.generation
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ControlTarget {
    rack: RackIdentity,
    bank: BankIdentity,
}

impl ControlTarget {
    #[must_use]
    pub const fn new(rack: RackIdentity, bank: BankIdentity) -> Self {
        Self { rack, bank }
    }
    #[must_use]
    pub const fn rack(self) -> RackIdentity {
        self.rack
    }
    #[must_use]
    pub const fn bank(self) -> BankIdentity {
        self.bank
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SlotIdentity(u64);

impl SlotIdentity {
    pub const fn new(value: u64) -> Result<Self, ControlProtocolError> {
        if value == NO_SLOT {
            Err(ControlProtocolError::InvalidSlotIdentity)
        } else {
            Ok(Self(value))
        }
    }
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A full request ready for non-realtime worker-control dispatch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlRequest {
    id: ControlRequestId,
    target: ControlTarget,
    operation: ControlOperation,
    slot: Option<SlotIdentity>,
    payload: Vec<u8>,
}

impl ControlRequest {
    pub fn new(
        id: ControlRequestId,
        target: ControlTarget,
        operation: ControlOperation,
        slot: Option<SlotIdentity>,
        payload: &[u8],
    ) -> Result<Self, ControlProtocolError> {
        validate_slot(operation, slot)?;
        operation.validate_payload_bytes(payload)?;
        Ok(Self {
            id,
            target,
            operation,
            slot,
            payload: payload.to_vec(),
        })
    }
    #[must_use]
    pub const fn request_id(&self) -> ControlRequestId {
        self.id
    }
    #[must_use]
    pub const fn target(&self) -> ControlTarget {
        self.target
    }
    #[must_use]
    pub const fn operation(&self) -> ControlOperation {
        self.operation
    }
    #[must_use]
    pub const fn slot(&self) -> Option<SlotIdentity> {
        self.slot
    }
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    pub fn write_to(&self, writer: &mut impl Write) -> Result<(), ControlProtocolError> {
        write_request(writer, RequestHeader::from_request(self)?, &self.payload)
    }
    pub fn read_from(reader: &mut impl Read) -> Result<Self, ControlProtocolError> {
        let (header, length) = read_request_header(reader)?;
        header.into_request(read_payload(reader, length)?)
    }
}

#[repr(u16)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlResponseStatus {
    Ok = 0,
    Rejected = 1,
    Failed = 2,
    StaleGeneration = 3,
    DuplicateRequest = 4,
    Unsupported = 5,
    ShuttingDown = 6,
}

impl ControlResponseStatus {
    pub const fn from_wire(value: u16) -> Result<Self, ControlProtocolError> {
        match value {
            0 => Ok(Self::Ok),
            1 => Ok(Self::Rejected),
            2 => Ok(Self::Failed),
            3 => Ok(Self::StaleGeneration),
            4 => Ok(Self::DuplicateRequest),
            5 => Ok(Self::Unsupported),
            6 => Ok(Self::ShuttingDown),
            _ => Err(ControlProtocolError::UnknownResponseStatus(value)),
        }
    }
    #[must_use]
    pub const fn wire_code(self) -> u16 {
        self as u16
    }
    const fn success(self) -> bool {
        matches!(self, Self::Ok | Self::ShuttingDown)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ControlErrorCode(u16);

impl ControlErrorCode {
    pub const INVALID_REQUEST: Self = Self(1);
    pub const UNSUPPORTED: Self = Self(2);
    pub const STALE_GENERATION: Self = Self(3);
    pub const DUPLICATE_REQUEST: Self = Self(4);
    pub const INTERNAL: Self = Self(5);
    pub const UNAVAILABLE: Self = Self(6);
    pub const OPERATION_FAILED: Self = Self(7);
    #[must_use]
    pub const fn new(value: u16) -> Self {
        Self(value)
    }
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlErrorRecord {
    code: ControlErrorCode,
    message: String,
}

impl ControlErrorRecord {
    pub fn new(code: ControlErrorCode, message: &str) -> Result<Self, ControlProtocolError> {
        if message.len() > MAX_CONTROL_ERROR_MESSAGE_BYTES {
            return Err(ControlProtocolError::ErrorRecordTooLarge {
                maximum: MAX_CONTROL_ERROR_MESSAGE_BYTES,
                received: message.len(),
            });
        }
        Ok(Self {
            code,
            message: message.to_owned(),
        })
    }
    #[must_use]
    pub const fn code(&self) -> ControlErrorCode {
        self.code
    }
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    fn encode(&self) -> Result<Vec<u8>, ControlProtocolError> {
        let total = ERROR_HEADER_BYTES + self.message.len();
        if total > MAX_CONTROL_ERROR_RECORD_BYTES {
            return Err(ControlProtocolError::ErrorRecordTooLarge {
                maximum: MAX_CONTROL_ERROR_RECORD_BYTES,
                received: total,
            });
        }
        let length = u16::try_from(self.message.len()).map_err(|_| {
            ControlProtocolError::ErrorRecordTooLarge {
                maximum: MAX_CONTROL_ERROR_MESSAGE_BYTES,
                received: self.message.len(),
            }
        })?;
        let mut bytes = Vec::with_capacity(total);
        bytes.extend_from_slice(&self.code.get().to_le_bytes());
        bytes.extend_from_slice(&length.to_le_bytes());
        bytes.extend_from_slice(self.message.as_bytes());
        Ok(bytes)
    }
    fn decode(bytes: &[u8]) -> Result<Self, ControlProtocolError> {
        if bytes.len() < ERROR_HEADER_BYTES || bytes.len() > MAX_CONTROL_ERROR_RECORD_BYTES {
            return Err(ControlProtocolError::MalformedErrorRecord);
        }
        let length = usize::from(u16_at(bytes, 2));
        if length > MAX_CONTROL_ERROR_MESSAGE_BYTES || ERROR_HEADER_BYTES + length != bytes.len() {
            return Err(ControlProtocolError::MalformedErrorRecord);
        }
        let message = std::str::from_utf8(&bytes[ERROR_HEADER_BYTES..])
            .map_err(|_| ControlProtocolError::MalformedErrorRecord)?;
        Self::new(ControlErrorCode::new(u16_at(bytes, 0)), message)
    }
}

/// A response containing a bounded result or bounded error record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlResponse {
    id: ControlRequestId,
    target: ControlTarget,
    slot: Option<SlotIdentity>,
    status: ControlResponseStatus,
    payload: Vec<u8>,
    error: Option<ControlErrorRecord>,
}

impl ControlResponse {
    pub fn success(
        id: ControlRequestId,
        target: ControlTarget,
        slot: Option<SlotIdentity>,
        payload: &[u8],
    ) -> Result<Self, ControlProtocolError> {
        Self::new(id, target, slot, ControlResponseStatus::Ok, payload, None)
    }
    pub fn error(
        id: ControlRequestId,
        target: ControlTarget,
        slot: Option<SlotIdentity>,
        status: ControlResponseStatus,
        error: ControlErrorRecord,
    ) -> Result<Self, ControlProtocolError> {
        Self::new(id, target, slot, status, &[], Some(error))
    }
    /// Builds the orderly shutdown acknowledgement. `ShuttingDown` is a success status, so it
    /// carries no error record; it tells the peer the worker accepted the close and will not
    /// serve further requests.
    pub fn shutting_down(
        id: ControlRequestId,
        target: ControlTarget,
        slot: Option<SlotIdentity>,
    ) -> Result<Self, ControlProtocolError> {
        Self::new(
            id,
            target,
            slot,
            ControlResponseStatus::ShuttingDown,
            &[],
            None,
        )
    }
    fn new(
        id: ControlRequestId,
        target: ControlTarget,
        slot: Option<SlotIdentity>,
        status: ControlResponseStatus,
        payload: &[u8],
        error: Option<ControlErrorRecord>,
    ) -> Result<Self, ControlProtocolError> {
        if payload.len() > MAX_CONTROL_PAYLOAD_BYTES {
            return Err(ControlProtocolError::ResponsePayloadTooLarge {
                maximum: MAX_CONTROL_PAYLOAD_BYTES,
                received: payload.len(),
            });
        }
        match (status.success(), error.as_ref()) {
            (true, None) | (false, Some(_)) => {}
            (true, Some(_)) => return Err(ControlProtocolError::SuccessResponseHasError),
            (false, None) => return Err(ControlProtocolError::ErrorResponseMissingRecord),
        }
        if let Some(record) = &error {
            let _ = record.encode()?;
        }
        Ok(Self {
            id,
            target,
            slot,
            status,
            payload: payload.to_vec(),
            error,
        })
    }
    #[must_use]
    pub const fn request_id(&self) -> ControlRequestId {
        self.id
    }
    #[must_use]
    pub const fn target(&self) -> ControlTarget {
        self.target
    }
    #[must_use]
    pub const fn slot(&self) -> Option<SlotIdentity> {
        self.slot
    }
    #[must_use]
    pub const fn status(&self) -> ControlResponseStatus {
        self.status
    }
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }
    #[must_use]
    pub const fn error_record(&self) -> Option<&ControlErrorRecord> {
        self.error.as_ref()
    }

    pub fn validate_for(&self, request: &ControlRequest) -> Result<(), ControlProtocolError> {
        let slot_matches = request.operation == ControlOperation::QuerySlotAttribution
            || self.slot == request.slot;
        if self.id != request.id || self.target != request.target || !slot_matches {
            Err(ControlProtocolError::CorrelationMismatch)
        } else {
            Ok(())
        }
    }
    pub fn write_to(&self, writer: &mut impl Write) -> Result<(), ControlProtocolError> {
        let error = self
            .error
            .as_ref()
            .map(ControlErrorRecord::encode)
            .transpose()?
            .unwrap_or_default();
        write_response(
            writer,
            ResponseHeader::from_response(self, error.len())?,
            &self.payload,
            &error,
        )
    }
    pub fn read_from(reader: &mut impl Read) -> Result<Self, ControlProtocolError> {
        let (header, payload_length, error_length) = read_response_header(reader)?;
        header.into_response(
            read_payload(reader, payload_length)?,
            read_error(reader, error_length)?,
        )
    }
}

/// A bounded, allocation-free replay and generation gate for one worker-control connection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlRequestGate {
    target: ControlTarget,
    last_id: Option<ControlRequestId>,
}

impl ControlRequestGate {
    #[must_use]
    pub const fn new(target: ControlTarget) -> Self {
        Self {
            target,
            last_id: None,
        }
    }
    #[must_use]
    pub const fn expected_target(&self) -> ControlTarget {
        self.target
    }
    pub fn set_expected_target(&mut self, target: ControlTarget) {
        self.target = target;
    }

    /// Validates fixed header identities before allocating its opaque payload.
    pub fn read_from(
        &mut self,
        reader: &mut impl Read,
    ) -> Result<ControlRequest, ControlProtocolError> {
        let (header, length) = read_request_header(reader)?;
        self.validate(&header)?;
        let request = header.into_request(read_payload(reader, length)?)?;
        self.last_id = Some(request.id);
        Ok(request)
    }
    pub fn admit(&mut self, request: &ControlRequest) -> Result<(), ControlProtocolError> {
        self.validate_target(request.target)?;
        self.validate_id(request.id)?;
        self.last_id = Some(request.id);
        Ok(())
    }
    fn validate(&self, header: &RequestHeader) -> Result<(), ControlProtocolError> {
        self.validate_target(header.target)?;
        self.validate_id(header.id)
    }
    fn validate_target(&self, target: ControlTarget) -> Result<(), ControlProtocolError> {
        if target.rack.index != self.target.rack.index {
            return Err(ControlProtocolError::StaleRackIndex {
                expected: self.target.rack.index,
                received: target.rack.index,
            });
        }
        if target.rack.generation != self.target.rack.generation {
            return Err(ControlProtocolError::StaleRackGeneration {
                expected: self.target.rack.generation,
                received: target.rack.generation,
            });
        }
        if target.bank.index != self.target.bank.index {
            return Err(ControlProtocolError::StaleBankIndex {
                expected: self.target.bank.index,
                received: target.bank.index,
            });
        }
        if target.bank.generation != self.target.bank.generation {
            return Err(ControlProtocolError::StaleBankGeneration {
                expected: self.target.bank.generation,
                received: target.bank.generation,
            });
        }
        Ok(())
    }
    fn validate_id(&self, id: ControlRequestId) -> Result<(), ControlProtocolError> {
        let Some(previous) = self.last_id else {
            return Ok(());
        };
        if id == previous {
            Err(ControlProtocolError::DuplicateRequestId(id.get()))
        } else if id < previous {
            Err(ControlProtocolError::NonMonotonicRequestId {
                previous: previous.get(),
                received: id.get(),
            })
        } else {
            Ok(())
        }
    }
}

#[derive(Debug)]
pub enum ControlProtocolError {
    Io(io::Error),
    FrameTooShort {
        received: usize,
        minimum: usize,
    },
    FrameTooLarge {
        maximum: usize,
        received: usize,
    },
    FrameLengthMismatch {
        declared: usize,
        expected: usize,
    },
    UnknownMagic(u32),
    UnsupportedVersion(u16),
    UnknownOperation(u16),
    UnknownResponseStatus(u16),
    InvalidRequestId,
    InvalidRackIndex(u8),
    InvalidRackGeneration,
    InvalidBankIndex(u8),
    InvalidBankGeneration,
    InvalidSlotIdentity,
    NonZeroReserved,
    MissingSlotIdentity(ControlOperation),
    UnexpectedSlotIdentity(ControlOperation),
    PayloadTooShort {
        operation: ControlOperation,
        minimum: usize,
        received: usize,
    },
    PayloadTooLarge {
        operation: ControlOperation,
        maximum: usize,
        received: usize,
    },
    ResponsePayloadTooLarge {
        maximum: usize,
        received: usize,
    },
    ErrorRecordTooLarge {
        maximum: usize,
        received: usize,
    },
    MalformedErrorRecord,
    InvalidPayload,
    SuccessResponseHasError,
    ErrorResponseMissingRecord,
    StaleRackIndex {
        expected: u8,
        received: u8,
    },
    StaleRackGeneration {
        expected: u64,
        received: u64,
    },
    StaleBankIndex {
        expected: u8,
        received: u8,
    },
    StaleBankGeneration {
        expected: u64,
        received: u64,
    },
    DuplicateRequestId(u64),
    NonMonotonicRequestId {
        previous: u64,
        received: u64,
    },
    CorrelationMismatch,
}

impl fmt::Display for ControlProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for ControlProtocolError {}
impl From<io::Error> for ControlProtocolError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RequestHeader {
    id: ControlRequestId,
    target: ControlTarget,
    operation: ControlOperation,
    slot: Option<SlotIdentity>,
    payload_length: usize,
}

impl RequestHeader {
    fn from_request(request: &ControlRequest) -> Result<Self, ControlProtocolError> {
        let header = Self {
            id: request.id,
            target: request.target,
            operation: request.operation,
            slot: request.slot,
            payload_length: request.payload.len(),
        };
        header.validate()?;
        Ok(header)
    }
    fn decode(bytes: &[u8; CONTROL_REQUEST_HEADER_BYTES]) -> Result<Self, ControlProtocolError> {
        validate_prefix(u32_at(bytes, 0), u16_at(bytes, 4))?;
        let header = Self {
            operation: ControlOperation::from_wire(u16_at(bytes, 6))?,
            id: ControlRequestId::new(u64_at(bytes, 8))?,
            target: target_from(u64_at(bytes, 16), u64_at(bytes, 24), bytes[32], bytes[33])?,
            slot: slot_from(u64_at(bytes, 36))?,
            payload_length: usize::try_from(u32_at(bytes, 44)).map_err(|_| too_large())?,
        };
        if u16_at(bytes, 34) != 0 {
            return Err(ControlProtocolError::NonZeroReserved);
        }
        header.validate()?;
        Ok(header)
    }
    fn validate(&self) -> Result<(), ControlProtocolError> {
        validate_slot(self.operation, self.slot)?;
        self.operation.validate_payload(self.payload_length)
    }
    fn into_request(self, payload: Vec<u8>) -> Result<ControlRequest, ControlProtocolError> {
        if payload.len() != self.payload_length {
            return Err(ControlProtocolError::FrameLengthMismatch {
                declared: self.payload_length,
                expected: payload.len(),
            });
        }
        ControlRequest::new(self.id, self.target, self.operation, self.slot, &payload)
    }
    fn encode(self) -> Result<[u8; CONTROL_REQUEST_HEADER_BYTES], ControlProtocolError> {
        self.validate()?;
        let mut bytes = [0; CONTROL_REQUEST_HEADER_BYTES];
        put_u32(&mut bytes, 0, CONTROL_PROTOCOL_MAGIC);
        put_u16(&mut bytes, 4, CONTROL_PROTOCOL_VERSION);
        put_u16(&mut bytes, 6, self.operation.wire_code());
        put_u64(&mut bytes, 8, self.id.get());
        put_u64(&mut bytes, 16, self.target.rack.generation);
        put_u64(&mut bytes, 24, self.target.bank.generation);
        bytes[32] = self.target.rack.index;
        bytes[33] = self.target.bank.index;
        put_u64(&mut bytes, 36, self.slot.map_or(NO_SLOT, SlotIdentity::get));
        put_u32(&mut bytes, 44, u32_len(self.payload_length)?);
        Ok(bytes)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ResponseHeader {
    id: ControlRequestId,
    target: ControlTarget,
    slot: Option<SlotIdentity>,
    status: ControlResponseStatus,
    payload_length: usize,
    error_length: usize,
}

impl ResponseHeader {
    fn from_response(
        response: &ControlResponse,
        error_length: usize,
    ) -> Result<Self, ControlProtocolError> {
        let header = Self {
            id: response.id,
            target: response.target,
            slot: response.slot,
            status: response.status,
            payload_length: response.payload.len(),
            error_length,
        };
        header.validate()?;
        Ok(header)
    }
    fn decode(bytes: &[u8; CONTROL_RESPONSE_HEADER_BYTES]) -> Result<Self, ControlProtocolError> {
        validate_prefix(u32_at(bytes, 0), u16_at(bytes, 4))?;
        let header = Self {
            status: ControlResponseStatus::from_wire(u16_at(bytes, 6))?,
            id: ControlRequestId::new(u64_at(bytes, 8))?,
            target: target_from(u64_at(bytes, 16), u64_at(bytes, 24), bytes[32], bytes[33])?,
            slot: slot_from(u64_at(bytes, 36))?,
            payload_length: usize::try_from(u32_at(bytes, 44)).map_err(|_| too_large())?,
            error_length: usize::try_from(u32_at(bytes, 48)).map_err(|_| too_large())?,
        };
        if u16_at(bytes, 34) != 0 {
            return Err(ControlProtocolError::NonZeroReserved);
        }
        header.validate()?;
        Ok(header)
    }
    fn validate(&self) -> Result<(), ControlProtocolError> {
        if self.payload_length > MAX_CONTROL_PAYLOAD_BYTES {
            return Err(ControlProtocolError::ResponsePayloadTooLarge {
                maximum: MAX_CONTROL_PAYLOAD_BYTES,
                received: self.payload_length,
            });
        }
        if self.error_length > MAX_CONTROL_ERROR_RECORD_BYTES {
            return Err(ControlProtocolError::ErrorRecordTooLarge {
                maximum: MAX_CONTROL_ERROR_RECORD_BYTES,
                received: self.error_length,
            });
        }
        match (self.status.success(), self.error_length) {
            (true, 0) | (false, ERROR_HEADER_BYTES..) => Ok(()),
            (true, _) => Err(ControlProtocolError::SuccessResponseHasError),
            (false, _) => Err(ControlProtocolError::ErrorResponseMissingRecord),
        }
    }
    fn into_response(
        self,
        payload: Vec<u8>,
        error: Vec<u8>,
    ) -> Result<ControlResponse, ControlProtocolError> {
        self.validate()?;
        if payload.len() != self.payload_length || error.len() != self.error_length {
            return Err(ControlProtocolError::FrameLengthMismatch {
                declared: self.payload_length + self.error_length,
                expected: payload.len() + error.len(),
            });
        }
        let record = if error.is_empty() {
            None
        } else {
            Some(ControlErrorRecord::decode(&error)?)
        };
        ControlResponse::new(
            self.id,
            self.target,
            self.slot,
            self.status,
            &payload,
            record,
        )
    }
    fn encode(self) -> Result<[u8; CONTROL_RESPONSE_HEADER_BYTES], ControlProtocolError> {
        self.validate()?;
        let mut bytes = [0; CONTROL_RESPONSE_HEADER_BYTES];
        put_u32(&mut bytes, 0, CONTROL_PROTOCOL_MAGIC);
        put_u16(&mut bytes, 4, CONTROL_PROTOCOL_VERSION);
        put_u16(&mut bytes, 6, self.status.wire_code());
        put_u64(&mut bytes, 8, self.id.get());
        put_u64(&mut bytes, 16, self.target.rack.generation);
        put_u64(&mut bytes, 24, self.target.bank.generation);
        bytes[32] = self.target.rack.index;
        bytes[33] = self.target.bank.index;
        put_u64(&mut bytes, 36, self.slot.map_or(NO_SLOT, SlotIdentity::get));
        put_u32(&mut bytes, 44, u32_len(self.payload_length)?);
        put_u32(&mut bytes, 48, u32_len(self.error_length)?);
        Ok(bytes)
    }
}

fn validate_prefix(magic: u32, version: u16) -> Result<(), ControlProtocolError> {
    if magic != CONTROL_PROTOCOL_MAGIC {
        return Err(ControlProtocolError::UnknownMagic(magic));
    }
    if version != CONTROL_PROTOCOL_VERSION {
        return Err(ControlProtocolError::UnsupportedVersion(version));
    }
    Ok(())
}
fn target_from(
    rack_generation: u64,
    bank_generation: u64,
    rack_index: u8,
    bank_index: u8,
) -> Result<ControlTarget, ControlProtocolError> {
    Ok(ControlTarget::new(
        RackIdentity::new(rack_index, rack_generation)?,
        BankIdentity::new(bank_index, bank_generation)?,
    ))
}
fn slot_from(value: u64) -> Result<Option<SlotIdentity>, ControlProtocolError> {
    if value == NO_SLOT {
        Ok(None)
    } else {
        SlotIdentity::new(value).map(Some)
    }
}
fn validate_slot(
    operation: ControlOperation,
    slot: Option<SlotIdentity>,
) -> Result<(), ControlProtocolError> {
    match (operation.slot_rule(), slot) {
        (SlotRule::Required, None) => Err(ControlProtocolError::MissingSlotIdentity(operation)),
        (SlotRule::Forbidden, Some(_)) => {
            Err(ControlProtocolError::UnexpectedSlotIdentity(operation))
        }
        _ => Ok(()),
    }
}

fn validate_reorder_payload(payload: &[u8]) -> Result<(), ControlProtocolError> {
    let count = usize::from(payload[0]);
    if count == 0 || count > usize::from(CONTROL_RACK_COUNT) || payload.len() != 1 + count * 2 {
        return Err(ControlProtocolError::InvalidPayload);
    }
    let mut current = [false; 8];
    for &slot in &payload[1..=count] {
        let index = usize::from(slot);
        if index >= current.len() || std::mem::replace(&mut current[index], true) {
            return Err(ControlProtocolError::InvalidPayload);
        }
    }
    let mut ordered = [false; 8];
    for &slot in &payload[1 + count..] {
        let index = usize::from(slot);
        if index >= ordered.len() || !current[index] || std::mem::replace(&mut ordered[index], true)
        {
            return Err(ControlProtocolError::InvalidPayload);
        }
    }
    Ok(())
}

fn read_request_header(
    reader: &mut impl Read,
) -> Result<(RequestHeader, usize), ControlProtocolError> {
    let body = read_body_length(reader, CONTROL_REQUEST_HEADER_BYTES)?;
    let mut bytes = [0; CONTROL_REQUEST_HEADER_BYTES];
    reader.read_exact(&mut bytes)?;
    let header = RequestHeader::decode(&bytes)?;
    let expected = CONTROL_REQUEST_HEADER_BYTES + header.payload_length;
    if body != expected {
        return Err(ControlProtocolError::FrameLengthMismatch {
            declared: body,
            expected,
        });
    }
    Ok((header, header.payload_length))
}
fn read_response_header(
    reader: &mut impl Read,
) -> Result<(ResponseHeader, usize, usize), ControlProtocolError> {
    let body = read_body_length(reader, CONTROL_RESPONSE_HEADER_BYTES)?;
    let mut bytes = [0; CONTROL_RESPONSE_HEADER_BYTES];
    reader.read_exact(&mut bytes)?;
    let header = ResponseHeader::decode(&bytes)?;
    let expected = CONTROL_RESPONSE_HEADER_BYTES + header.payload_length + header.error_length;
    if body != expected {
        return Err(ControlProtocolError::FrameLengthMismatch {
            declared: body,
            expected,
        });
    }
    Ok((header, header.payload_length, header.error_length))
}
fn read_body_length(reader: &mut impl Read, minimum: usize) -> Result<usize, ControlProtocolError> {
    let mut bytes = [0; PREFIX_BYTES];
    reader.read_exact(&mut bytes)?;
    let length = usize::try_from(u32::from_le_bytes(bytes)).map_err(|_| too_large())?;
    if length < minimum {
        return Err(ControlProtocolError::FrameTooShort {
            received: length,
            minimum,
        });
    }
    if length > MAX_CONTROL_FRAME_BYTES {
        return Err(ControlProtocolError::FrameTooLarge {
            maximum: MAX_CONTROL_FRAME_BYTES,
            received: length,
        });
    }
    Ok(length)
}
fn read_payload(reader: &mut impl Read, length: usize) -> Result<Vec<u8>, ControlProtocolError> {
    if length > MAX_CONTROL_PAYLOAD_BYTES {
        return Err(ControlProtocolError::ResponsePayloadTooLarge {
            maximum: MAX_CONTROL_PAYLOAD_BYTES,
            received: length,
        });
    }
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload)?;
    Ok(payload)
}
fn read_error(reader: &mut impl Read, length: usize) -> Result<Vec<u8>, ControlProtocolError> {
    if length > MAX_CONTROL_ERROR_RECORD_BYTES {
        return Err(ControlProtocolError::ErrorRecordTooLarge {
            maximum: MAX_CONTROL_ERROR_RECORD_BYTES,
            received: length,
        });
    }
    let mut error = vec![0; length];
    reader.read_exact(&mut error)?;
    Ok(error)
}
// Each frame is assembled into one buffer and issued as a single write so a frame is never
// split across syscalls; readers with finite timeouts must never observe a torn frame start.
fn write_request(
    writer: &mut impl Write,
    header: RequestHeader,
    payload: &[u8],
) -> Result<(), ControlProtocolError> {
    let bytes = header.encode()?;
    let mut frame = Vec::with_capacity(PREFIX_BYTES + CONTROL_REQUEST_HEADER_BYTES + payload.len());
    encode_body_length(&mut frame, CONTROL_REQUEST_HEADER_BYTES + payload.len())?;
    frame.extend_from_slice(&bytes);
    frame.extend_from_slice(payload);
    writer.write_all(&frame)?;
    Ok(())
}
fn write_response(
    writer: &mut impl Write,
    header: ResponseHeader,
    payload: &[u8],
    error: &[u8],
) -> Result<(), ControlProtocolError> {
    let bytes = header.encode()?;
    let mut frame = Vec::with_capacity(
        PREFIX_BYTES + CONTROL_RESPONSE_HEADER_BYTES + payload.len() + error.len(),
    );
    encode_body_length(
        &mut frame,
        CONTROL_RESPONSE_HEADER_BYTES + payload.len() + error.len(),
    )?;
    frame.extend_from_slice(&bytes);
    frame.extend_from_slice(payload);
    frame.extend_from_slice(error);
    writer.write_all(&frame)?;
    Ok(())
}
fn encode_body_length(frame: &mut Vec<u8>, length: usize) -> Result<(), ControlProtocolError> {
    if length > MAX_CONTROL_FRAME_BYTES {
        return Err(ControlProtocolError::FrameTooLarge {
            maximum: MAX_CONTROL_FRAME_BYTES,
            received: length,
        });
    }
    frame.extend_from_slice(&u32_len(length)?.to_le_bytes());
    Ok(())
}
fn u32_len(length: usize) -> Result<u32, ControlProtocolError> {
    u32::try_from(length).map_err(|_| too_large())
}
fn too_large() -> ControlProtocolError {
    ControlProtocolError::FrameTooLarge {
        maximum: MAX_CONTROL_FRAME_BYTES,
        received: usize::MAX,
    }
}
fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}
fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}
fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ])
}
fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}
fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

/// Reusable Unix-domain request/response stream wrappers. They neither launch helpers nor load plug-ins.
#[cfg(unix)]
pub mod unix {
    use super::{
        ControlProtocolError, ControlRequest, ControlRequestGate, ControlResponse, ControlTarget,
    };
    use std::{io::Write, os::unix::net::UnixStream, path::Path};

    #[derive(Debug)]
    pub struct UnixControlClient {
        stream: UnixStream,
        last_sent_id: Option<u64>,
    }
    impl UnixControlClient {
        pub fn connect(path: impl AsRef<Path>) -> Result<Self, ControlProtocolError> {
            Ok(Self::from_stream(UnixStream::connect(path)?))
        }
        #[must_use]
        pub fn from_stream(stream: UnixStream) -> Self {
            Self {
                stream,
                last_sent_id: None,
            }
        }
        pub fn send(&mut self, request: &ControlRequest) -> Result<(), ControlProtocolError> {
            let id = request.request_id().get();
            if let Some(previous) = self.last_sent_id {
                if id == previous {
                    return Err(ControlProtocolError::DuplicateRequestId(id));
                }
                if id < previous {
                    return Err(ControlProtocolError::NonMonotonicRequestId {
                        previous,
                        received: id,
                    });
                }
            }
            request.write_to(&mut self.stream)?;
            self.stream.flush()?;
            self.last_sent_id = Some(id);
            Ok(())
        }
        pub fn receive(&mut self) -> Result<ControlResponse, ControlProtocolError> {
            ControlResponse::read_from(&mut self.stream)
        }
        pub fn round_trip(
            &mut self,
            request: &ControlRequest,
        ) -> Result<ControlResponse, ControlProtocolError> {
            self.send(request)?;
            let response = self.receive()?;
            response.validate_for(request)?;
            Ok(response)
        }
    }

    #[derive(Debug)]
    pub struct UnixControlServer {
        stream: UnixStream,
        gate: ControlRequestGate,
    }
    impl UnixControlServer {
        #[must_use]
        pub fn from_stream(stream: UnixStream, target: ControlTarget) -> Self {
            Self {
                stream,
                gate: ControlRequestGate::new(target),
            }
        }
        pub fn receive(&mut self) -> Result<ControlRequest, ControlProtocolError> {
            self.gate.read_from(&mut self.stream)
        }
        pub fn respond(&mut self, response: &ControlResponse) -> Result<(), ControlProtocolError> {
            response.write_to(&mut self.stream)?;
            self.stream.flush()?;
            Ok(())
        }
        #[must_use]
        pub const fn gate(&self) -> &ControlRequestGate {
            &self.gate
        }
        pub fn gate_mut(&mut self) -> &mut ControlRequestGate {
            &mut self.gate
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Cursor, Read};

    fn target() -> ControlTarget {
        ControlTarget::new(
            RackIdentity::new(1, 11).expect("rack"),
            BankIdentity::new(0, 21).expect("bank"),
        )
    }
    fn slot() -> SlotIdentity {
        SlotIdentity::new(31).expect("slot")
    }
    fn request(operation: ControlOperation, id: u64) -> ControlRequest {
        let (minimum, _) = operation.payload_bounds();
        let slot = match operation.slot_rule() {
            SlotRule::Required => Some(slot()),
            SlotRule::Forbidden | SlotRule::Optional => None,
        };
        let payload = if operation == ControlOperation::ReorderRack {
            vec![2, 0, 6, 6, 0]
        } else {
            vec![1; minimum]
        };
        ControlRequest::new(
            ControlRequestId::new(id).expect("ID"),
            target(),
            operation,
            slot,
            &payload,
        )
        .expect("request")
    }

    #[test]
    fn every_lifecycle_parameter_state_editor_and_health_operation_round_trips() {
        for (index, operation) in ControlOperation::ALL.into_iter().enumerate() {
            let request = request(operation, u64::try_from(index).expect("index") + 1);
            let mut bytes = Vec::new();
            request.write_to(&mut bytes).expect("write");
            assert_eq!(
                ControlRequest::read_from(&mut Cursor::new(bytes)).expect("read"),
                request
            );
        }
    }

    #[test]
    fn bounded_error_response_round_trips_with_correlation() {
        let request = request(ControlOperation::LoadPlugin, 1);
        let response = ControlResponse::error(
            request.request_id(),
            request.target(),
            request.slot(),
            ControlResponseStatus::Rejected,
            ControlErrorRecord::new(ControlErrorCode::UNAVAILABLE, "quarantined").expect("record"),
        )
        .expect("response");
        let mut bytes = Vec::new();
        response.write_to(&mut bytes).expect("write");
        let decoded = ControlResponse::read_from(&mut Cursor::new(bytes)).expect("read");
        decoded.validate_for(&request).expect("correlation");
        assert_eq!(decoded, response);
    }

    #[test]
    fn malformed_unknown_oversized_stale_and_duplicate_headers_reject_before_payload_read() {
        let preload = request(ControlOperation::PreloadPlugin, 1);
        let mut unknown = request_header(&preload);
        put_u16(&mut unknown, 6, u16::MAX);
        let mut unknown_reader =
            PanicAfterHeader::new(frame(CONTROL_REQUEST_HEADER_BYTES + 1, &unknown));
        assert!(matches!(
            ControlRequest::read_from(&mut unknown_reader),
            Err(ControlProtocolError::UnknownOperation(_))
        ));

        let mut oversized = request_header(&preload);
        put_u32(
            &mut oversized,
            44,
            u32::try_from(MAX_PLUGIN_REFERENCE_BYTES + 1).expect("bound"),
        );
        let mut oversized_reader = PanicAfterHeader::new(frame(
            CONTROL_REQUEST_HEADER_BYTES + MAX_PLUGIN_REFERENCE_BYTES + 1,
            &oversized,
        ));
        assert!(matches!(
            ControlRequest::read_from(&mut oversized_reader),
            Err(ControlProtocolError::PayloadTooLarge { .. })
        ));

        let mut gate = ControlRequestGate::new(target());
        let mut valid = Vec::new();
        preload.write_to(&mut valid).expect("write");
        gate.read_from(&mut Cursor::new(valid)).expect("first");
        let header = request_header(&preload);
        let mut duplicate_reader = PanicAfterHeader::new(frame(
            CONTROL_REQUEST_HEADER_BYTES + preload.payload().len(),
            &header,
        ));
        assert!(matches!(
            gate.read_from(&mut duplicate_reader),
            Err(ControlProtocolError::DuplicateRequestId(1))
        ));

        let next = request(ControlOperation::LoadPlugin, 2);
        let mut stale = request_header(&next);
        put_u64(&mut stale, 16, 10);
        let mut stale_reader = PanicAfterHeader::new(frame(
            CONTROL_REQUEST_HEADER_BYTES + next.payload().len(),
            &stale,
        ));
        assert!(matches!(
            gate.read_from(&mut stale_reader),
            Err(ControlProtocolError::StaleRackGeneration { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn unix_domain_transport_delivers_an_admitted_request_response() {
        use super::unix::{UnixControlClient, UnixControlServer};
        use std::{os::unix::net::UnixStream, thread};
        let request = request(ControlOperation::QueryHealth, 1);
        let (client, server) = UnixStream::pair().expect("pair");
        let worker = thread::spawn(move || -> Result<(), ControlProtocolError> {
            let mut server = UnixControlServer::from_stream(server, target());
            let received = server.receive()?;
            server.respond(&ControlResponse::success(
                received.request_id(),
                received.target(),
                received.slot(),
                b"healthy",
            )?)
        });
        let response = UnixControlClient::from_stream(client)
            .round_trip(&request)
            .expect("round trip");
        assert_eq!(response.payload(), b"healthy");
        worker.join().expect("thread").expect("worker");
    }

    fn request_header(request: &ControlRequest) -> [u8; CONTROL_REQUEST_HEADER_BYTES] {
        RequestHeader::from_request(request)
            .expect("header")
            .encode()
            .expect("encode")
    }
    fn frame(length: usize, header: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&u32::try_from(length).expect("length").to_le_bytes());
        bytes.extend_from_slice(header);
        bytes
    }
    struct PanicAfterHeader(Cursor<Vec<u8>>);
    impl PanicAfterHeader {
        fn new(bytes: Vec<u8>) -> Self {
            Self(Cursor::new(bytes))
        }
    }
    impl Read for PanicAfterHeader {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            let before = self.0.position();
            let count = self.0.read(bytes)?;
            assert!(
                count != 0 || before != self.0.position(),
                "payload read after rejection"
            );
            Ok(count)
        }
    }
}
