//! Real VST3 SDK hosting boundary backed by `vst3-host`.
//!
//! All `vst3-host` / low-level VST3 types stay inside this module. Helper binaries enable
//! `--features sdk`; the main application and engine must never depend on this crate's `sdk`
//! feature.

use std::path::Path;

use vst3_host::audio::AudioBuffers;
use vst3_host::discovery::get_detailed_plugin_info;
use vst3_host::midi::MidiChannel;
use vst3_host::simple;

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

    /// Loads a bundle with an explicit sample rate and block size.
    ///
    /// Prefer [`SDK_MAX_FRAMES`] so smaller realtime blocks avoid per-call buffer allocation.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError`] when the bundle cannot be loaded or activated.
    pub fn load_with_format(
        bundle: &Vst3BundlePath,
        sample_rate_hz: f64,
        block_size: usize,
    ) -> Result<Self, SdkError> {
        if block_size == 0 || block_size > SDK_MAX_FRAMES {
            return Err(SdkError::Host(format!(
                "block size {block_size} must be in 1..={SDK_MAX_FRAMES}"
            )));
        }
        let path = bundle.as_path();
        if !path.exists() {
            return Err(SdkError::InvalidBundle(path.display().to_string()));
        }
        let mut plugin = simple::load_plugin_with_settings(path, sample_rate_hz, block_size)?;
        plugin.start_processing()?;
        let output_channels = plugin.output_channel_count().max(1);
        let input_channels = output_channels.clamp(1, 2);
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
}

fn midi_channel(channel: u8) -> Result<MidiChannel, SdkError> {
    MidiChannel::from_index(channel).ok_or_else(|| {
        SdkError::Host(format!(
            "MIDI channel {channel} is outside the valid 0..=15 range"
        ))
    })
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
