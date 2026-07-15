//! Product [`AudioEndpoint`] implementation over the Phase 1 `CoreAudio` harness.

use sp_audio_io::{AudioDeviceId, AudioDeviceInfo, AudioEndpoint, AudioFormat, AudioFormatError};
use std::{io, time::Duration};

use sp_engine::{PreparedGraph, RealtimeRackMixer};
use sp_shared_memory_macos::SharedMemoryRegion;

use crate::{
    ActiveOutput, CoreAudioError, InterleavedStereoF32, PhaseOneConfig, PhaseOneFrames,
    PhaseOneRenderer, RenderDisposition,
};

/// Renderer that owns the product mixer for the `CoreAudio` callback.
///
/// Configure the mixer on the control thread before [`MacOsAudioEndpoint::start`]. While the
/// stream is running the mixer is exclusively borrowed by the realtime callback.
pub struct ProductRenderer {
    mixer: RealtimeRackMixer,
    input: [f32; 512],
    dispatcher: Option<crate::RackSharedMemoryDispatcher>,
}

impl ProductRenderer {
    /// Creates a renderer for an initial graph.
    #[must_use]
    pub fn new(graph: PreparedGraph) -> Self {
        Self {
            mixer: RealtimeRackMixer::new(graph),
            input: [0.0; 512],
            dispatcher: None,
        }
    }

    /// Creates a renderer with control-plane-created shared-memory banks, one per rack.
    ///
    /// The mappings and all dispatcher buffers are installed before the callback starts.
    ///
    /// # Errors
    ///
    /// Returns an error when the bank count exceeds the fixed rack capacity or the macOS
    /// monotonic clock cannot be initialized.
    pub fn with_rack_banks(
        graph: PreparedGraph,
        banks: Vec<SharedMemoryRegion>,
    ) -> io::Result<Self> {
        Ok(Self {
            mixer: RealtimeRackMixer::new(graph),
            input: [0.0; 512],
            dispatcher: Some(crate::RackSharedMemoryDispatcher::new(banks)?),
        })
    }

    /// Borrows the mixer for control-thread configuration before start.
    pub fn mixer_mut(&mut self) -> &mut RealtimeRackMixer {
        &mut self.mixer
    }
}

impl PhaseOneRenderer for ProductRenderer {
    fn render(&mut self, mut output: InterleavedStereoF32<'_>) -> RenderDisposition {
        let frames = output.frames().as_u32() as usize;
        let samples = frames.saturating_mul(2);
        if samples > self.input.len() {
            output.samples_mut().fill(0.0);
            return RenderDisposition::Silence;
        }
        let input = &self.input[..samples];
        if let Some(dispatcher) = self.dispatcher.as_mut() {
            let deadline = Duration::from_nanos(
                u64::try_from(frames)
                    .unwrap_or(u64::MAX)
                    .saturating_mul(1_000_000_000)
                    .saturating_mul(3)
                    / (48_000 * 4),
            );
            dispatcher.process_block(input, frames, deadline);
            dispatcher.apply_to_mixer(&mut self.mixer);
            let sources = dispatcher.sources();
            if self
                .mixer
                .render_block(input, &sources, output.samples_mut(), frames)
                .is_ok()
            {
                return RenderDisposition::Rendered;
            }
        } else if self
            .mixer
            .render_block(input, &[], output.samples_mut(), frames)
            .is_ok()
        {
            return RenderDisposition::Rendered;
        }
        output.samples_mut().fill(0.0);
        RenderDisposition::Silence
    }
}

/// macOS product audio endpoint: 48 kHz stereo, 128/256-frame `CoreAudio` output.
pub struct MacOsAudioEndpoint {
    pending_renderer: Option<ProductRenderer>,
    output: Option<ActiveOutput<ProductRenderer>>,
    active_format: Option<AudioFormat>,
    allow_device_reconfiguration: bool,
}

impl MacOsAudioEndpoint {
    /// Creates a stopped endpoint with an empty graph.
    #[must_use]
    pub fn new() -> Self {
        Self::with_renderer(ProductRenderer::new(PreparedGraph::empty()))
    }

    /// Creates a stopped endpoint with a caller-configured renderer.
    #[must_use]
    pub fn with_renderer(renderer: ProductRenderer) -> Self {
        Self {
            pending_renderer: Some(renderer),
            output: None,
            active_format: None,
            allow_device_reconfiguration: false,
        }
    }

    /// Permits requesting the fixed product sample rate / buffer size on the device.
    #[must_use]
    pub const fn allow_device_reconfiguration(mut self) -> Self {
        self.allow_device_reconfiguration = true;
        self
    }

    /// Configures the pending mixer while the endpoint is stopped.
    ///
    /// # Errors
    ///
    /// Returns an error when the stream is already running.
    pub fn with_pending_mixer_mut<R>(
        &mut self,
        configure: impl FnOnce(&mut RealtimeRackMixer) -> R,
    ) -> Result<R, Box<dyn std::error::Error + Send + Sync>> {
        let renderer = self
            .pending_renderer
            .as_mut()
            .ok_or("audio endpoint is running; stop before configuring the mixer")?;
        Ok(configure(renderer.mixer_mut()))
    }
}

impl Default for MacOsAudioEndpoint {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioEndpoint for MacOsAudioEndpoint {
    fn enumerate_outputs(
        &self,
    ) -> Result<Vec<AudioDeviceInfo>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(vec![AudioDeviceInfo {
            id: AudioDeviceId::new("default-output"),
            name: "Default Output".to_owned(),
            max_output_channels: 2,
        }])
    }

    fn start(
        &mut self,
        format: AudioFormat,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let format = format
            .validate()
            .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { Box::new(error) })?;
        if !format.is_product_format() {
            return Err(Box::new(AudioFormatError::UnsupportedProductFrames {
                frames: format.max_frames_per_callback,
            }));
        }
        if self.output.is_some() {
            self.stop()?;
        }
        let frames = match format.max_frames_per_callback {
            128 => PhaseOneFrames::Frames128,
            256 => PhaseOneFrames::Frames256,
            other => {
                return Err(Box::new(AudioFormatError::UnsupportedProductFrames {
                    frames: other,
                }));
            }
        };
        let mut config = PhaseOneConfig::new(frames);
        if self.allow_device_reconfiguration {
            config = config.allow_device_reconfiguration();
        }
        let renderer = self
            .pending_renderer
            .take()
            .unwrap_or_else(|| ProductRenderer::new(PreparedGraph::empty()));
        let output = ActiveOutput::start(config, renderer).map_err(core_audio_box)?;
        self.output = Some(output);
        self.active_format = Some(format);
        Ok(())
    }

    fn stop(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if let Some(mut output) = self.output.take() {
            output.stop().map_err(core_audio_box)?;
            // Renderer is dropped with ActiveOutput; restore a fresh pending mixer.
            self.pending_renderer = Some(ProductRenderer::new(PreparedGraph::empty()));
        }
        self.active_format = None;
        Ok(())
    }

    fn active_format(&self) -> Option<AudioFormat> {
        self.active_format
    }
}

fn core_audio_box(error: CoreAudioError) -> Box<dyn std::error::Error + Send + Sync> {
    Box::new(error)
}

#[cfg(test)]
mod tests {
    use sp_audio_io::{AudioEndpoint, AudioFormat};

    use super::MacOsAudioEndpoint;

    #[test]
    fn enumerates_default_output() {
        let endpoint = MacOsAudioEndpoint::new();
        let devices = endpoint.enumerate_outputs().expect("enumerate");
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].name, "Default Output");
    }

    #[test]
    fn rejects_non_product_format_without_starting_device() {
        let mut endpoint = MacOsAudioEndpoint::new();
        let error = endpoint
            .start(AudioFormat {
                sample_rate_hz: 44_100,
                channel_count: 2,
                max_frames_per_callback: 128,
            })
            .expect_err("must reject");
        assert!(!error.to_string().is_empty());
        assert!(endpoint.active_format().is_none());
    }
}
