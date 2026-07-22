//! Real VST3 SDK hosting boundary backed by `vst3-host`.
//!
//! All `vst3-host` / low-level VST3 types stay inside this module. Helper binaries enable
//! `--features sdk`; the main application and engine must never depend on this crate's `sdk`
//! feature.

use std::path::Path;

use vst3_host::audio::{
    AudioBuffers, BusArrangements, BusDirection, MediaType, SpeakerArrangement,
};
use vst3_host::discovery::get_detailed_plugin_info;
use vst3_host::midi::{MidiChannel, MidiEvent};
use vst3_host::plugin::{ParameterEditKind, WindowHandle};
use vst3_host::simple;

use sp_model::{
    PluginBusConfiguration, PluginBusMetadata, PluginClassScanMetadata, PluginIdentity,
    PluginParameterMetadata,
};

use super::adapter::{
    AdapterNotification, BusDirection as RackBusDirection, BusRole, MainBusLayout, MidiMessage,
    OutputChange, OutputChangeSink, ParameterFlags, ParameterGesture, ParameterGestureAction,
    PluginProcessBlock, ProcessingFormat, RackPluginAdapter, TimedMidiMessage, Vst3BusDescriptor,
    Vst3BusTopology, Vst3ClassDescriptor, Vst3ClassSelection, Vst3ParameterInfo, Vst3StateStreams,
};
use super::{Vst3BundlePath, Vst3PluginDescriptor};

/// Matches [`sp_protocol::MAX_FRAMES`] without pulling that crate into the adapter.
pub const SDK_MAX_FRAMES: usize = 256;

/// Errors produced while loading or processing through the VST3 SDK.
#[derive(Debug)]
pub enum SdkError {
    /// Bundle path was rejected before native load.
    InvalidBundle(String),
    /// Native hosting failed.
    Host(String),
}

impl std::fmt::Display for SdkError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidBundle(path) => write!(formatter, "invalid VST3 bundle: {path}"),
            Self::Host(detail) => write!(formatter, "VST3 host error: {detail}"),
        }
    }
}

impl std::error::Error for SdkError {}

impl From<vst3_host::Error> for SdkError {
    fn from(error: vst3_host::Error) -> Self {
        Self::Host(error.to_string())
    }
}

/// Helper-side factory for real SDK-backed plug-in instances.
pub trait SdkPluginFactory {
    /// Loads descriptors for a validated bundle path.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when the bundle cannot be opened or enumerated.
    fn enumerate(&self, bundle: &Vst3BundlePath) -> Result<Vec<Vst3PluginDescriptor>, SdkError>;
}

/// `vst3-host`-backed factory used by isolated scanner/worker helpers.
#[derive(Clone, Copy, Debug, Default)]
pub struct HostSdkFactory;

impl SdkPluginFactory for HostSdkFactory {
    fn enumerate(&self, bundle: &Vst3BundlePath) -> Result<Vec<Vst3PluginDescriptor>, SdkError> {
        let path = bundle.as_path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("vst3") {
            return Err(SdkError::InvalidBundle(path.display().to_string()));
        }
        let detailed = get_detailed_plugin_info(path)?;
        let vendor = if detailed.factory.vendor.is_empty() {
            detailed.info.vendor.clone()
        } else {
            detailed.factory.vendor.clone()
        };
        if detailed.classes.is_empty() {
            return Ok(vec![Vst3PluginDescriptor {
                class_id: detailed.info.uid.clone(),
                name: detailed.info.name.clone(),
                vendor,
            }]);
        }
        Ok(detailed
            .classes
            .into_iter()
            .map(|class| Vst3PluginDescriptor {
                class_id: class.class_id,
                name: if class.name.is_empty() {
                    detailed.info.name.clone()
                } else {
                    class.name
                },
                vendor: vendor.clone(),
            })
            .collect())
    }
}

/// Normalized parameter metadata exposed without leaking SDK types.
#[derive(Clone, Debug, PartialEq)]
pub struct SdkParameterInfo {
    /// Host-facing parameter identifier.
    pub id: u32,
    /// Display name.
    pub name: String,
    /// Current normalized value in `0.0..=1.0`.
    pub normalized: f64,
    /// Default normalized value supplied by the controller.
    pub default_normalized: f64,
    /// Controller-provided unit string.
    pub unit: String,
    /// Number of discrete intervals; zero is continuous.
    pub step_count: i32,
    /// Complete raw VST3 parameter flags.
    pub flags: u32,
    /// Whether the host may automate the parameter.
    pub can_automate: bool,
    /// Whether the parameter is read-only.
    pub is_read_only: bool,
    /// Whether the parameter is a bypass control.
    pub is_bypass: bool,
}

/// Fixed pixel size for a worker-owned native VST3 editor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Vst3EditorSize {
    /// Width in physical pixels.
    pub width: u32,
    /// Height in physical pixels.
    pub height: u32,
}

impl Vst3EditorSize {
    /// Converts a controller-supplied signed size into a validated public size.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when either supplied dimension is non-positive.
    pub fn from_sdk(width: i32, height: i32) -> Result<Self, SdkError> {
        let width = u32::try_from(width)
            .ok()
            .filter(|width| *width > 0)
            .ok_or_else(|| SdkError::Host(format!("invalid VST3 editor width {width}")))?;
        let height = u32::try_from(height)
            .ok()
            .filter(|height| *height > 0)
            .ok_or_else(|| SdkError::Host(format!("invalid VST3 editor height {height}")))?;
        Ok(Self { width, height })
    }
}

/// SDK-free editor capability and current worker-owned editor state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Vst3EditorMetadata {
    /// Whether the controller advertises a native editor.
    pub supported: bool,
    /// Current native editor size when one is available.
    pub preferred_size: Option<Vst3EditorSize>,
}

/// Comprehensive scanner-facing metadata for one selected VST3 class.
#[derive(Clone, Debug, PartialEq)]
pub struct Vst3ScanMetadata {
    /// Selected class identifier.
    pub class_id: String,
    /// Plug-in display name.
    pub name: String,
    /// Plug-in vendor.
    pub vendor: String,
    /// Plug-in version supplied by the component.
    pub version: String,
    /// Selected audio-module category.
    pub category: String,
    /// Discovered main/aux audio inputs before layout negotiation.
    pub audio_inputs: Vec<Vst3BusDescriptor>,
    /// Discovered main/aux audio outputs before layout negotiation.
    pub audio_outputs: Vec<Vst3BusDescriptor>,
    /// Discovered event inputs before layout negotiation.
    pub event_inputs: Vec<Vst3BusDescriptor>,
    /// Discovered event outputs before layout negotiation.
    pub event_outputs: Vec<Vst3BusDescriptor>,
    /// Complete normalized controller parameter metadata.
    pub parameters: Vec<Vst3ParameterInfo>,
    /// Native editor capability and preferred size.
    pub editor: Vst3EditorMetadata,
}

/// Opaque, processable VST3 instance owned by a helper process.
pub struct SdkPlugin {
    plugin: vst3_host::Plugin,
    buffers: AudioBuffers,
    sample_rate_hz: f64,
    block_size: usize,
}

impl SdkPlugin {
    /// Loads a bundle at 48 kHz with [`SDK_MAX_FRAMES`] as the maximum block size.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when the bundle cannot be loaded or activated.
    pub fn load(bundle: &Vst3BundlePath) -> Result<Self, SdkError> {
        Self::load_with_format(bundle, 48_000.0, SDK_MAX_FRAMES)
    }

    /// Loads, configures, and starts a bundle with an explicit sample rate and block size.
    ///
    /// This legacy convenience method preserves the Phase 1 worker contract. New fixed-rack
    /// code uses [`Self::load_inactive_with_format`] and lets the rack enforce bus activation
    /// and `setProcessing` ordering.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when the bundle cannot be loaded or activated.
    pub fn load_with_format(
        bundle: &Vst3BundlePath,
        sample_rate_hz: f64,
        block_size: usize,
    ) -> Result<Self, SdkError> {
        let mut instance = Self::load_inactive_with_format(bundle, sample_rate_hz, block_size)?;
        instance.start_processing()?;
        Ok(instance)
    }

    /// Loads a bundle but leaves the component inactive.
    ///
    /// The returned instance has preallocated planar buffers and is ready for inactive bus
    /// negotiation. Only the fixed serial rack should call this API, because it owns the VST3
    /// activate/start/stop ordering.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when the bundle cannot be loaded or the requested fixed format is
    /// invalid.
    pub fn load_inactive_with_format(
        bundle: &Vst3BundlePath,
        sample_rate_hz: f64,
        block_size: usize,
    ) -> Result<Self, SdkError> {
        if !(sample_rate_hz.is_finite() && sample_rate_hz > 0.0)
            || block_size == 0
            || block_size > SDK_MAX_FRAMES
        {
            return Err(SdkError::Host(format!(
                "format {sample_rate_hz} Hz / block size {block_size} frames must be finite and in 1..={SDK_MAX_FRAMES}"
            )));
        }
        let path = bundle.as_path();
        if !path.exists() {
            return Err(SdkError::InvalidBundle(path.display().to_string()));
        }
        let plugin = simple::load_plugin_with_settings(path, sample_rate_hz, block_size)?;
        let output_channels = plugin.output_channel_count().clamp(1, 2);
        let input_channels = plugin
            .bus_arrangements()
            .ok()
            .and_then(|buses| buses.inputs.first().copied())
            .map_or(0, SpeakerArrangement::channel_count)
            .clamp(0, 2);
        let buffers =
            AudioBuffers::new(input_channels, output_channels, block_size, sample_rate_hz);
        Ok(Self {
            plugin,
            buffers,
            sample_rate_hz,
            block_size,
        })
    }

    /// Returns the active sample rate.
    #[must_use]
    pub const fn sample_rate_hz(&self) -> f64 {
        self.sample_rate_hz
    }

    /// Returns the configured block size.
    #[must_use]
    pub const fn block_size(&self) -> usize {
        self.block_size
    }

    /// Returns reported processing latency in samples.
    #[must_use]
    pub fn latency_samples(&self) -> u32 {
        self.plugin.latency_samples()
    }

    /// Lists current parameters.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when the controller cannot be queried.
    pub fn parameters(&self) -> Result<Vec<SdkParameterInfo>, SdkError> {
        Ok(self
            .plugin
            .get_parameters()?
            .into_iter()
            .map(|parameter| SdkParameterInfo {
                id: parameter.id,
                name: parameter.name,
                normalized: parameter.value,
                default_normalized: parameter.default,
                unit: parameter.unit,
                step_count: parameter.step_count,
                flags: parameter.flags,
                can_automate: parameter.can_automate,
                is_read_only: parameter.is_read_only,
                is_bypass: parameter.is_bypass,
            })
            .collect())
    }

    /// Sets one normalized parameter.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when the parameter cannot be written.
    pub fn set_parameter(&mut self, id: u32, normalized: f64) -> Result<(), SdkError> {
        self.plugin.set_parameter(id, normalized)?;
        Ok(())
    }

    /// Queues a MIDI note-on for the next process call.
    ///
    /// `channel` is 0-based (`0..=15`).
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when the channel is invalid or the host rejects the event.
    pub fn send_note_on(&mut self, note: u8, velocity: u8, channel: u8) -> Result<(), SdkError> {
        let channel = midi_channel(channel)?;
        self.plugin
            .send_midi_note(note, velocity, channel)
            .map_err(SdkError::from)
    }

    /// Queues a MIDI note-off for the next process call.
    ///
    /// `channel` is 0-based (`0..=15`).
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when the channel is invalid or the host rejects the event.
    pub fn send_note_off(&mut self, note: u8, channel: u8) -> Result<(), SdkError> {
        let channel = midi_channel(channel)?;
        self.plugin
            .send_midi_note_off(note, channel)
            .map_err(SdkError::from)
    }

    /// Queues a MIDI continuous-controller event for the next process call.
    ///
    /// `channel` is 0-based (`0..=15`).
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when the channel is invalid or the host rejects the event.
    pub fn send_cc(&mut self, controller: u8, value: u8, channel: u8) -> Result<(), SdkError> {
        let channel = midi_channel(channel)?;
        self.plugin
            .send_midi_cc(controller, value, channel)
            .map_err(SdkError::from)
    }

    /// Captures opaque component/controller state.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when state cannot be serialized.
    pub fn save_state(&self) -> Result<Vec<u8>, SdkError> {
        Ok(self.plugin.save_state()?)
    }

    /// Restores opaque state while processing is active in the helper.
    ///
    /// Callers must keep the rack muted/bypassed during live restore; scenes must never
    /// invoke this path.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when state cannot be applied.
    pub fn load_state(&mut self, data: &[u8]) -> Result<(), SdkError> {
        self.plugin.load_state(data)?;
        Ok(())
    }

    /// Processes planar `f32` audio in place of the shared-memory block.
    ///
    /// Input channels are copied into the host buffers; completed outputs are written back.
    /// Extra output channels beyond `output` are discarded; missing channels are zeroed.
    /// Variable frame counts below the configured maximum reuse the preallocated buffers
    /// without allocating.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when the plug-in rejects the block.
    pub fn process_planar(
        &mut self,
        input: &[&[f32]],
        output: &mut [&mut [f32]],
        frames: usize,
    ) -> Result<(), SdkError> {
        if frames == 0 || frames > self.block_size {
            return Err(SdkError::Host(format!(
                "frame count {frames} exceeds configured block size {}",
                self.block_size
            )));
        }
        self.buffers.clear();
        for (channel, samples) in input.iter().enumerate() {
            if let Some(destination) = self.buffers.inputs.get_mut(channel) {
                let copy_len = frames.min(samples.len()).min(destination.len());
                destination[..copy_len].copy_from_slice(&samples[..copy_len]);
            }
        }

        // vst3-host derives numSamples from channel vector lengths; temporarily shrink
        // without reallocating so variable CoreAudio/callback sizes stay allocation-free.
        truncate_channels(&mut self.buffers.inputs, frames);
        truncate_channels(&mut self.buffers.outputs, frames);
        let process_result = self.plugin.process_audio(&mut self.buffers);
        restore_channels(&mut self.buffers.inputs, self.block_size);
        restore_channels(&mut self.buffers.outputs, self.block_size);
        process_result?;

        for (channel, destination) in output.iter_mut().enumerate() {
            if let Some(source) = self.buffers.outputs.get(channel) {
                let copy_len = frames.min(destination.len()).min(source.len());
                destination[..copy_len].copy_from_slice(&source[..copy_len]);
                let zero_end = frames.min(destination.len());
                if copy_len < zero_end {
                    destination[copy_len..zero_end].fill(0.0);
                }
            } else {
                let fill = frames.min(destination.len());
                destination[..fill].fill(0.0);
            }
        }
        Ok(())
    }

    /// Convenience: process interleaved stereo into interleaved stereo.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when channel/frame counts are incompatible or processing fails.
    pub fn process_interleaved_stereo(
        &mut self,
        input: &[f32],
        output: &mut [f32],
        frames: usize,
    ) -> Result<(), SdkError> {
        if input.len() < frames.saturating_mul(2) || output.len() < frames.saturating_mul(2) {
            return Err(SdkError::Host(
                "interleaved stereo buffers are shorter than frame count".to_owned(),
            ));
        }
        let mut left_in = [0.0_f32; SDK_MAX_FRAMES];
        let mut right_in = [0.0_f32; SDK_MAX_FRAMES];
        let mut left_out = [0.0_f32; SDK_MAX_FRAMES];
        let mut right_out = [0.0_f32; SDK_MAX_FRAMES];
        for frame in 0..frames {
            left_in[frame] = input[frame * 2];
            right_in[frame] = input[frame * 2 + 1];
        }
        {
            let input_planes: [&[f32]; 2] = [&left_in[..frames], &right_in[..frames]];
            let mut output_planes: [&mut [f32]; 2] =
                [&mut left_out[..frames], &mut right_out[..frames]];
            self.process_planar(&input_planes, &mut output_planes, frames)?;
        }
        for frame in 0..frames {
            output[frame * 2] = left_out[frame];
            output[frame * 2 + 1] = right_out[frame];
        }
        Ok(())
    }

    /// Starts VST3 processing after inactive bus preparation.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when the component rejects `setProcessing`.
    pub fn start_processing(&mut self) -> Result<(), SdkError> {
        self.plugin.start_processing()?;
        Ok(())
    }

    /// Stops VST3 processing before state restoration or bus changes.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when the component rejects its stop sequence.
    pub fn stop_processing(&mut self) -> Result<(), SdkError> {
        self.plugin.stop_processing()?;
        Ok(())
    }

    /// Returns whether the component is currently processing.
    #[must_use]
    pub fn is_processing(&self) -> bool {
        self.plugin.is_processing()
    }

    /// Returns the instantiated VST3 component class identifier.
    #[must_use]
    pub fn class_id(&self) -> &str {
        &self.plugin.info().uid
    }

    /// Returns whether the selected controller exposes a native editor.
    #[must_use]
    pub fn has_editor(&self) -> bool {
        self.plugin.has_editor()
    }

    /// Returns SDK-free native-editor capability and its preferred size.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] if an advertised editor returns an invalid preferred size.
    pub fn editor_metadata(&self) -> Result<Vst3EditorMetadata, SdkError> {
        if !self.has_editor() {
            return Ok(Vst3EditorMetadata {
                supported: false,
                preferred_size: None,
            });
        }
        let (width, height) = self.plugin.get_editor_size()?;
        Ok(Vst3EditorMetadata {
            supported: true,
            preferred_size: Some(Vst3EditorSize::from_sdk(width, height)?),
        })
    }

    /// Opens the worker-owned native editor in a native parent view.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when the editor is unavailable or rejects attachment.
    pub fn open_editor(
        &mut self,
        parent_view: *mut std::ffi::c_void,
    ) -> Result<Vst3EditorSize, SdkError> {
        if !self.plugin.has_editor() {
            return Err(SdkError::Host(
                "selected VST3 class has no native editor".to_owned(),
            ));
        }
        #[cfg(target_os = "macos")]
        let parent_window = WindowHandle::from_nsview(parent_view);
        #[cfg(not(target_os = "macos"))]
        let parent_window = {
            let _ = parent_view;
            return Err(SdkError::Host(
                "native VST3 editor hosting is currently macOS-only".to_owned(),
            ));
        };
        self.plugin.open_editor(parent_window)?;
        let (width, height) = self.plugin.get_editor_size()?;
        Vst3EditorSize::from_sdk(width, height)
    }

    /// Closes the worker-owned native editor.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when the controller rejects editor teardown.
    pub fn close_editor(&mut self) -> Result<(), SdkError> {
        self.plugin.close_editor()?;
        Ok(())
    }

    /// Returns a plug-in-initiated native editor resize request, if any.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when the plug-in reports an invalid size.
    pub fn take_editor_resize_request(&self) -> Result<Option<Vst3EditorSize>, SdkError> {
        self.plugin
            .take_editor_resize_request()
            .map(|(width, height)| Vst3EditorSize::from_sdk(width, height))
            .transpose()
    }

    /// Returns the component's current audio bus arrangements.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] if the host layer cannot query arrangements.
    pub fn bus_arrangements(&self) -> Result<BusArrangements, SdkError> {
        Ok(self.plugin.bus_arrangements()?)
    }

    /// Returns the current component bus topology without exposing VST3 SDK types.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when the host layer cannot query bus arrangements.
    pub fn bus_topology(&self) -> Result<Vst3BusTopology, SdkError> {
        let arrangements = self.bus_arrangements()?;
        let audio_inputs = arrangements
            .inputs
            .iter()
            .map(|arrangement| {
                Vst3BusDescriptor::audio(
                    RackBusDirection::Input,
                    BusRole::Main,
                    u8::try_from(arrangement.channel_count()).unwrap_or(u8::MAX),
                )
            })
            .collect();
        let audio_outputs = arrangements
            .outputs
            .iter()
            .map(|arrangement| {
                Vst3BusDescriptor::audio(
                    RackBusDirection::Output,
                    BusRole::Main,
                    u8::try_from(arrangement.channel_count()).unwrap_or(u8::MAX),
                )
            })
            .collect();
        let info = self.plugin.info();
        let event_inputs = if info.has_midi_input {
            vec![Vst3BusDescriptor::event(
                RackBusDirection::Input,
                BusRole::Main,
            )]
        } else {
            Vec::new()
        };
        let event_outputs = if info.has_midi_output {
            vec![Vst3BusDescriptor::event(
                RackBusDirection::Output,
                BusRole::Main,
            )]
        } else {
            Vec::new()
        };
        Ok(Vst3BusTopology {
            audio_inputs,
            audio_outputs,
            event_inputs,
            event_outputs,
        })
    }

    /// Reads comprehensive SDK-free metadata for this selected, inactive component class.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when unsupported buses, parameter metadata, or editor metadata cannot
    /// be read.
    pub fn scan_metadata(&self, class: &Vst3ClassDescriptor) -> Result<Vst3ScanMetadata, SdkError> {
        if self.class_id() != class.class_id {
            return Err(SdkError::Host(format!(
                "loaded class {} does not match requested class {}",
                self.class_id(),
                class.class_id
            )));
        }
        let topology = self.bus_topology()?;
        validate_scanner_topology(&topology)?;
        let parameters = self
            .parameters()?
            .into_iter()
            .map(|parameter| Vst3ParameterInfo {
                id: parameter.id,
                short_title: parameter.name.clone(),
                title: parameter.name,
                unit: parameter.unit,
                normalized: parameter.normalized,
                default_normalized: parameter.default_normalized,
                step_count: parameter.step_count,
                flags: ParameterFlags(parameter.flags),
            })
            .collect();
        let info = self.plugin.info();
        Ok(Vst3ScanMetadata {
            class_id: class.class_id.clone(),
            name: info.name.clone(),
            vendor: info.vendor.clone(),
            version: if class.version.is_empty() {
                info.version.clone()
            } else {
                class.version.clone()
            },
            category: class.category.clone(),
            audio_inputs: topology.audio_inputs,
            audio_outputs: topology.audio_outputs,
            event_inputs: topology.event_inputs,
            event_outputs: topology.event_outputs,
            parameters,
            editor: self.editor_metadata()?,
        })
    }

    /// Negotiates main audio bus arrangements while inactive and rebuilds planar storage.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when the VST3 component rejects inactive configuration.
    pub fn set_bus_arrangements(
        &mut self,
        inputs: &[SpeakerArrangement],
        outputs: &[SpeakerArrangement],
    ) -> Result<(), SdkError> {
        self.plugin.set_bus_arrangements(inputs, outputs)?;
        let arrangements = self.plugin.bus_arrangements()?;
        let input_channels = arrangements
            .inputs
            .first()
            .map_or(0, |arrangement| arrangement.channel_count())
            .clamp(0, 2);
        let output_channels = arrangements
            .outputs
            .first()
            .map_or(0, |arrangement| arrangement.channel_count())
            .clamp(1, 2);
        self.buffers = AudioBuffers::new(
            input_channels,
            output_channels,
            self.block_size,
            self.sample_rate_hz,
        );
        Ok(())
    }

    /// Activates or deactivates a discovered VST3 bus while inactive.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when bus activation is rejected.
    pub fn set_bus_active(
        &mut self,
        media: MediaType,
        direction: BusDirection,
        index: i32,
        active: bool,
    ) -> Result<(), SdkError> {
        self.plugin
            .set_bus_active(media, direction, index, active)?;
        Ok(())
    }

    /// Formats a normalized parameter value through the plug-in controller.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] if the parameter is unknown or formatting fails.
    pub fn format_parameter(&self, id: u32, normalized: f64) -> Result<String, SdkError> {
        Ok(self.plugin.format_parameter(id, normalized)?)
    }

    /// Queues a sample-accurate normalized parameter change for the next process call.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when the parameter value or offset is rejected.
    pub fn set_parameter_at(
        &mut self,
        id: u32,
        normalized: f64,
        sample_offset: u16,
    ) -> Result<(), SdkError> {
        self.plugin
            .set_parameter_at(id, normalized, i32::from(sample_offset))?;
        Ok(())
    }

    /// Queues a sample-accurate SDK-free MIDI 1.0 message for the next process call.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] for invalid channels or a plug-in rejection.
    pub fn send_midi_at(
        &mut self,
        message: MidiMessage,
        sample_offset: u16,
    ) -> Result<(), SdkError> {
        let event = match message {
            MidiMessage::NoteOn {
                channel,
                note,
                velocity,
            } => MidiEvent::NoteOn {
                channel: midi_channel(channel)?,
                note,
                velocity,
            },
            MidiMessage::NoteOff {
                channel,
                note,
                velocity,
            } => MidiEvent::NoteOff {
                channel: midi_channel(channel)?,
                note,
                velocity,
            },
            MidiMessage::ControlChange {
                channel,
                controller,
                value,
            } => MidiEvent::ControlChange {
                channel: midi_channel(channel)?,
                controller,
                value,
            },
            MidiMessage::PitchBend { channel, value } => MidiEvent::PitchBend {
                channel: midi_channel(channel)?,
                value,
            },
            MidiMessage::ChannelPressure { channel, pressure } => MidiEvent::ChannelAftertouch {
                channel: midi_channel(channel)?,
                pressure,
            },
            MidiMessage::ProgramChange { channel, program } => MidiEvent::ProgramChange {
                channel: midi_channel(channel)?,
                program,
            },
        };
        self.plugin
            .send_midi_event_at(event, i32::from(sample_offset))?;
        Ok(())
    }

    /// Drains SDK MIDI output and converts it to SDK-free bounded events.
    pub fn drain_output_midi(&mut self, output: &mut dyn OutputChangeSink) {
        for event in self.plugin.take_output_midi() {
            let Some(message) = from_sdk_midi(event) else {
                continue;
            };
            let _ = output.push(OutputChange::Midi(TimedMidiMessage {
                message,
                sample_offset: 0,
            }));
        }
    }

    /// Drains SDK controller gestures and converts them to SDK-free ordered changes.
    pub fn drain_parameter_gestures(&mut self, output: &mut dyn OutputChangeSink) {
        for edit in self.plugin.take_parameter_edits() {
            let (action, normalized) = match edit.kind {
                ParameterEditKind::BeginGesture => (ParameterGestureAction::Begin, None),
                ParameterEditKind::ValueChange => (ParameterGestureAction::Value, edit.value),
                ParameterEditKind::EndGesture => (ParameterGestureAction::End, None),
            };
            let _ = output.push(OutputChange::Gesture(ParameterGesture {
                parameter_id: edit.id,
                action,
                normalized,
            }));
        }
    }
}

fn midi_channel(channel: u8) -> Result<MidiChannel, SdkError> {
    MidiChannel::from_index(channel).ok_or_else(|| {
        SdkError::Host(format!(
            "MIDI channel {channel} is outside the valid 0..=15 range"
        ))
    })
}

fn from_sdk_midi(event: MidiEvent) -> Option<MidiMessage> {
    match event {
        MidiEvent::NoteOn {
            channel,
            note,
            velocity,
        } => Some(MidiMessage::NoteOn {
            channel: channel.as_index(),
            note,
            velocity,
        }),
        MidiEvent::NoteOff {
            channel,
            note,
            velocity,
        } => Some(MidiMessage::NoteOff {
            channel: channel.as_index(),
            note,
            velocity,
        }),
        MidiEvent::ControlChange {
            channel,
            controller,
            value,
        } => Some(MidiMessage::ControlChange {
            channel: channel.as_index(),
            controller,
            value,
        }),
        MidiEvent::PitchBend { channel, value } => Some(MidiMessage::PitchBend {
            channel: channel.as_index(),
            value,
        }),
        MidiEvent::ChannelAftertouch { channel, pressure } => Some(MidiMessage::ChannelPressure {
            channel: channel.as_index(),
            pressure,
        }),
        MidiEvent::ProgramChange { channel, program } => Some(MidiMessage::ProgramChange {
            channel: channel.as_index(),
            program,
        }),
        _ => None,
    }
}

fn validate_scanner_topology(topology: &Vst3BusTopology) -> Result<(), SdkError> {
    let input_channels = topology
        .audio_inputs
        .first()
        .map_or(0, |bus| bus.channel_count);
    let output_channels = topology
        .audio_outputs
        .first()
        .map_or(0, |bus| bus.channel_count);
    let requested = MainBusLayout::new(
        input_channels,
        output_channels,
        !topology.event_inputs.is_empty(),
    )
    .map_err(|error| SdkError::Host(format!("unsupported VST3 bus layout: {error}")))?;
    topology
        .validate_fixed(requested)
        .map_err(|error| SdkError::Host(format!("unsupported VST3 bus layout: {error}")))
}

fn truncate_channels(channels: &mut [Vec<f32>], frames: usize) {
    for channel in channels {
        if channel.len() > frames {
            channel.truncate(frames);
        }
    }
}

fn restore_channels(channels: &mut [Vec<f32>], block_size: usize) {
    for channel in channels {
        if channel.len() < block_size {
            // Capacity remains `block_size` after truncate, so this does not allocate.
            channel.resize(block_size, 0.0);
        }
    }
}

/// Factory for a real helper-owned VST3 rack adapter.
///
/// It enumerates module classes before instantiation. The factory and returned adapter are only
/// available behind the helper-only `sdk` feature, so application/engine code never observes SDK
/// objects or accidentally loads a plug-in.
#[derive(Clone, Copy, Debug, Default)]
pub struct HostSdkRackFactory;

impl HostSdkRackFactory {
    /// Enumerates audio-module classes from one VST3 bundle.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when isolated helper-side module enumeration fails.
    pub fn enumerate_classes(
        &self,
        bundle: &Vst3BundlePath,
    ) -> Result<Vec<Vst3ClassDescriptor>, SdkError> {
        if bundle
            .as_path()
            .extension()
            .and_then(|extension| extension.to_str())
            != Some("vst3")
        {
            return Err(SdkError::InvalidBundle(
                bundle.as_path().display().to_string(),
            ));
        }
        let detailed = get_detailed_plugin_info(bundle.as_path())?;
        let classes = detailed
            .classes
            .into_iter()
            .filter(|class| class.category.contains("Audio Module Class"))
            .map(|class| Vst3ClassDescriptor {
                class_id: class.class_id,
                name: class.name,
                category: class.category,
                version: class.version,
            })
            .collect();
        Ok(classes)
    }

    /// Describes one explicitly selected class with comprehensive SDK-free metadata.
    ///
    /// This helper-side operation loads the selected component without starting processing. It
    /// rejects bus counts/channel counts outside the fixed rack contract rather than returning a
    /// metadata record that a worker could never activate.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when the selection is absent, the host loads a different class, or
    /// the class exposes unsupported bus, parameter, or editor metadata.
    pub fn describe_class(
        &self,
        selection: &Vst3ClassSelection,
        format: ProcessingFormat,
    ) -> Result<Vst3ScanMetadata, SdkError> {
        let class = self
            .enumerate_classes(&selection.bundle)?
            .into_iter()
            .find(|class| class.class_id == selection.class_id)
            .ok_or_else(|| {
                SdkError::Host(format!(
                    "selected class {} was not found in {}",
                    selection.class_id,
                    selection.bundle.as_path().display()
                ))
            })?;
        let plugin = SdkPlugin::load_inactive_with_format(
            &selection.bundle,
            format.sample_rate_hz,
            format.maximum_frames,
        )?;
        plugin.scan_metadata(&class)
    }

    /// Scans one selected class into the scanner's SDK-free model metadata.
    ///
    /// This method may load the selected class and therefore is restricted to the isolated
    /// scanner helper. It does not start processing or open an editor.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when class selection, component/controller initialization, bus
    /// discovery, parameter metadata, or editor metadata cannot be read.
    pub fn scan_class_metadata(
        &self,
        selection: &Vst3ClassSelection,
        format: ProcessingFormat,
    ) -> Result<PluginClassScanMetadata, SdkError> {
        let class = self
            .enumerate_classes(&selection.bundle)?
            .into_iter()
            .find(|class| class.class_id == selection.class_id)
            .ok_or_else(|| {
                SdkError::Host(format!(
                    "selected class {} was not found in {}",
                    selection.class_id,
                    selection.bundle.as_path().display()
                ))
            })?;
        let mut adapter = self.create_adapter();
        adapter.select_module_class(selection, format)?;
        adapter.initialize_component()?;
        adapter.initialize_controller()?;
        adapter.connect_component_controller()?;
        let buses = adapter.discover_buses()?;
        validate_scanner_topology(&buses)?;
        let parameters = adapter.parameters()?;
        let editor = adapter.editor_metadata()?;
        let info = adapter.plugin()?.plugin.info();
        Ok(PluginClassScanMetadata {
            identity: PluginIdentity {
                vendor: info.vendor.clone(),
                name: info.name.clone(),
                unique_id: selection.class_id.clone(),
            },
            version: if class.version.is_empty() {
                info.version.clone()
            } else {
                class.version
            },
            buses: plugin_bus_configuration(&buses),
            parameters: parameters.iter().map(plugin_parameter_metadata).collect(),
            editor_supported: editor.supported,
        })
    }

    /// Creates an uninitialized adapter. The fixed rack invokes class selection during inactive
    /// preparation, preserving component/controller lifecycle ordering.
    #[must_use]
    pub const fn create_adapter(&self) -> HostSdkRackAdapter {
        HostSdkRackAdapter::new()
    }
}

/// Real `vst3-host` implementation of the fixed-rack adapter contract.
///
/// `vst3-host` owns the low-level VST3 component/controller connections. This wrapper imposes
/// Superposition's stricter fixed bus and lifecycle contract around it, converts all SDK objects
/// at the boundary, and retains only preallocated planar buffers in the process path.
pub struct HostSdkRackAdapter {
    plugin: Option<SdkPlugin>,
    selected: Option<Vst3ClassSelection>,
    lifecycle: HostLifecycle,
    layout: Option<MainBusLayout>,
    editor: Option<EditorSession>,
    last_latency_samples: Option<u32>,
    pending_restart_flags: Option<u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EditorSession {
    size: Vst3EditorSize,
    focused: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HostLifecycle {
    Empty,
    ClassSelected,
    ComponentInitialized,
    ControllerInitialized,
    Connected,
    BusesNegotiated,
    BusesActivated,
    Processing,
}

impl HostSdkRackAdapter {
    /// Creates an adapter with no loaded module.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            plugin: None,
            selected: None,
            lifecycle: HostLifecycle::Empty,
            layout: None,
            editor: None,
            last_latency_samples: None,
            pending_restart_flags: None,
        }
    }

    /// Records an SDK restart request for the bounded worker control handoff.
    ///
    /// The current `vst3-host` surface does not publish `IComponentHandler::restartComponent`
    /// directly. A low-level backend can call this same adapter seam when it receives that
    /// callback; the fixed rack will then drain it as [`AdapterNotification::RestartRequested`].
    pub fn report_restart_requested(&mut self, flags: u32) {
        self.pending_restart_flags = Some(
            self.pending_restart_flags
                .map_or(flags, |pending| pending | flags),
        );
    }

    /// Sets one normalized controller parameter without exposing SDK parameter types.
    ///
    /// The change is queued for the next process block when the adapter is processing.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] for a non-finite/out-of-range value, an unknown parameter, or an
    /// unloaded class.
    pub fn set_parameter(&mut self, parameter_id: u32, normalized: f64) -> Result<(), SdkError> {
        if !(normalized.is_finite() && (0.0..=1.0).contains(&normalized)) {
            return Err(SdkError::Host(format!(
                "normalized parameter value {normalized} is outside 0.0..=1.0"
            )));
        }
        self.plugin_mut()?.set_parameter(parameter_id, normalized)
    }

    /// Returns one controller parameter's complete metadata.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when the parameter is unknown or the controller cannot be queried.
    pub fn parameter_metadata(&mut self, parameter_id: u32) -> Result<Vst3ParameterInfo, SdkError> {
        self.parameters()?
            .into_iter()
            .find(|parameter| parameter.id == parameter_id)
            .ok_or_else(|| SdkError::Host(format!("unknown VST3 parameter {parameter_id}")))
    }

    /// Reads one current normalized controller parameter value.
    ///
    /// # Errors
    /// Returns [`SdkError`] when the parameter is unknown.
    pub fn read_parameter(&mut self, parameter_id: u32) -> Result<f64, SdkError> {
        Ok(self.parameter_metadata(parameter_id)?.normalized)
    }

    /// Returns an explicit capability failure because `vst3-host` does not surface host-initiated
    /// begin/end edit calls.
    ///
    /// # Errors
    /// Always returns [`SdkError::Host`]; the capability is unavailable.
    pub fn parameter_gesture(&mut self, _parameter_id: u32, _begin: bool) -> Result<(), SdkError> {
        Err(SdkError::Host(
            "vst3-host does not expose host-initiated IComponentHandler beginEdit/endEdit"
                .to_owned(),
        ))
    }

    /// Returns native-editor capability and preferred size without opening an editor.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when the selected class cannot query its editor geometry.
    pub fn editor_metadata(&self) -> Result<Vst3EditorMetadata, SdkError> {
        let plugin = self.plugin()?;
        if !plugin.has_editor() {
            return Ok(Vst3EditorMetadata {
                supported: false,
                preferred_size: None,
            });
        }
        let (width, height) = plugin.plugin.get_editor_size()?;
        Ok(Vst3EditorMetadata {
            supported: true,
            preferred_size: Some(Vst3EditorSize::from_sdk(width, height)?),
        })
    }

    /// Opens the plug-in editor in a worker-owned native parent view.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when no class/editor is available or attachment fails.
    pub fn open_editor(
        &mut self,
        parent_view: *mut std::ffi::c_void,
    ) -> Result<Vst3EditorSize, SdkError> {
        if self.editor.is_some() {
            return Err(SdkError::Host("VST3 editor is already open".to_owned()));
        }
        let size = self.plugin_mut()?.open_editor(parent_view)?;
        self.editor = Some(EditorSession {
            size,
            focused: false,
        });
        Ok(size)
    }

    /// Records a focus transition after the worker's owning native window applies it.
    ///
    /// The helper owns the `AppKit` window and applies platform focus first; this adapter keeps the
    /// plug-in editor lifecycle state synchronized without exposing an `NSView` or SDK pointer.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when no editor is open.
    pub fn focus_editor(&mut self, focused: bool) -> Result<(), SdkError> {
        let session = self
            .editor
            .as_mut()
            .ok_or_else(|| SdkError::Host("VST3 editor is not open".to_owned()))?;
        session.focused = focused;
        Ok(())
    }

    /// Records the size applied to the worker-owned native editor window.
    ///
    /// The worker resizes its native container before invoking this method. A plug-in requested
    /// size is obtained through [`Self::take_editor_resize_request`], preventing cross-process
    /// view embedding or SDK objects from escaping the helper.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] for a zero dimension or when no editor is open.
    pub fn resize_editor(&mut self, size: Vst3EditorSize) -> Result<(), SdkError> {
        if size.width == 0 || size.height == 0 {
            return Err(SdkError::Host(
                "VST3 editor size must be nonzero".to_owned(),
            ));
        }
        let session = self
            .editor
            .as_mut()
            .ok_or_else(|| SdkError::Host("VST3 editor is not open".to_owned()))?;
        session.size = size;
        Ok(())
    }

    /// Drains one plug-in initiated editor resize request.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] for an invalid plug-in requested geometry or an unloaded class.
    pub fn take_editor_resize_request(&mut self) -> Result<Option<Vst3EditorSize>, SdkError> {
        let requested = self.plugin()?.take_editor_resize_request()?;
        if let Some(size) = requested
            && let Some(session) = self.editor.as_mut()
        {
            session.size = size;
        }
        Ok(requested)
    }

    /// Closes the worker-owned native editor before the hosting window is destroyed.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when no editor is open or controller teardown fails.
    pub fn close_editor(&mut self) -> Result<(), SdkError> {
        if self.editor.is_none() {
            return Err(SdkError::Host("VST3 editor is not open".to_owned()));
        }
        self.plugin_mut()?.close_editor()?;
        self.editor = None;
        Ok(())
    }

    /// Captures distinct component/controller state streams while processing is inactive.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when the plug-in is active or an opaque stream cannot be captured.
    pub fn capture_state_streams(&mut self) -> Result<Vst3StateStreams, SdkError> {
        <Self as RackPluginAdapter>::capture_state(self)
    }

    /// Restores state while inactive in component, controller synchronization, then controller
    /// state order. The method never starts processing implicitly.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when processing is active or one official restore step fails.
    pub fn restore_state_streams(&mut self, state: &Vst3StateStreams) -> Result<(), SdkError> {
        if self.lifecycle == HostLifecycle::Processing {
            return Err(SdkError::Host(
                "opaque state restore requires stopped VST3 processing".to_owned(),
            ));
        }
        <Self as RackPluginAdapter>::restore_component_state(self, &state.component)?;
        <Self as RackPluginAdapter>::synchronize_controller_from_component_state(self)?;
        <Self as RackPluginAdapter>::restore_controller_state(self, &state.controller)
    }

    fn plugin(&self) -> Result<&SdkPlugin, SdkError> {
        self.plugin
            .as_ref()
            .ok_or_else(|| SdkError::Host("VST3 class has not been selected".to_owned()))
    }

    fn plugin_mut(&mut self) -> Result<&mut SdkPlugin, SdkError> {
        self.plugin
            .as_mut()
            .ok_or_else(|| SdkError::Host("VST3 class has not been selected".to_owned()))
    }

    fn require_lifecycle(&self, expected: HostLifecycle) -> Result<(), SdkError> {
        if self.lifecycle == expected {
            return Ok(());
        }
        Err(SdkError::Host(format!(
            "invalid VST3 lifecycle transition from {:?}; expected {:?}",
            self.lifecycle, expected
        )))
    }

    fn topology(&self) -> Result<Vst3BusTopology, SdkError> {
        let plugin = self.plugin()?;
        let arrangements = plugin.bus_arrangements()?;
        let audio_inputs = arrangements
            .inputs
            .iter()
            .map(|arrangement| {
                Vst3BusDescriptor::audio(
                    RackBusDirection::Input,
                    BusRole::Main,
                    u8::try_from(arrangement.channel_count()).unwrap_or(u8::MAX),
                )
            })
            .collect();
        let audio_outputs = arrangements
            .outputs
            .iter()
            .map(|arrangement| {
                Vst3BusDescriptor::audio(
                    RackBusDirection::Output,
                    BusRole::Main,
                    u8::try_from(arrangement.channel_count()).unwrap_or(u8::MAX),
                )
            })
            .collect();
        let info = plugin.plugin.info();
        let event_inputs = if info.has_midi_input {
            vec![Vst3BusDescriptor::event(
                RackBusDirection::Input,
                BusRole::Main,
            )]
        } else {
            Vec::new()
        };
        let event_outputs = if info.has_midi_output {
            vec![Vst3BusDescriptor::event(
                RackBusDirection::Output,
                BusRole::Main,
            )]
        } else {
            Vec::new()
        };
        Ok(Vst3BusTopology {
            audio_inputs,
            audio_outputs,
            event_inputs,
            event_outputs,
        })
    }
}

impl Default for HostSdkRackAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl RackPluginAdapter for HostSdkRackAdapter {
    type Error = SdkError;

    fn select_module_class(
        &mut self,
        selection: &Vst3ClassSelection,
        format: ProcessingFormat,
    ) -> Result<(), Self::Error> {
        self.require_lifecycle(HostLifecycle::Empty)?;
        if format.maximum_frames > SDK_MAX_FRAMES {
            return Err(SdkError::Host(format!(
                "rack format exceeds SDK fixed buffer capacity {SDK_MAX_FRAMES}"
            )));
        }
        let classes = HostSdkRackFactory.enumerate_classes(&selection.bundle)?;
        if !classes
            .iter()
            .any(|class| class.class_id == selection.class_id)
        {
            return Err(SdkError::Host(format!(
                "class {} is not an audio module in {}",
                selection.class_id,
                selection.bundle.as_path().display()
            )));
        }
        let plugin = SdkPlugin::load_inactive_with_format(
            &selection.bundle,
            format.sample_rate_hz,
            format.maximum_frames,
        )?;
        if plugin.class_id() != selection.class_id {
            return Err(SdkError::Host(format!(
                "vst3-host instantiated class {}, not selected class {}; class-specific loading requires the low-level backend",
                plugin.class_id(),
                selection.class_id
            )));
        }
        self.last_latency_samples = Some(plugin.latency_samples());
        self.plugin = Some(plugin);
        self.selected = Some(selection.clone());
        self.lifecycle = HostLifecycle::ClassSelected;
        Ok(())
    }

    fn initialize_component(&mut self) -> Result<(), Self::Error> {
        self.require_lifecycle(HostLifecycle::ClassSelected)?;
        self.plugin()?;
        self.lifecycle = HostLifecycle::ComponentInitialized;
        Ok(())
    }

    fn initialize_controller(&mut self) -> Result<(), Self::Error> {
        self.require_lifecycle(HostLifecycle::ComponentInitialized)?;
        self.plugin()?;
        self.lifecycle = HostLifecycle::ControllerInitialized;
        Ok(())
    }

    fn connect_component_controller(&mut self) -> Result<(), Self::Error> {
        self.require_lifecycle(HostLifecycle::ControllerInitialized)?;
        self.plugin()?;
        self.lifecycle = HostLifecycle::Connected;
        Ok(())
    }

    fn discover_buses(&mut self) -> Result<Vst3BusTopology, Self::Error> {
        match self.lifecycle {
            HostLifecycle::Connected
            | HostLifecycle::BusesNegotiated
            | HostLifecycle::BusesActivated => self.topology(),
            state => Err(SdkError::Host(format!(
                "cannot discover VST3 buses in lifecycle state {state:?}"
            ))),
        }
    }

    fn negotiate_main_buses(&mut self, layout: MainBusLayout) -> Result<(), Self::Error> {
        self.require_lifecycle(HostLifecycle::Connected)?;
        let discovered = self.topology()?;
        let inputs = match discovered.audio_inputs.len() {
            0 if layout.input_channels == 0 => Vec::new(),
            1 => vec![speaker_arrangement(layout.input_channels)],
            _ => {
                return Err(SdkError::Host(
                    "main-bus negotiation requires at most one audio input".to_owned(),
                ));
            }
        };
        if discovered.audio_outputs.len() != 1 {
            return Err(SdkError::Host(
                "main-bus negotiation requires exactly one audio output".to_owned(),
            ));
        }
        self.plugin_mut()?
            .set_bus_arrangements(&inputs, &[speaker_arrangement(layout.output_channels)])?;
        self.layout = Some(layout);
        self.lifecycle = HostLifecycle::BusesNegotiated;
        Ok(())
    }

    fn activate_main_buses(&mut self, layout: MainBusLayout) -> Result<(), Self::Error> {
        self.require_lifecycle(HostLifecycle::BusesNegotiated)?;
        let discovered = self.topology()?;
        if layout.input_channels > 0 {
            self.plugin_mut()?
                .set_bus_active(MediaType::Audio, BusDirection::Input, 0, true)?;
        }
        self.plugin_mut()?
            .set_bus_active(MediaType::Audio, BusDirection::Output, 0, true)?;
        if layout.event_input_active && !discovered.event_inputs.is_empty() {
            self.plugin_mut()?
                .set_bus_active(MediaType::Event, BusDirection::Input, 0, true)?;
        }
        self.lifecycle = HostLifecycle::BusesActivated;
        Ok(())
    }

    fn start_processing(&mut self) -> Result<(), Self::Error> {
        self.require_lifecycle(HostLifecycle::BusesActivated)?;
        self.plugin_mut()?.start_processing()?;
        self.lifecycle = HostLifecycle::Processing;
        Ok(())
    }

    fn stop_processing(&mut self) -> Result<(), Self::Error> {
        match self.lifecycle {
            HostLifecycle::Processing => {
                self.plugin_mut()?.stop_processing()?;
                // `setProcessing(false)` leaves the component active with the accepted bus
                // arrangement. Keep the adapter at the real pre-start boundary so a later
                // inactive control operation can be followed by `setProcessing(true)` again.
                self.lifecycle = HostLifecycle::BusesActivated;
                Ok(())
            }
            HostLifecycle::BusesActivated => Ok(()),
            state => Err(SdkError::Host(format!(
                "cannot stop VST3 processing in lifecycle state {state:?}"
            ))),
        }
    }

    fn process(&mut self, block: PluginProcessBlock<'_>) -> Result<(), Self::Error> {
        self.require_lifecycle(HostLifecycle::Processing)?;
        let PluginProcessBlock {
            frames,
            input,
            output,
            midi,
            parameter_changes,
            output_changes,
        } = block;
        if frames == 0 || frames > self.plugin()?.block_size() {
            return Err(SdkError::Host(format!(
                "process frame count {frames} exceeds prepared capacity"
            )));
        }
        for change in parameter_changes.iter() {
            self.plugin_mut()?.set_parameter_at(
                change.parameter_id,
                change.normalized,
                change.sample_offset,
            )?;
        }
        for event in midi.iter() {
            self.plugin_mut()?
                .send_midi_at(event.message, event.sample_offset)?;
        }
        let (input_planes, input_channels) = input.into_parts();
        let (mut output_planes, output_channels) = output.into_parts();
        self.plugin_mut()?.process_planar(
            &input_planes[..usize::from(input_channels)],
            &mut output_planes[..usize::from(output_channels)],
            frames,
        )?;
        self.plugin_mut()?.drain_output_midi(output_changes);
        self.plugin_mut()?.drain_parameter_gestures(output_changes);
        let latency = self.plugin()?.latency_samples();
        if self
            .last_latency_samples
            .replace(latency)
            .is_some_and(|previous| previous != latency)
        {
            let _ = output_changes.push(OutputChange::Notification(
                AdapterNotification::LatencyChanged { samples: latency },
            ));
        }
        Ok(())
    }

    fn parameters(&mut self) -> Result<Vec<Vst3ParameterInfo>, Self::Error> {
        let parameters = self.plugin()?.parameters()?;
        Ok(parameters
            .into_iter()
            .map(|parameter| Vst3ParameterInfo {
                id: parameter.id,
                short_title: parameter.name.clone(),
                title: parameter.name,
                unit: parameter.unit,
                normalized: parameter.normalized,
                default_normalized: parameter.default_normalized,
                step_count: parameter.step_count,
                flags: ParameterFlags(parameter.flags),
            })
            .collect())
    }

    fn format_parameter(
        &mut self,
        parameter_id: u32,
        normalized: f64,
    ) -> Result<String, Self::Error> {
        self.plugin()?.format_parameter(parameter_id, normalized)
    }

    fn capture_state(&mut self) -> Result<Vst3StateStreams, Self::Error> {
        if self.lifecycle == HostLifecycle::Processing {
            return Err(SdkError::Host(
                "opaque state capture requires stopped VST3 processing".to_owned(),
            ));
        }
        // `vst3-host` exposes IComponent::getState but not IEditController::getState. Returning
        // an empty controller stream would falsely claim a complete two-stream snapshot, so the
        // high-level backend rejects this capability until a low-level VST3 backend supplies it.
        Err(SdkError::Host(
            "vst3-host cannot independently capture IEditController state; controller-specific state is unsupported".to_owned(),
        ))
    }

    fn restore_component_state(&mut self, component: &[u8]) -> Result<(), Self::Error> {
        if self.lifecycle == HostLifecycle::Processing {
            return Err(SdkError::Host(
                "component state restore requires stopped VST3 processing".to_owned(),
            ));
        }
        self.plugin_mut()?.load_state(component)
    }

    fn synchronize_controller_from_component_state(&mut self) -> Result<(), Self::Error> {
        if self.lifecycle == HostLifecycle::Processing {
            return Err(SdkError::Host(
                "controller synchronization requires stopped VST3 processing".to_owned(),
            ));
        }
        // `SdkPlugin::load_state` performs the component -> controller synchronization exposed by
        // vst3-host. The explicit method retains the official ordering in the SDK-free contract.
        Ok(())
    }

    fn restore_controller_state(&mut self, controller: &[u8]) -> Result<(), Self::Error> {
        if self.lifecycle == HostLifecycle::Processing {
            return Err(SdkError::Host(
                "controller state restore requires stopped VST3 processing".to_owned(),
            ));
        }
        if controller.is_empty() {
            return Ok(());
        }
        Err(SdkError::Host(
            "vst3-host does not expose IEditController::setState; use the low-level adapter for controller-specific state"
                .to_owned(),
        ))
    }

    fn latency_samples(&mut self) -> Result<u32, Self::Error> {
        Ok(self.plugin()?.latency_samples())
    }

    fn drain_notifications(
        &mut self,
        output: &mut dyn OutputChangeSink,
    ) -> Result<(), Self::Error> {
        if let Some(flags) = self.pending_restart_flags.take() {
            let _ = output.push(OutputChange::Notification(
                AdapterNotification::RestartRequested { flags },
            ));
        }
        Ok(())
    }
}

fn plugin_bus_configuration(topology: &Vst3BusTopology) -> PluginBusConfiguration {
    let inputs = topology
        .audio_inputs
        .iter()
        .chain(&topology.event_inputs)
        .enumerate()
        .map(|(index, bus)| PluginBusMetadata {
            index: u32::try_from(index).unwrap_or(u32::MAX),
            channels: bus.channel_count,
            main: bus.role == BusRole::Main,
            event: matches!(bus.media, super::adapter::BusMedia::Event),
        })
        .collect();
    let outputs = topology
        .audio_outputs
        .iter()
        .chain(&topology.event_outputs)
        .enumerate()
        .map(|(index, bus)| PluginBusMetadata {
            index: u32::try_from(index).unwrap_or(u32::MAX),
            channels: bus.channel_count,
            main: bus.role == BusRole::Main,
            event: matches!(bus.media, super::adapter::BusMedia::Event),
        })
        .collect();
    PluginBusConfiguration { inputs, outputs }
}

fn plugin_parameter_metadata(parameter: &Vst3ParameterInfo) -> PluginParameterMetadata {
    PluginParameterMetadata {
        id: parameter.id,
        name: parameter.title.clone(),
        short_name: parameter.short_title.clone(),
        unit: parameter.unit.clone(),
        default_normalized: parameter
            .default_normalized
            .is_finite()
            .then_some(parameter.default_normalized),
        automatable: parameter.flags.contains(ParameterFlags::CAN_AUTOMATE),
        read_only: parameter.flags.contains(ParameterFlags::READ_ONLY),
        bypass: parameter.flags.contains(ParameterFlags::BYPASS),
    }
}

fn speaker_arrangement(channels: u8) -> SpeakerArrangement {
    match channels {
        1 => SpeakerArrangement::MONO,
        2 => SpeakerArrangement::STEREO,
        _ => SpeakerArrangement::EMPTY,
    }
}

/// Returns whether `path` looks like a loadable VST3 bundle on disk.
#[must_use]
pub fn bundle_exists(path: &Path) -> bool {
    path.extension().and_then(|extension| extension.to_str()) == Some("vst3") && path.exists()
}

#[cfg(test)]
mod tests {
    use super::{HostSdkFactory, SDK_MAX_FRAMES, SdkPlugin, SdkPluginFactory};
    use crate::Vst3BundlePath;
    use std::env;
    use std::path::PathBuf;

    #[test]
    fn rejects_non_vst3_extension() {
        let error = HostSdkFactory
            .enumerate(&Vst3BundlePath::new("/tmp/not-a-plugin.dylib"))
            .expect_err("extension must be validated");
        assert!(error.to_string().contains("invalid VST3 bundle"));
    }

    #[test]
    fn rejects_missing_bundle_on_load() {
        let error = SdkPlugin::load(&Vst3BundlePath::new("/tmp/definitely-missing.vst3"))
            .err()
            .expect("missing bundle must fail");
        assert!(error.to_string().contains("invalid VST3 bundle"));
    }

    #[test]
    fn rejects_oversized_block_size() {
        let error = SdkPlugin::load_with_format(
            &Vst3BundlePath::new("/tmp/definitely-missing.vst3"),
            48_000.0,
            SDK_MAX_FRAMES + 1,
        )
        .err()
        .expect("oversized block must fail before filesystem access matters");
        assert!(error.to_string().contains("block size"));
    }

    #[test]
    fn loads_env_bundle_when_configured() {
        let Ok(path) = env::var("SUPERPOSITION_TEST_VST3") else {
            return;
        };
        let path = PathBuf::from(path);
        if !path.exists() {
            return;
        }
        let mut plugin =
            SdkPlugin::load(&Vst3BundlePath::new(path)).expect("env bundle should load");
        assert_eq!(plugin.block_size(), SDK_MAX_FRAMES);
        let left = [0.1_f32; 128];
        let right = [0.2_f32; 128];
        let input: [&[f32]; 2] = [&left, &right];
        let mut output_left = [0.0_f32; 128];
        let mut output_right = [0.0_f32; 128];
        let mut output: [&mut [f32]; 2] = [&mut output_left, &mut output_right];
        plugin
            .process_planar(&input, &mut output, 128)
            .expect("process should succeed");
        let _ = plugin.parameters();
        let state = plugin.save_state().expect("state capture should succeed");
        plugin
            .load_state(&state)
            .expect("state restore should succeed");
    }
}
