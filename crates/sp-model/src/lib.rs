//! Platform-neutral, versioned session contracts for Superposition.
//!
//! This crate intentionally describes only persistent session state. It has no
//! plug-in SDK, host, audio-device, or operating-system types.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The only session schema version supported during Phase 0.
pub const CURRENT_SESSION_VERSION: u32 = 1;
/// Maximum number of racks in one session.
pub const MAX_RACKS: usize = 8;
/// Maximum number of plug-in slots in a rack.
pub const MAX_SLOTS_PER_RACK: usize = 8;
/// Maximum number of declared sources in one session.
pub const MAX_SOURCES: usize = 16;
/// Maximum number of declared endpoints in one session.
pub const MAX_ENDPOINTS: usize = 16;
/// Maximum number of saved scenes in one session.
pub const MAX_SCENES: usize = 128;
/// Maximum number of MIDI mappings in one session.
pub const MAX_MIDI_MAPPINGS: usize = 1_024;
/// Maximum number of normalized parameters stored for one plug-in instance.
pub const MAX_PARAMETERS_PER_PLUGIN: usize = 4_096;
/// Maximum number of parameter overrides in one scene.
pub const MAX_SCENE_PARAMETER_VALUES: usize = 4_096;

/// A persisted Superposition session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// The serialized schema version.
    #[serde(default = "current_session_version")]
    pub version: u32,
    /// Available input sources.
    #[serde(default)]
    pub sources: Vec<Source>,
    /// Available output endpoints.
    #[serde(default)]
    pub endpoints: Vec<Endpoint>,
    /// Ordered processing racks.
    #[serde(default)]
    pub racks: Vec<Rack>,
    /// Saved state recalls.
    #[serde(default)]
    pub scenes: Vec<Scene>,
    /// MIDI-to-parameter controls.
    #[serde(default)]
    pub midi_mappings: Vec<MidiMapping>,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            version: CURRENT_SESSION_VERSION,
            sources: Vec::new(),
            endpoints: Vec::new(),
            racks: Vec::new(),
            scenes: Vec::new(),
            midi_mappings: Vec::new(),
        }
    }
}

impl Session {
    /// Creates an empty session at the current schema version.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Validates all schema, capacity, value, and reference invariants.
    ///
    /// # Errors
    ///
    /// Returns [`ValidationError`] when a field, capacity, or cross-reference
    /// violates the Phase 0 session contract.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.version != CURRENT_SESSION_VERSION {
            return Err(ValidationError::UnsupportedVersion {
                found: self.version,
                supported: CURRENT_SESSION_VERSION,
            });
        }

        validate_capacity(Collection::Sources, self.sources.len(), MAX_SOURCES)?;
        validate_capacity(Collection::Endpoints, self.endpoints.len(), MAX_ENDPOINTS)?;
        validate_capacity(Collection::Racks, self.racks.len(), MAX_RACKS)?;
        validate_capacity(Collection::Scenes, self.scenes.len(), MAX_SCENES)?;
        validate_capacity(
            Collection::MidiMappings,
            self.midi_mappings.len(),
            MAX_MIDI_MAPPINGS,
        )?;

        let mut source_ids = BTreeSet::new();
        for source in &self.sources {
            validate_identifier(&source.id.0, EntityKind::Source)?;
            if !source_ids.insert(&source.id) {
                return Err(ValidationError::DuplicateId {
                    kind: EntityKind::Source,
                    id: source.id.0.clone(),
                });
            }
            source.layout.validate()?;
        }

        let mut endpoint_ids = BTreeSet::new();
        for endpoint in &self.endpoints {
            validate_identifier(&endpoint.id.0, EntityKind::Endpoint)?;
            if !endpoint_ids.insert(&endpoint.id) {
                return Err(ValidationError::DuplicateId {
                    kind: EntityKind::Endpoint,
                    id: endpoint.id.0.clone(),
                });
            }
            endpoint.layout.validate()?;
        }

        let mut rack_ids = BTreeSet::new();
        for rack in &self.racks {
            validate_identifier(&rack.id.0, EntityKind::Rack)?;
            if !rack_ids.insert(&rack.id) {
                return Err(ValidationError::DuplicateId {
                    kind: EntityKind::Rack,
                    id: rack.id.0.clone(),
                });
            }
            if !source_ids.contains(&rack.source_id) {
                return Err(ValidationError::UnknownReference {
                    owner: EntityKind::Rack,
                    reference: EntityKind::Source,
                    id: rack.source_id.0.clone(),
                });
            }
            if !endpoint_ids.contains(&rack.endpoint_id) {
                return Err(ValidationError::UnknownReference {
                    owner: EntityKind::Rack,
                    reference: EntityKind::Endpoint,
                    id: rack.endpoint_id.0.clone(),
                });
            }
            validate_capacity(Collection::RackSlots, rack.slots.len(), MAX_SLOTS_PER_RACK)?;

            let mut slot_ids = BTreeSet::new();
            for slot in &rack.slots {
                validate_identifier(&slot.id.0, EntityKind::PluginSlot)?;
                if !slot_ids.insert(&slot.id) {
                    return Err(ValidationError::DuplicateId {
                        kind: EntityKind::PluginSlot,
                        id: slot.id.0.clone(),
                    });
                }
                slot.plugin.validate()?;
                slot.parameters.validate()?;
            }
        }

        let mut scene_ids = BTreeSet::new();
        for scene in &self.scenes {
            validate_identifier(&scene.id.0, EntityKind::Scene)?;
            if !scene_ids.insert(&scene.id) {
                return Err(ValidationError::DuplicateId {
                    kind: EntityKind::Scene,
                    id: scene.id.0.clone(),
                });
            }
            scene.validate(&rack_ids, &self.racks)?;
        }

        let mut mapping_ids = BTreeSet::new();
        for mapping in &self.midi_mappings {
            validate_identifier(&mapping.id.0, EntityKind::MidiMapping)?;
            if !mapping_ids.insert(&mapping.id) {
                return Err(ValidationError::DuplicateId {
                    kind: EntityKind::MidiMapping,
                    id: mapping.id.0.clone(),
                });
            }
            mapping.validate(&rack_ids, &self.racks)?;
        }

        Ok(())
    }
}

const fn current_session_version() -> u32 {
    CURRENT_SESSION_VERSION
}

/// An input source declaration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    /// Stable source identifier.
    pub id: SourceId,
    /// User-facing name.
    pub name: String,
    /// Channel arrangement provided by this source.
    pub layout: ChannelLayout,
}

/// An output endpoint declaration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    /// Stable endpoint identifier.
    pub id: EndpointId,
    /// User-facing name.
    pub name: String,
    /// Channel arrangement accepted by this endpoint.
    pub layout: ChannelLayout,
}

/// One independently routable plug-in chain.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rack {
    /// Stable rack identifier.
    pub id: RackId,
    /// User-facing name.
    pub name: String,
    /// Source connected to this rack's input.
    pub source_id: SourceId,
    /// Endpoint connected to this rack's output.
    pub endpoint_id: EndpointId,
    /// Requested processing topology.
    #[serde(default)]
    pub topology: RackTopology,
    /// Ordered plug-in instances.
    #[serde(default)]
    pub slots: Vec<PluginSlot>,
}

/// A topology requested by a rack.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RackTopology {
    /// Process slots in a single serial chain.
    #[default]
    Serial,
    /// Process slots as parallel branches and sum their output.
    Parallel,
}

/// A plug-in instance in a rack.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PluginSlot {
    /// Stable slot/instance identifier.
    pub id: PluginInstanceId,
    /// Immutable identity used to find the plug-in.
    pub plugin: PluginDescriptor,
    /// Persisted normalized parameter values.
    #[serde(default)]
    pub parameters: NormalizedParameters,
}

/// Identity and fingerprint for a plug-in instance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginDescriptor {
    /// Stable vendor-neutral identity metadata.
    pub identity: PluginIdentity,
    /// Fingerprint of the plug-in binary/version expected by the session.
    pub fingerprint: PluginFingerprint,
}

impl PluginDescriptor {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_identifier(&self.identity.vendor, EntityKind::PluginIdentity)?;
        validate_identifier(&self.identity.name, EntityKind::PluginIdentity)?;
        validate_identifier(&self.identity.unique_id, EntityKind::PluginIdentity)?;
        validate_identifier(&self.fingerprint.algorithm, EntityKind::PluginFingerprint)?;
        validate_identifier(&self.fingerprint.digest, EntityKind::PluginFingerprint)?;
        validate_identifier(
            &self.fingerprint.plugin_version,
            EntityKind::PluginFingerprint,
        )
    }
}

/// Platform-neutral metadata that identifies a plug-in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginIdentity {
    /// Plug-in publisher.
    pub vendor: String,
    /// Plug-in display name.
    pub name: String,
    /// Publisher-defined stable identifier.
    pub unique_id: String,
}

/// A reproducible fingerprint of the plug-in loaded for a session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginFingerprint {
    /// Digest algorithm name, such as `sha256`.
    pub algorithm: String,
    /// Encoded digest bytes.
    pub digest: String,
    /// Plug-in's reported version string.
    pub plugin_version: String,
}

/// Canonically ordered normalized values for a plug-in's parameters.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NormalizedParameters {
    /// Mapping from stable parameter identifier to its normalized value.
    #[serde(default)]
    pub values: std::collections::BTreeMap<ParameterId, NormalizedValue>,
}

impl NormalizedParameters {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_capacity(
            Collection::PluginParameters,
            self.values.len(),
            MAX_PARAMETERS_PER_PLUGIN,
        )?;
        for (id, value) in &self.values {
            validate_identifier(&id.0, EntityKind::Parameter)?;
            value.validate()?;
        }
        Ok(())
    }
}

/// A value in the inclusive normalized parameter range `[0.0, 1.0]`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NormalizedValue(pub f32);

impl NormalizedValue {
    /// Creates a valid normalized value.
    ///
    /// # Errors
    ///
    /// Returns [`ValidationError::InvalidNormalizedValue`] when `value` is not
    /// finite or is outside the inclusive normalized range.
    pub fn new(value: f32) -> Result<Self, ValidationError> {
        let normalized = Self(value);
        normalized.validate()?;
        Ok(normalized)
    }

    /// Returns the underlying normalized value.
    #[must_use]
    pub const fn get(self) -> f32 {
        self.0
    }

    fn validate(self) -> Result<(), ValidationError> {
        if !self.0.is_finite() || !(0.0..=1.0).contains(&self.0) {
            return Err(ValidationError::InvalidNormalizedValue { value: self.0 });
        }
        Ok(())
    }
}

/// A saved recall state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Scene {
    /// Stable scene identifier.
    pub id: SceneId,
    /// User-facing scene name.
    pub name: String,
    /// Rack gain overrides.
    #[serde(default)]
    pub gains: Vec<RackGain>,
    /// Rack mute overrides.
    #[serde(default)]
    pub mutes: Vec<RackMute>,
    /// Plug-in bypass overrides.
    #[serde(default)]
    pub bypasses: Vec<SlotBypass>,
    /// Plug-in parameter overrides.
    #[serde(default)]
    pub parameter_values: Vec<SceneParameterValue>,
    /// Duration for interpolation into this scene, in milliseconds.
    #[serde(default)]
    pub transition_ms: u32,
}

impl Scene {
    fn validate(
        &self,
        rack_ids: &BTreeSet<&RackId>,
        racks: &[Rack],
    ) -> Result<(), ValidationError> {
        validate_capacity(Collection::SceneGains, self.gains.len(), MAX_RACKS)?;
        validate_capacity(Collection::SceneMutes, self.mutes.len(), MAX_RACKS)?;
        validate_capacity(
            Collection::SceneBypasses,
            self.bypasses.len(),
            MAX_RACKS * MAX_SLOTS_PER_RACK,
        )?;
        validate_capacity(
            Collection::SceneParameterValues,
            self.parameter_values.len(),
            MAX_SCENE_PARAMETER_VALUES,
        )?;

        let mut gain_racks = BTreeSet::new();
        for gain in &self.gains {
            validate_rack_reference(&gain.rack_id, rack_ids, EntityKind::Scene)?;
            if !gain_racks.insert(&gain.rack_id) {
                return Err(ValidationError::DuplicateTarget {
                    kind: EntityKind::Scene,
                    id: gain.rack_id.0.clone(),
                });
            }
            gain.gain_db.validate()?;
        }

        let mut mute_racks = BTreeSet::new();
        for mute in &self.mutes {
            validate_rack_reference(&mute.rack_id, rack_ids, EntityKind::Scene)?;
            if !mute_racks.insert(&mute.rack_id) {
                return Err(ValidationError::DuplicateTarget {
                    kind: EntityKind::Scene,
                    id: mute.rack_id.0.clone(),
                });
            }
        }

        let mut bypass_targets = BTreeSet::new();
        for bypass in &self.bypasses {
            let slot = resolve_slot(&bypass.rack_id, &bypass.slot_id, racks, EntityKind::Scene)?;
            let key = (&bypass.rack_id, &slot.id);
            if !bypass_targets.insert(key) {
                return Err(ValidationError::DuplicateTarget {
                    kind: EntityKind::Scene,
                    id: format!("{}/{}", bypass.rack_id.0, bypass.slot_id.0),
                });
            }
        }

        let mut parameter_targets = BTreeSet::new();
        for parameter in &self.parameter_values {
            let slot = resolve_slot(
                &parameter.rack_id,
                &parameter.slot_id,
                racks,
                EntityKind::Scene,
            )?;
            validate_parameter_reference(slot, &parameter.parameter_id, EntityKind::Scene)?;
            parameter.value.validate()?;
            let key = (
                &parameter.rack_id,
                &parameter.slot_id,
                &parameter.parameter_id,
            );
            if !parameter_targets.insert(key) {
                return Err(ValidationError::DuplicateTarget {
                    kind: EntityKind::Scene,
                    id: format!(
                        "{}/{}/{}",
                        parameter.rack_id.0, parameter.slot_id.0, parameter.parameter_id.0
                    ),
                });
            }
        }

        Ok(())
    }
}

/// A rack gain override expressed in decibels.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RackGain {
    /// Rack receiving the override.
    pub rack_id: RackId,
    /// Gain to apply.
    pub gain_db: GainDb,
}

/// A finite gain in the inclusive range `[-120.0, 24.0]` dB.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GainDb(pub f32);

impl GainDb {
    /// Creates a valid scene gain.
    ///
    /// # Errors
    ///
    /// Returns [`ValidationError::InvalidGain`] when `value` is not finite or
    /// outside the supported scene range.
    pub fn new(value: f32) -> Result<Self, ValidationError> {
        let gain = Self(value);
        gain.validate()?;
        Ok(gain)
    }

    /// Returns the gain in decibels.
    #[must_use]
    pub const fn get(self) -> f32 {
        self.0
    }

    fn validate(self) -> Result<(), ValidationError> {
        if !self.0.is_finite() || !(-120.0..=24.0).contains(&self.0) {
            return Err(ValidationError::InvalidGain { value: self.0 });
        }
        Ok(())
    }
}

/// A rack mute override.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RackMute {
    /// Rack receiving the override.
    pub rack_id: RackId,
    /// Whether the rack is muted.
    pub muted: bool,
}

/// A plug-in bypass override.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotBypass {
    /// Rack containing the plug-in.
    pub rack_id: RackId,
    /// Plug-in instance receiving the override.
    pub slot_id: PluginInstanceId,
    /// Whether the plug-in is bypassed.
    pub bypassed: bool,
}

/// A single normalized parameter override in a scene.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SceneParameterValue {
    /// Rack containing the parameter.
    pub rack_id: RackId,
    /// Plug-in instance containing the parameter.
    pub slot_id: PluginInstanceId,
    /// Stable parameter identifier.
    pub parameter_id: ParameterId,
    /// Target normalized value.
    pub value: NormalizedValue,
}

/// A MIDI control mapping.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MidiMapping {
    /// Stable mapping identifier.
    pub id: MidiMappingId,
    /// Incoming controller binding.
    pub source: MidiController,
    /// Parameter that receives the mapped value.
    pub target: ParameterAddress,
    /// Normalized output produced by MIDI value zero.
    pub minimum: NormalizedValue,
    /// Normalized output produced by MIDI value 127.
    pub maximum: NormalizedValue,
}

impl MidiMapping {
    fn validate(
        &self,
        rack_ids: &BTreeSet<&RackId>,
        racks: &[Rack],
    ) -> Result<(), ValidationError> {
        self.source.validate()?;
        validate_rack_reference(&self.target.rack_id, rack_ids, EntityKind::MidiMapping)?;
        let slot = resolve_slot(
            &self.target.rack_id,
            &self.target.slot_id,
            racks,
            EntityKind::MidiMapping,
        )?;
        validate_parameter_reference(slot, &self.target.parameter_id, EntityKind::MidiMapping)?;
        self.minimum.validate()?;
        self.maximum.validate()?;
        if self.minimum.0 > self.maximum.0 {
            return Err(ValidationError::InvalidMidiRange {
                minimum: self.minimum.0,
                maximum: self.maximum.0,
            });
        }
        Ok(())
    }
}

/// MIDI channel/controller coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MidiController {
    /// One-based MIDI channel number.
    pub channel: u8,
    /// Zero-based MIDI continuous-controller number.
    pub controller: u8,
}

impl MidiController {
    fn validate(self) -> Result<(), ValidationError> {
        if !(1..=16).contains(&self.channel) {
            return Err(ValidationError::InvalidMidiChannel {
                channel: self.channel,
            });
        }
        if self.controller > 127 {
            return Err(ValidationError::InvalidMidiController {
                controller: self.controller,
            });
        }
        Ok(())
    }
}

/// Identifies a plug-in parameter within a rack.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ParameterAddress {
    /// Rack containing the plug-in.
    pub rack_id: RackId,
    /// Plug-in instance containing the parameter.
    pub slot_id: PluginInstanceId,
    /// Stable parameter identifier.
    pub parameter_id: ParameterId,
}

/// A channel arrangement requested by a source or endpoint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChannelLayout {
    /// One discrete channel.
    Mono,
    /// Two discrete channels.
    Stereo,
    /// An explicitly-sized discrete layout.
    Discrete {
        /// Number of channels.
        channels: u8,
    },
}

impl ChannelLayout {
    /// Returns the number of discrete channels in this layout.
    #[must_use]
    pub const fn channels(&self) -> u8 {
        match self {
            Self::Mono => 1,
            Self::Stereo => 2,
            Self::Discrete { channels } => *channels,
        }
    }

    fn validate(&self) -> Result<(), ValidationError> {
        if self.channels() == 0 || self.channels() > 64 {
            return Err(ValidationError::InvalidChannelCount {
                channels: self.channels(),
            });
        }
        Ok(())
    }
}

/// Stable source identifier.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SourceId(pub String);
/// Stable endpoint identifier.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EndpointId(pub String);
/// Stable rack identifier.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RackId(pub String);
/// Stable plug-in instance identifier.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PluginInstanceId(pub String);
/// Stable plug-in parameter identifier.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ParameterId(pub String);
/// Stable scene identifier.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SceneId(pub String);
/// Stable MIDI mapping identifier.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MidiMappingId(pub String);

/// The collection whose capacity was exceeded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Collection {
    /// Sources collection.
    Sources,
    /// Endpoints collection.
    Endpoints,
    /// Racks collection.
    Racks,
    /// Rack slots collection.
    RackSlots,
    /// Scenes collection.
    Scenes,
    /// MIDI mappings collection.
    MidiMappings,
    /// Per-plug-in parameter collection.
    PluginParameters,
    /// Scene gains collection.
    SceneGains,
    /// Scene mutes collection.
    SceneMutes,
    /// Scene bypasses collection.
    SceneBypasses,
    /// Scene parameter values collection.
    SceneParameterValues,
}

/// The kind of entity named in a validation failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntityKind {
    /// Source entity.
    Source,
    /// Endpoint entity.
    Endpoint,
    /// Rack entity.
    Rack,
    /// Plug-in slot entity.
    PluginSlot,
    /// Plug-in identity metadata.
    PluginIdentity,
    /// Plug-in fingerprint metadata.
    PluginFingerprint,
    /// Parameter entity.
    Parameter,
    /// Scene entity.
    Scene,
    /// MIDI mapping entity.
    MidiMapping,
}

/// Reasons a session model cannot be accepted.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum ValidationError {
    /// A session uses an unsupported schema version.
    #[error("unsupported session version {found}; only version {supported} is supported")]
    UnsupportedVersion {
        /// Version present in the session.
        found: u32,
        /// Version supported by this crate.
        supported: u32,
    },
    /// A bounded collection is too large.
    #[error("{collection:?} has {found} entries; maximum is {capacity}")]
    CapacityExceeded {
        /// Collection that exceeded its bound.
        collection: Collection,
        /// Maximum allowed entries.
        capacity: usize,
        /// Entries present in the input.
        found: usize,
    },
    /// An ID is empty or whitespace-only.
    #[error("{kind:?} identifier must not be empty")]
    EmptyIdentifier {
        /// Entity whose identifier is invalid.
        kind: EntityKind,
    },
    /// The same ID appeared more than once.
    #[error("duplicate {kind:?} identifier {id:?}")]
    DuplicateId {
        /// Duplicated entity kind.
        kind: EntityKind,
        /// Duplicate identifier.
        id: String,
    },
    /// A referenced entity does not exist.
    #[error("{owner:?} references unknown {reference:?} {id:?}")]
    UnknownReference {
        /// Entity holding the reference.
        owner: EntityKind,
        /// Referenced entity kind.
        reference: EntityKind,
        /// Missing identifier.
        id: String,
    },
    /// A scene attempted to override the same target more than once.
    #[error("duplicate {kind:?} target {id:?}")]
    DuplicateTarget {
        /// Entity containing the duplicate target.
        kind: EntityKind,
        /// Target description.
        id: String,
    },
    /// A value is not finite or falls outside the normalized range.
    #[error("normalized value {value} must be finite and within 0.0 through 1.0")]
    InvalidNormalizedValue {
        /// Invalid value.
        value: f32,
    },
    /// A gain is not finite or falls outside the supported scene range.
    #[error("gain {value} dB must be finite and within -120.0 through 24.0")]
    InvalidGain {
        /// Invalid gain.
        value: f32,
    },
    /// A channel layout declares an unsupported channel count.
    #[error("channel count {channels} must be between 1 and 64")]
    InvalidChannelCount {
        /// Invalid channel count.
        channels: u8,
    },
    /// MIDI channel is not in the one-based range 1 through 16.
    #[error("MIDI channel {channel} must be between 1 and 16")]
    InvalidMidiChannel {
        /// Invalid MIDI channel.
        channel: u8,
    },
    /// MIDI controller is not in the range 0 through 127.
    #[error("MIDI controller {controller} must be between 0 and 127")]
    InvalidMidiController {
        /// Invalid controller number.
        controller: u8,
    },
    /// MIDI mapping range is inverted.
    #[error("MIDI range minimum {minimum} exceeds maximum {maximum}")]
    InvalidMidiRange {
        /// Lower endpoint.
        minimum: f32,
        /// Upper endpoint.
        maximum: f32,
    },
}

fn validate_capacity(
    collection: Collection,
    found: usize,
    capacity: usize,
) -> Result<(), ValidationError> {
    if found > capacity {
        return Err(ValidationError::CapacityExceeded {
            collection,
            capacity,
            found,
        });
    }
    Ok(())
}

fn validate_identifier(value: &str, kind: EntityKind) -> Result<(), ValidationError> {
    if value.trim().is_empty() {
        return Err(ValidationError::EmptyIdentifier { kind });
    }
    Ok(())
}

fn validate_rack_reference(
    id: &RackId,
    rack_ids: &BTreeSet<&RackId>,
    owner: EntityKind,
) -> Result<(), ValidationError> {
    if !rack_ids.contains(id) {
        return Err(ValidationError::UnknownReference {
            owner,
            reference: EntityKind::Rack,
            id: id.0.clone(),
        });
    }
    Ok(())
}

fn resolve_slot<'a>(
    rack_id: &RackId,
    slot_id: &PluginInstanceId,
    racks: &'a [Rack],
    owner: EntityKind,
) -> Result<&'a PluginSlot, ValidationError> {
    let Some(rack) = racks.iter().find(|rack| rack.id == *rack_id) else {
        return Err(ValidationError::UnknownReference {
            owner,
            reference: EntityKind::Rack,
            id: rack_id.0.clone(),
        });
    };
    rack.slots
        .iter()
        .find(|slot| slot.id == *slot_id)
        .ok_or_else(|| ValidationError::UnknownReference {
            owner,
            reference: EntityKind::PluginSlot,
            id: slot_id.0.clone(),
        })
}

fn validate_parameter_reference(
    slot: &PluginSlot,
    parameter_id: &ParameterId,
    owner: EntityKind,
) -> Result<(), ValidationError> {
    if !slot.parameters.values.contains_key(parameter_id) {
        return Err(ValidationError::UnknownReference {
            owner,
            reference: EntityKind::Parameter,
            id: parameter_id.0.clone(),
        });
    }
    Ok(())
}
