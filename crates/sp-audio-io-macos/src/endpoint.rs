//! macOS product audio endpoint over the direct AUHAL streams in this crate.

use sp_audio_io::{
    AudioDeviceInfo, AudioEndpoint, AudioEndpointEvent, AudioFormat, AudioFormatError,
    AudioRouteConfig, BlockFrames, CallbackTelemetry, ProductRenderer, StoppedRecoveryAccess,
};
use sp_engine::{PreparedGraph, RealtimeRackMixer};

use crate::{
    ActiveMultiChannelDuplex, ActiveOutput, CoreAudioError, OutputConfig, enumerate_devices,
};

/// The AUHAL streams run at 48 kHz only.
const PRODUCT_SAMPLE_RATE: u32 = 48_000;

/// macOS product audio endpoint: 48 kHz, 1–64 physical output channels.
pub struct MacOsAudioEndpoint {
    pending_renderer: Option<Box<ProductRenderer>>,
    output: Option<ActiveOutput<ProductRenderer>>,
    duplex: Option<ActiveMultiChannelDuplex<ProductRenderer>>,
    active_format: Option<AudioFormat>,
    selected_route: Option<AudioRouteConfig>,
    retirement_failed: bool,
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
            pending_renderer: Some(Box::new(renderer)),
            output: None,
            duplex: None,
            active_format: None,
            selected_route: None,
            retirement_failed: false,
            allow_device_reconfiguration: false,
        }
    }

    /// Permits requesting the fixed product sample rate / buffer size on the device.
    #[must_use]
    pub const fn allow_device_reconfiguration(mut self) -> Self {
        self.allow_device_reconfiguration = true;
        self
    }

    /// Returns the explicitly selected duplex route while the endpoint is running.
    #[must_use]
    pub fn selected_route(&self) -> Option<&AudioRouteConfig> {
        self.selected_route.as_ref()
    }

    /// Whether native cleanup failed and this endpoint must be replaced.
    ///
    /// A faulted endpoint has no safely usable active format. Native storage may still be
    /// alive; it is retained instead of risking a callback into freed renderer memory.
    #[must_use]
    pub const fn is_faulted(&self) -> bool {
        self.retirement_failed
    }

    /// Services worker bank handoffs without audio after proven callback retirement.
    ///
    /// Returns a borrowed proof of the recovery identities still owned by the retained dispatcher.
    /// A caller may handle unattached workers separately, but must not acknowledge owned handoffs.
    /// Returns `None` while audio is active or native cleanup is uncertain. This never renders
    /// a block, starts a device, or discards the retained graph and control receiver.
    pub fn service_stopped_recoveries(&mut self) -> Option<StoppedRecoveryAccess<'_>> {
        if self.retirement_failed || self.output.is_some() || self.duplex.is_some() {
            return None;
        }
        Some(self.pending_renderer.as_mut()?.stopped_recovery_access())
    }

    /// Returns coherent callback counters while a stream is running.
    #[must_use]
    pub fn callback_telemetry(&self) -> Option<CallbackTelemetry> {
        self.duplex
            .as_ref()
            .map(ActiveMultiChannelDuplex::telemetry)
            .or_else(|| self.output.as_ref().map(ActiveOutput::telemetry))
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
        Ok(enumerate_devices()?
            .into_iter()
            .filter_map(|device| {
                (device.capabilities.max_output_channels > 0).then_some(device.info)
            })
            .collect())
    }

    fn enumerate_inputs(
        &self,
    ) -> Result<Vec<AudioDeviceInfo>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(enumerate_devices()?
            .into_iter()
            .filter_map(|device| {
                (device.capabilities.max_input_channels > 0).then_some(device.info)
            })
            .collect())
    }

    fn start_route(
        &mut self,
        route: AudioRouteConfig,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let format = route.format.validate()?;
        if format.sample_rate_hz != PRODUCT_SAMPLE_RATE {
            return Err(format!("product audio requires {PRODUCT_SAMPLE_RATE} Hz").into());
        }
        if format.channel_count > 64 {
            return Err(format!(
                "product audio supports at most 64 output channels; requested {}",
                format.channel_count
            )
            .into());
        }
        if !matches!(format.max_frames_per_callback, 32 | 64 | 128 | 256) {
            return Err(Box::new(AudioFormatError::UnsupportedProductFrames {
                frames: format.max_frames_per_callback,
            }));
        }
        let devices = enumerate_devices().map_err(core_audio_box)?;
        let output_device = devices
            .iter()
            .find(|device| device.info.id == route.output)
            .ok_or_else(|| format!("selected output device {} was not found", route.output))?;
        if format.channel_count > output_device.capabilities.max_output_channels {
            return Err(format!(
                "selected output device {} has {} channels, but {} were requested",
                route.output, output_device.capabilities.max_output_channels, format.channel_count
            )
            .into());
        }
        let input_channels = if let Some(input_id) = &route.input {
            devices
                .iter()
                .find(|device| &device.info.id == input_id)
                .ok_or_else(|| format!("selected input device {input_id} was not found"))?
                .capabilities
                .max_input_channels
                .min(64)
        } else {
            0
        };
        if let Some(renderer) = self.pending_renderer.as_ref() {
            renderer.validate_routes(
                usize::from(input_channels),
                usize::from(format.channel_count),
            )?;
        }
        self.stop()?;
        let pending = self.pending_renderer.as_ref().ok_or(
            "audio renderer could not be safely recovered; rebuild the endpoint before starting",
        )?;
        pending.validate_routes(
            usize::from(input_channels),
            usize::from(format.channel_count),
        )?;
        let renderer = self
            .pending_renderer
            .take()
            .expect("pending renderer validated");
        let duplex = match ActiveMultiChannelDuplex::start_boxed(
            route.clone(),
            renderer,
            self.allow_device_reconfiguration,
        ) {
            Ok(duplex) => duplex,
            Err((error, renderer)) => {
                self.retirement_failed = renderer.is_none();
                self.pending_renderer = renderer;
                return Err(core_audio_box(error));
            }
        };
        self.active_format = Some(format);
        self.selected_route = Some(route);
        self.duplex = Some(duplex);
        Ok(())
    }

    fn poll_event(&mut self) -> Option<AudioEndpointEvent> {
        self.duplex
            .as_mut()
            .and_then(ActiveMultiChannelDuplex::poll_event)
    }

    fn start(
        &mut self,
        format: AudioFormat,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let format = format
            .validate()
            .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { Box::new(error) })?;
        // The AUHAL streams run at 48 kHz only; 96 kHz sessions are Linux/Tunnel first.
        if !format.is_product_format() || format.sample_rate_hz != PRODUCT_SAMPLE_RATE {
            return Err(Box::new(AudioFormatError::UnsupportedProductFrames {
                frames: format.max_frames_per_callback,
            }));
        }
        if self.output.is_some() || self.duplex.is_some() {
            self.stop()?;
        }
        let frames = match format.max_frames_per_callback {
            32 => BlockFrames::Frames32,
            64 => BlockFrames::Frames64,
            128 => BlockFrames::Frames128,
            256 => BlockFrames::Frames256,
            other => {
                return Err(Box::new(AudioFormatError::UnsupportedProductFrames {
                    frames: other,
                }));
            }
        };
        let mut config = OutputConfig::new(frames);
        if self.allow_device_reconfiguration {
            config = config.allow_device_reconfiguration();
        }
        let renderer = self.pending_renderer.take().ok_or(
            "audio renderer could not be safely recovered; rebuild the endpoint before starting",
        )?;
        let output = match ActiveOutput::start_boxed(config, renderer) {
            Ok(output) => output,
            Err((error, renderer)) => {
                self.retirement_failed = renderer.is_none();
                self.pending_renderer = renderer;
                return Err(core_audio_box(error));
            }
        };
        self.output = Some(output);
        self.active_format = Some(format);
        Ok(())
    }

    fn stop(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if self.retirement_failed {
            return Err("native audio cleanup failed; this endpoint cannot be reused".into());
        }
        self.active_format = None;
        self.selected_route = None;
        if let Some(mut duplex) = self.duplex.take() {
            match duplex.stop_and_take_renderer() {
                Ok(renderer) => self.pending_renderer = renderer,
                Err(error) => {
                    self.retirement_failed = true;
                    return Err(core_audio_box(error));
                }
            }
        }
        if let Some(mut output) = self.output.take() {
            match output.stop_and_take_renderer() {
                Ok(renderer) => self.pending_renderer = renderer,
                Err(error) => {
                    self.retirement_failed = true;
                    return Err(core_audio_box(error));
                }
            }
        }
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
    use sp_audio_io::{AudioEndpoint, AudioFormat, ProductControl, ProductRenderer};
    use sp_engine::PreparedGraph;

    use super::MacOsAudioEndpoint;

    #[test]
    fn endpoint_stop_preserves_renderer_state_and_control_channel() {
        let (mut control, receiver) = ProductControl::new();
        let (_mapping_publisher, mapping_receiver) = sp_midi::MidiMappingPublisher::new();
        let mut renderer = ProductRenderer::new(PreparedGraph::empty()).with_live_control(
            receiver,
            mapping_receiver,
            sp_midi::MidiLearnTable::default(),
            Vec::new(),
            Vec::new(),
        );
        renderer.mixer_mut().set_rack_gain(0, 0.4);
        let output = crate::ActiveOutput::start_test(
            crate::OutputConfig::new(crate::BlockFrames::Frames64),
            renderer,
            crate::TestFakeConfig::new(crate::BlockFrames::Frames64),
            std::sync::Arc::new(crate::TestHook::new()),
        )
        .expect("fake output starts");
        let pointer = std::ptr::from_ref(output.renderer.as_ref().unwrap().as_ref());
        let mut endpoint = MacOsAudioEndpoint::new();
        endpoint.pending_renderer = None;
        endpoint.output = Some(output);
        endpoint.active_format = Some(AudioFormat::product_stereo(64).unwrap());
        endpoint.stop().expect("native callback retires");
        assert!(endpoint.active_format().is_none());
        let access = endpoint
            .service_stopped_recoveries()
            .expect("retirement proof");
        assert!(!access.owns(
            0,
            &std::sync::Arc::new(sp_shared_memory_os::RackRecoverySignal::new())
        ));
        let renderer = endpoint
            .pending_renderer
            .as_mut()
            .expect("original renderer returned");
        assert_eq!(std::ptr::from_ref(renderer.as_ref()), pointer);
        assert_eq!(
            renderer
                .mixer_mut()
                .rack_settings(0)
                .unwrap()
                .gain
                .to_bits(),
            0.4_f32.to_bits()
        );
        // The retained renderer still owns the control receiver.
        assert!(control.set_rack_gain(0, 0.8));
        endpoint
            .stop()
            .expect("repeated stop preserves pending renderer");
        assert_eq!(
            std::ptr::from_ref(endpoint.pending_renderer.as_ref().unwrap().as_ref()),
            pointer
        );
    }

    #[test]
    fn endpoint_failed_retirement_stays_faulted() {
        let output = crate::ActiveOutput::start_test(
            crate::OutputConfig::new(crate::BlockFrames::Frames64),
            ProductRenderer::new(PreparedGraph::empty()),
            crate::TestFakeConfig::new(crate::BlockFrames::Frames64)
                .fail_cleanup(crate::CoreAudioOperation::Stop, -70),
            std::sync::Arc::new(crate::TestHook::new()),
        )
        .expect("fake output starts");
        let mut endpoint = MacOsAudioEndpoint::new();
        endpoint.pending_renderer = None;
        endpoint.output = Some(output);
        endpoint.active_format = Some(AudioFormat::product_stereo(64).unwrap());
        assert!(endpoint.stop().is_err());
        assert!(endpoint.is_faulted());
        assert!(endpoint.service_stopped_recoveries().is_none());
        assert!(endpoint.active_format().is_none());
        assert!(endpoint.pending_renderer.is_none());
        assert!(endpoint.stop().is_err());
        assert!(
            endpoint
                .start(AudioFormat::product_stereo(64).unwrap())
                .is_err()
        );
    }

    #[test]
    fn enumerates_stereo_capable_outputs() {
        let endpoint = MacOsAudioEndpoint::new();
        // The device list depends on the host hardware, so only the stereo-output filter
        // contract is asserted: enumeration succeeds and every entry is identifiable.
        let devices = endpoint.enumerate_outputs().expect("enumerate");
        for device in devices {
            assert!(!device.name.is_empty());
        }
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
