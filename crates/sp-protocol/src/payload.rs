#![allow(
    missing_docs,
    clippy::cast_possible_truncation,
    clippy::if_not_else,
    clippy::missing_errors_doc,
    clippy::range_plus_one
)]
//! Typed bounded payload codecs for [`crate::control::ControlRequest`] and responses.
//!
//! `PreloadPlugin` and `LoadPlugin` use [`PluginSlotConfiguration`]; `RebuildRack` uses
//! [`RackTopology`]; `ReorderRack` uses [`SlotOrder`]; both bypass operations use [`Bypass`];
//! metadata/read/gesture use [`ParameterId`], write uses [`ParameterWrite`]; restore uses
//! [`StateRestore`]; latency/restart/health use their matching report records; and editor open or
//! resize use [`EditorGeometry`]. Every decoder validates all lengths and ranges before allocating.

use crate::control::{
    CONTROL_RACK_COUNT, ControlProtocolError, MAX_CONTROL_STATE_BYTES, MAX_PLUGIN_REFERENCE_BYTES,
    SlotIdentity,
};

pub const MAX_STATE_CHUNK_BYTES: usize = (MAX_CONTROL_STATE_BYTES - 8) / 2;

pub trait ControlPayloadCodec: Sized {
    fn encode(&self) -> Result<Vec<u8>, ControlProtocolError>;
    fn decode(bytes: &[u8]) -> Result<Self, ControlProtocolError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginSlotConfiguration {
    pub slot: u8,
    pub input_channels: u8,
    pub output_channels: u8,
    pub event_input_active: bool,
    pub bundle_path: String,
    pub class_id: Option<String>,
}
impl ControlPayloadCodec for PluginSlotConfiguration {
    fn encode(&self) -> Result<Vec<u8>, ControlProtocolError> {
        if self.slot >= CONTROL_RACK_COUNT
            || self.bundle_path.is_empty()
            || self.input_channels > 2
            || self.output_channels == 0
            || self.output_channels > 2
        {
            return Err(ControlProtocolError::InvalidPayload);
        }
        let path = self.bundle_path.as_bytes();
        let class = self.class_id.as_deref().unwrap_or("").as_bytes();
        let len = 8 + path.len() + class.len();
        if len > MAX_PLUGIN_REFERENCE_BYTES
            || path.len() > u16::MAX as usize
            || class.len() > u16::MAX as usize
        {
            return Err(ControlProtocolError::InvalidPayload);
        }
        let mut out = Vec::with_capacity(len);
        out.push(self.slot);
        out.push(self.input_channels);
        out.push(self.output_channels);
        out.push(u8::from(self.event_input_active));
        put16(&mut out, path.len() as u16);
        put16(&mut out, class.len() as u16);
        out.extend_from_slice(path);
        out.extend_from_slice(class);
        Ok(out)
    }
    fn decode(bytes: &[u8]) -> Result<Self, ControlProtocolError> {
        if bytes.len() < 8
            || bytes.len() > MAX_PLUGIN_REFERENCE_BYTES
            || bytes[0] >= CONTROL_RACK_COUNT
            || bytes[1] > 2
            || bytes[2] == 0
            || bytes[2] > 2
            || bytes[3] > 1
        {
            return Err(ControlProtocolError::InvalidPayload);
        }
        let path_len = usize::from(at16(bytes, 4));
        let class_len = usize::from(at16(bytes, 6));
        if 8 + path_len + class_len != bytes.len() || path_len == 0 {
            return Err(ControlProtocolError::InvalidPayload);
        }
        let path_end = 8 + path_len;
        let bundle_path = std::str::from_utf8(&bytes[8..path_end])
            .map_err(|_| ControlProtocolError::InvalidPayload)?
            .to_owned();
        let class = std::str::from_utf8(&bytes[path_end..])
            .map_err(|_| ControlProtocolError::InvalidPayload)?;
        Ok(Self {
            slot: bytes[0],
            input_channels: bytes[1],
            output_channels: bytes[2],
            event_input_active: bytes[3] == 1,
            bundle_path,
            class_id: (!class.is_empty()).then(|| class.to_owned()),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RackTopology {
    pub slots: Vec<PluginSlotConfiguration>,
}
impl ControlPayloadCodec for RackTopology {
    fn encode(&self) -> Result<Vec<u8>, ControlProtocolError> {
        if self.slots.is_empty() || self.slots.len() > CONTROL_RACK_COUNT as usize {
            return Err(ControlProtocolError::InvalidPayload);
        }
        let mut used = [false; 8];
        let mut out = vec![self.slots.len() as u8];
        for slot in &self.slots {
            let encoded = slot.encode()?;
            if used[slot.slot as usize] || encoded.len() > u16::MAX as usize {
                return Err(ControlProtocolError::InvalidPayload);
            }
            used[slot.slot as usize] = true;
            put16(&mut out, encoded.len() as u16);
            out.extend_from_slice(&encoded);
        }
        if out.len() > 32_768 {
            Err(ControlProtocolError::InvalidPayload)
        } else {
            Ok(out)
        }
    }
    fn decode(bytes: &[u8]) -> Result<Self, ControlProtocolError> {
        let Some(&count) = bytes.first() else {
            return Err(ControlProtocolError::InvalidPayload);
        };
        if count == 0 || count > CONTROL_RACK_COUNT {
            return Err(ControlProtocolError::InvalidPayload);
        }
        let mut offset = 1;
        let mut used = [false; 8];
        let mut slots = Vec::with_capacity(count as usize);
        for _ in 0..count {
            if offset + 2 > bytes.len() {
                return Err(ControlProtocolError::InvalidPayload);
            }
            let len = usize::from(at16(bytes, offset));
            offset += 2;
            if len == 0 || offset + len > bytes.len() {
                return Err(ControlProtocolError::InvalidPayload);
            }
            let slot = PluginSlotConfiguration::decode(&bytes[offset..offset + len])?;
            if used[slot.slot as usize] {
                return Err(ControlProtocolError::InvalidPayload);
            }
            used[slot.slot as usize] = true;
            slots.push(slot);
            offset += len;
        }
        if offset != bytes.len() {
            Err(ControlProtocolError::InvalidPayload)
        } else {
            Ok(Self { slots })
        }
    }
}

/// Reorder encoding has count, complete current membership, then the requested complete order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SlotOrder {
    pub current: Vec<u8>,
    pub order: Vec<u8>,
}
impl ControlPayloadCodec for SlotOrder {
    fn encode(&self) -> Result<Vec<u8>, ControlProtocolError> {
        validate_order(&self.current, &self.order)?;
        let mut out = Vec::with_capacity(1 + self.current.len() * 2);
        out.push(self.current.len() as u8);
        out.extend_from_slice(&self.current);
        out.extend_from_slice(&self.order);
        Ok(out)
    }
    fn decode(bytes: &[u8]) -> Result<Self, ControlProtocolError> {
        if bytes.is_empty() {
            return Err(ControlProtocolError::InvalidPayload);
        }
        let count = bytes[0] as usize;
        if count == 0 || count > 8 || bytes.len() != 1 + count * 2 {
            return Err(ControlProtocolError::InvalidPayload);
        }
        let current = bytes[1..1 + count].to_vec();
        let order = bytes[1 + count..].to_vec();
        validate_order(&current, &order)?;
        Ok(Self { current, order })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Bypass {
    pub enabled: bool,
}
impl ControlPayloadCodec for Bypass {
    fn encode(&self) -> Result<Vec<u8>, ControlProtocolError> {
        Ok(vec![u8::from(self.enabled)])
    }
    fn decode(bytes: &[u8]) -> Result<Self, ControlProtocolError> {
        match bytes {
            [0] => Ok(Self { enabled: false }),
            [1] => Ok(Self { enabled: true }),
            _ => Err(ControlProtocolError::InvalidPayload),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParameterId {
    pub value: u64,
}
impl ParameterId {
    /// Decodes a fixed-width parameter identifier.
    pub fn decode(bytes: &[u8]) -> Result<Self, ControlProtocolError> {
        <Self as ControlPayloadCodec>::decode(bytes)
    }
}
impl ControlPayloadCodec for ParameterId {
    fn encode(&self) -> Result<Vec<u8>, ControlProtocolError> {
        Ok(self.value.to_le_bytes().to_vec())
    }
    fn decode(bytes: &[u8]) -> Result<Self, ControlProtocolError> {
        if bytes.len() != 8 {
            Err(ControlProtocolError::InvalidPayload)
        } else {
            Ok(Self {
                value: at64(bytes, 0),
            })
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParameterWrite {
    pub id: ParameterId,
    pub normalized: f64,
}

/// Bounded controller metadata for one parameter. Strings are UTF-8 and capped at 255 bytes.
#[derive(Clone, Debug, PartialEq)]
pub struct ParameterMetadata {
    pub id: ParameterId,
    pub normalized: f64,
    pub default_normalized: f64,
    pub step_count: i32,
    pub flags: u32,
    pub name: String,
    pub unit: String,
    pub formatted: String,
}
impl ControlPayloadCodec for ParameterMetadata {
    fn encode(&self) -> Result<Vec<u8>, ControlProtocolError> {
        if !self.normalized.is_finite()
            || !self.default_normalized.is_finite()
            || !(0.0..=1.0).contains(&self.normalized)
            || !(0.0..=1.0).contains(&self.default_normalized)
            || self.name.len() > u8::MAX as usize
            || self.unit.len() > u8::MAX as usize
            || self.formatted.len() > u8::MAX as usize
        {
            return Err(ControlProtocolError::InvalidPayload);
        }
        let mut out =
            Vec::with_capacity(35 + self.name.len() + self.unit.len() + self.formatted.len());
        out.extend_from_slice(&self.id.value.to_le_bytes());
        out.extend_from_slice(&self.normalized.to_le_bytes());
        out.extend_from_slice(&self.default_normalized.to_le_bytes());
        put32(&mut out, self.step_count.cast_unsigned());
        put32(&mut out, self.flags);
        out.push(self.name.len() as u8);
        out.push(self.unit.len() as u8);
        out.push(self.formatted.len() as u8);
        out.extend_from_slice(self.name.as_bytes());
        out.extend_from_slice(self.unit.as_bytes());
        out.extend_from_slice(self.formatted.as_bytes());
        Ok(out)
    }
    fn decode(bytes: &[u8]) -> Result<Self, ControlProtocolError> {
        if bytes.len() < 35 {
            return Err(ControlProtocolError::InvalidPayload);
        }
        let name_len = bytes[32] as usize;
        let unit_len = bytes[33] as usize;
        let formatted_len = bytes[34] as usize;
        if 35 + name_len + unit_len + formatted_len != bytes.len() {
            return Err(ControlProtocolError::InvalidPayload);
        }
        let normalized = f64::from_le_bytes(
            bytes[8..16]
                .try_into()
                .map_err(|_| ControlProtocolError::InvalidPayload)?,
        );
        let default_normalized = f64::from_le_bytes(
            bytes[16..24]
                .try_into()
                .map_err(|_| ControlProtocolError::InvalidPayload)?,
        );
        let result = Self {
            id: ParameterId {
                value: at64(bytes, 0),
            },
            normalized,
            default_normalized,
            step_count: at32(bytes, 24).cast_signed(),
            flags: at32(bytes, 28),
            name: std::str::from_utf8(&bytes[35..35 + name_len])
                .map_err(|_| ControlProtocolError::InvalidPayload)?
                .to_owned(),
            unit: std::str::from_utf8(&bytes[35 + name_len..35 + name_len + unit_len])
                .map_err(|_| ControlProtocolError::InvalidPayload)?
                .to_owned(),
            formatted: std::str::from_utf8(&bytes[35 + name_len + unit_len..])
                .map_err(|_| ControlProtocolError::InvalidPayload)?
                .to_owned(),
        };
        result.encode().map(|_| result)
    }
}

/// The current normalized value for one parameter.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReadParameter {
    pub id: ParameterId,
    pub normalized: f64,
}
impl ControlPayloadCodec for ReadParameter {
    fn encode(&self) -> Result<Vec<u8>, ControlProtocolError> {
        ParameterWrite {
            id: self.id,
            normalized: self.normalized,
        }
        .encode()
    }
    fn decode(bytes: &[u8]) -> Result<Self, ControlProtocolError> {
        let value = ParameterWrite::decode(bytes)?;
        Ok(Self {
            id: value.id,
            normalized: value.normalized,
        })
    }
}

/// A parameter gesture identifier used for begin/end requests and plug-in-originated output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParameterGesture {
    pub id: ParameterId,
}
impl ControlPayloadCodec for ParameterGesture {
    fn encode(&self) -> Result<Vec<u8>, ControlProtocolError> {
        self.id.encode()
    }
    fn decode(bytes: &[u8]) -> Result<Self, ControlProtocolError> {
        Ok(Self {
            id: ParameterId::decode(bytes)?,
        })
    }
}
pub type BeginParameterGesture = ParameterGesture;
pub type EndParameterGesture = ParameterGesture;
impl ControlPayloadCodec for ParameterWrite {
    fn encode(&self) -> Result<Vec<u8>, ControlProtocolError> {
        if !self.normalized.is_finite() || !(0.0..=1.0).contains(&self.normalized) {
            return Err(ControlProtocolError::InvalidPayload);
        }
        let mut out = self.id.value.to_le_bytes().to_vec();
        out.extend_from_slice(&self.normalized.to_le_bytes());
        Ok(out)
    }
    fn decode(bytes: &[u8]) -> Result<Self, ControlProtocolError> {
        if bytes.len() != 16 {
            return Err(ControlProtocolError::InvalidPayload);
        }
        let value = f64::from_le_bytes(
            bytes[8..16]
                .try_into()
                .map_err(|_| ControlProtocolError::InvalidPayload)?,
        );
        Self {
            id: ParameterId {
                value: at64(bytes, 0),
            },
            normalized: value,
        }
        .encode()
        .map(|_| Self {
            id: ParameterId {
                value: at64(bytes, 0),
            },
            normalized: value,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StateRestore {
    pub component: Vec<u8>,
    pub controller: Vec<u8>,
}
impl ControlPayloadCodec for StateRestore {
    fn encode(&self) -> Result<Vec<u8>, ControlProtocolError> {
        if self.component.len() > MAX_STATE_CHUNK_BYTES
            || self.controller.len() > MAX_STATE_CHUNK_BYTES
        {
            return Err(ControlProtocolError::InvalidPayload);
        }
        let mut out = Vec::with_capacity(8 + self.component.len() + self.controller.len());
        put32(&mut out, self.component.len() as u32);
        put32(&mut out, self.controller.len() as u32);
        out.extend_from_slice(&self.component);
        out.extend_from_slice(&self.controller);
        Ok(out)
    }
    fn decode(bytes: &[u8]) -> Result<Self, ControlProtocolError> {
        if bytes.len() < 8 {
            return Err(ControlProtocolError::InvalidPayload);
        }
        let component = at32(bytes, 0) as usize;
        let controller = at32(bytes, 4) as usize;
        if component > MAX_STATE_CHUNK_BYTES
            || controller > MAX_STATE_CHUNK_BYTES
            || 8 + component + controller != bytes.len()
        {
            return Err(ControlProtocolError::InvalidPayload);
        }
        Ok(Self {
            component: bytes[8..8 + component].to_vec(),
            controller: bytes[8 + component..].to_vec(),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LatencyReport {
    pub samples: u64,
}
impl ControlPayloadCodec for LatencyReport {
    fn encode(&self) -> Result<Vec<u8>, ControlProtocolError> {
        Ok(self.samples.to_le_bytes().to_vec())
    }
    fn decode(bytes: &[u8]) -> Result<Self, ControlProtocolError> {
        if bytes.len() == 8 {
            Ok(Self {
                samples: at64(bytes, 0),
            })
        } else {
            Err(ControlProtocolError::InvalidPayload)
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RestartReport {
    pub requested: bool,
    pub reason: u32,
}
impl ControlPayloadCodec for RestartReport {
    fn encode(&self) -> Result<Vec<u8>, ControlProtocolError> {
        let mut out = vec![u8::from(self.requested)];
        put32(&mut out, self.reason);
        Ok(out)
    }
    fn decode(bytes: &[u8]) -> Result<Self, ControlProtocolError> {
        if bytes.len() != 5 || bytes[0] > 1 {
            Err(ControlProtocolError::InvalidPayload)
        } else {
            Ok(Self {
                requested: bytes[0] == 1,
                reason: at32(bytes, 1),
            })
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EditorGeometry {
    pub width: u32,
    pub height: u32,
}
impl ControlPayloadCodec for EditorGeometry {
    fn encode(&self) -> Result<Vec<u8>, ControlProtocolError> {
        if self.width == 0 || self.height == 0 {
            return Err(ControlProtocolError::InvalidPayload);
        }
        let mut out = Vec::with_capacity(8);
        put32(&mut out, self.width);
        put32(&mut out, self.height);
        Ok(out)
    }
    fn decode(bytes: &[u8]) -> Result<Self, ControlProtocolError> {
        if bytes.len() != 8 {
            return Err(ControlProtocolError::InvalidPayload);
        }
        let value = Self {
            width: at32(bytes, 0),
            height: at32(bytes, 4),
        };
        value.encode().map(|_| value)
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HealthReport {
    pub online: bool,
    pub current_slot: Option<SlotIdentity>,
    pub latency_samples: u32,
    pub restart_requested: bool,
}
impl ControlPayloadCodec for HealthReport {
    fn encode(&self) -> Result<Vec<u8>, ControlProtocolError> {
        let mut out = vec![u8::from(self.online)];
        out.extend_from_slice(&self.current_slot.map_or(0, SlotIdentity::get).to_le_bytes());
        put32(&mut out, self.latency_samples);
        out.push(u8::from(self.restart_requested));
        Ok(out)
    }
    fn decode(bytes: &[u8]) -> Result<Self, ControlProtocolError> {
        if bytes.len() != 14 || bytes[0] > 1 || bytes[13] > 1 {
            return Err(ControlProtocolError::InvalidPayload);
        }
        let raw = at64(bytes, 1);
        Ok(Self {
            online: bytes[0] == 1,
            current_slot: if raw == 0 {
                None
            } else {
                Some(SlotIdentity::new(raw)?)
            },
            latency_samples: at32(bytes, 9),
            restart_requested: bytes[13] == 1,
        })
    }
}

fn validate_order(current: &[u8], order: &[u8]) -> Result<(), ControlProtocolError> {
    if current.len() != order.len() || current.is_empty() || current.len() > 8 {
        return Err(ControlProtocolError::InvalidPayload);
    }
    let mut seen = [false; 8];
    for &slot in current {
        if slot >= 8 || seen[slot as usize] {
            return Err(ControlProtocolError::InvalidPayload);
        }
        seen[slot as usize] = true;
    }
    let mut ordered = [false; 8];
    for &slot in order {
        if slot >= 8 || !seen[slot as usize] || ordered[slot as usize] {
            return Err(ControlProtocolError::InvalidPayload);
        }
        ordered[slot as usize] = true;
    }
    Ok(())
}
fn at16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}
fn at32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("checked"))
}
fn at64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("checked"))
}
fn put16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}
fn put32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::{ControlPayloadCodec, ParameterId, ParameterMetadata};

    #[test]
    fn parameter_metadata_round_trip_preserves_formatted_value() {
        let metadata = ParameterMetadata {
            id: ParameterId { value: 42 },
            normalized: 0.5,
            default_normalized: 0.25,
            step_count: 100,
            flags: 3,
            name: "Frequency".to_owned(),
            unit: "Hz".to_owned(),
            formatted: "1.00 kHz".to_owned(),
        };
        let encoded = metadata.encode().expect("valid metadata");
        assert_eq!(ParameterMetadata::decode(&encoded).unwrap(), metadata);
    }
}
