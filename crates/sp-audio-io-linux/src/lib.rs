//! JACK product audio for Linux, through `quanta-audio-io`.
//!
//! [`LinuxAudioEndpoint`] implements [`sp_audio_io::AudioEndpoint`] with the same inherent API as
//! the macOS endpoint. JACK exposes one device, the server (`PipeWire`'s JACK library or jackd),
//! and the server owns the sample rate and quantum: a route starts only when they match the
//! requested product format. The callback interleaves JACK's planar ports into preallocated
//! frame-major buffers for the [`ProductRenderer`].
//!
//! `quanta-audio-io` keeps the callback until the stream is dropped, and dropping deactivates the
//! client before the callback is freed. The callback hands the renderer back from its `Drop`, so
//! a stopped endpoint retains its renderer just like the AUHAL endpoint. When the JACK server
//! died, `quanta-audio-io` leaks the callback instead; the renderer is then lost and the endpoint
//! latches a fault.
//!
//! On other platforms this crate is empty.
#![cfg(target_os = "linux")]

mod telemetry;

use std::{
    error::Error,
    sync::{Arc, Mutex, PoisonError},
};

use quanta_audio_io::{
    AudioBlock, Callback, Counters, DeviceId, FailureReason, MAX_CHANNELS, Stream, StreamConfig,
    StreamState,
};
use sp_audio_io::{
    AudioDeviceCapabilities, AudioDeviceId, AudioDeviceInfo, AudioEndpoint, AudioEndpointEvent,
    AudioFormat, AudioFormatError, AudioRouteConfig, BlockFrames, CallbackTelemetry,
    InterleavedStereoF32, MultiChannelDuplexF32, MultiChannelDuplexRenderer, OutputRenderer,
    PRODUCT_SAMPLE_RATES_HZ, ProductRenderer, RenderDisposition, StoppedRecoveryAccess,
};
use sp_engine::{PreparedGraph, RealtimeRackMixer};

use telemetry::{BlockOutcome, TelemetryCell};

type BoxError = Box<dyn Error + Send + Sync>;

const MAX_PRODUCT_CHANNELS: u16 = 64;
const DEVICE_ID_PREFIX: &str = "jack:";

/// A JACK server with its directional physical ports and current format.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LinuxAudioDevice {
    /// Platform-neutral visible metadata. The ID is `jack:<server name>`.
    pub info: AudioDeviceInfo,
    /// Physical capture and playback port counts.
    pub capabilities: AudioDeviceCapabilities,
    /// The server's current quantum when it is a product block size; JACK cannot change it here.
    /// Opening a stream rejects a server whose sample rate differs from the product format.
    pub supported_buffer_frames: Vec<u32>,
}

/// Lists running JACK servers. Never starts a server.
///
/// # Errors
/// Returns a `quanta-audio-io` error when no JACK server is running or discovery fails.
pub fn enumerate_devices() -> Result<Vec<LinuxAudioDevice>, quanta_audio_io::Error> {
    Ok(quanta_audio_io::enumerate_devices()?
        .into_iter()
        .filter(|device| device.id == DeviceId::Jack)
        .map(|device| {
            let clamp = |channels: u32| {
                u16::try_from(channels.min(u32::from(MAX_PRODUCT_CHANNELS)))
                    .unwrap_or(MAX_PRODUCT_CHANNELS)
            };
            let quantum = device.buffer_sizes.map(|sizes| sizes.minimum);
            LinuxAudioDevice {
                info: AudioDeviceInfo {
                    id: AudioDeviceId::new(format!("{DEVICE_ID_PREFIX}{}", device.uid)),
                    name: format!("JACK ({})", device.uid),
                    max_output_channels: clamp(device.output_channels),
                },
                capabilities: AudioDeviceCapabilities {
                    max_input_channels: clamp(device.input_channels),
                    max_output_channels: clamp(device.output_channels),
                    is_default_input: device.is_default_input,
                    is_default_output: device.is_default_output,
                },
                supported_buffer_frames: quantum
                    .filter(|frames| BlockFrames::from_u32(*frames).is_some())
                    .into_iter()
                    .collect(),
            }
        })
        .collect())
}

/// Where a stopped stream's callback hands the renderer back.
type RendererReturn = Arc<Mutex<Option<Box<ProductRenderer>>>>;

/// The JACK callback: interleaves, renders, and de-interleaves one product block.
struct ProductCallback {
    renderer: Option<Box<ProductRenderer>>,
    returned: RendererReturn,
    frames: BlockFrames,
    input_channels: u16,
    output_channels: u16,
    /// `start` drives the stereo [`OutputRenderer`] path; routes drive the duplex path.
    stereo_output: bool,
    input: Box<[f32]>,
    output: Box<[f32]>,
    telemetry: Arc<TelemetryCell>,
}

impl ProductCallback {
    fn new(
        renderer: Box<ProductRenderer>,
        returned: RendererReturn,
        frames: BlockFrames,
        input_channels: u16,
        output_channels: u16,
        stereo_output: bool,
        telemetry: Arc<TelemetryCell>,
    ) -> Self {
        let frame_count = frames.as_u32() as usize;
        Self {
            renderer: Some(renderer),
            returned,
            frames,
            input_channels,
            output_channels,
            stereo_output,
            input: vec![0.0; frame_count * usize::from(input_channels)].into_boxed_slice(),
            output: vec![0.0; frame_count * usize::from(output_channels)].into_boxed_slice(),
            telemetry,
        }
    }

    fn render(&mut self, block: &mut AudioBlock<'_>) -> BlockOutcome {
        let frames = self.frames.as_u32() as usize;
        if block.frames() != frames {
            return BlockOutcome::InvalidFrames;
        }
        if block.input_channels() != usize::from(self.input_channels)
            || block.output_channels() != usize::from(self.output_channels)
        {
            return BlockOutcome::InvalidChannels;
        }
        let input_channels = usize::from(self.input_channels);
        for channel in 0..input_channels {
            let Some(samples) = block.input(channel) else {
                return BlockOutcome::InvalidBuffers;
            };
            for (frame, sample) in samples.iter().enumerate() {
                self.input[frame * input_channels + channel] = *sample;
            }
        }
        let Some(renderer) = self.renderer.as_deref_mut() else {
            return BlockOutcome::Silenced;
        };
        let disposition = if self.stereo_output {
            match InterleavedStereoF32::new(&mut self.output, self.frames) {
                Ok(output) => OutputRenderer::render(renderer, output),
                Err(_) => return BlockOutcome::InvalidBuffers,
            }
        } else {
            match MultiChannelDuplexF32::new(
                &self.input,
                &mut self.output,
                self.input_channels,
                self.output_channels,
                self.frames,
            ) {
                Some(duplex) => MultiChannelDuplexRenderer::render(renderer, duplex),
                None => return BlockOutcome::InvalidBuffers,
            }
        };
        if disposition == RenderDisposition::Silence {
            return BlockOutcome::Silenced;
        }
        let output_channels = usize::from(self.output_channels);
        for channel in 0..output_channels {
            let Some(samples) = block.output(channel) else {
                return BlockOutcome::InvalidBuffers;
            };
            for (frame, sample) in samples.iter_mut().enumerate() {
                *sample = self.output[frame * output_channels + channel];
            }
        }
        BlockOutcome::Rendered
    }
}

impl Callback for ProductCallback {
    fn process(&mut self, mut block: AudioBlock<'_>) {
        let outcome = self.render(&mut block);
        if outcome != BlockOutcome::Rendered {
            for channel in 0..block.output_channels() {
                if let Some(samples) = block.output(channel) {
                    samples.fill(0.0);
                }
            }
        }
        self.telemetry.record(outcome, self.frames);
    }
}

impl Drop for ProductCallback {
    fn drop(&mut self) {
        // `quanta-audio-io` drops the callback on the control thread after deactivating JACK.
        if let Some(renderer) = self.renderer.take() {
            *self.returned.lock().unwrap_or_else(PoisonError::into_inner) = Some(renderer);
        }
    }
}

struct ActiveStream {
    stream: Stream,
    counters: Counters,
    returned: RendererReturn,
    telemetry: Arc<TelemetryCell>,
    device: AudioDeviceId,
    /// Invalid-frame count already reported as a configuration change.
    reported_invalid_frames: u64,
    reported_failure: bool,
}

/// Linux product audio endpoint: JACK at the server's rate and quantum, 1–64 output channels.
pub struct LinuxAudioEndpoint {
    pending_renderer: Option<Box<ProductRenderer>>,
    stream: Option<ActiveStream>,
    active_format: Option<AudioFormat>,
    selected_route: Option<AudioRouteConfig>,
    retirement_failed: bool,
}

impl LinuxAudioEndpoint {
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
            stream: None,
            active_format: None,
            selected_route: None,
            retirement_failed: false,
        }
    }

    /// Accepted for API parity with macOS. The JACK server owns its rate and quantum, so this
    /// endpoint never reconfigures it; set the `PipeWire` quantum to the product block size.
    #[must_use]
    pub const fn allow_device_reconfiguration(self) -> Self {
        self
    }

    /// Returns the explicitly selected route while the endpoint is running.
    #[must_use]
    pub fn selected_route(&self) -> Option<&AudioRouteConfig> {
        self.selected_route.as_ref()
    }

    /// Whether callback retirement could not be proven and this endpoint must be replaced.
    #[must_use]
    pub const fn is_faulted(&self) -> bool {
        self.retirement_failed
    }

    /// Services worker bank handoffs without audio after proven callback retirement.
    ///
    /// Returns `None` while audio is active or retirement is uncertain.
    pub fn service_stopped_recoveries(&mut self) -> Option<StoppedRecoveryAccess<'_>> {
        if self.retirement_failed || self.stream.is_some() {
            return None;
        }
        Some(self.pending_renderer.as_mut()?.stopped_recovery_access())
    }

    /// Returns coherent callback counters while a stream is running.
    #[must_use]
    pub fn callback_telemetry(&self) -> Option<CallbackTelemetry> {
        self.stream
            .as_ref()
            .map(|active| active.telemetry.snapshot())
    }

    /// The stopped renderer, for a caller that moves it to another endpoint. `None` while
    /// audio runs or after a retirement fault.
    #[must_use]
    pub fn into_renderer(mut self) -> Option<Box<ProductRenderer>> {
        if self.is_faulted() || self.stream.is_some() {
            return None;
        }
        self.pending_renderer.take()
    }

    /// Configures the pending mixer while the endpoint is stopped.
    ///
    /// # Errors
    ///
    /// Returns an error when the stream is already running.
    pub fn with_pending_mixer_mut<R>(
        &mut self,
        configure: impl FnOnce(&mut RealtimeRackMixer) -> R,
    ) -> Result<R, BoxError> {
        let renderer = self
            .pending_renderer
            .as_mut()
            .ok_or("audio endpoint is running; stop before configuring the mixer")?;
        Ok(configure(renderer.mixer_mut()))
    }

    fn open(
        &mut self,
        route: AudioRouteConfig,
        input_channels: u16,
        stereo_output: bool,
    ) -> Result<(), BoxError> {
        let format = route.format;
        let frames = BlockFrames::from_u32(format.max_frames_per_callback).ok_or(
            AudioFormatError::UnsupportedProductFrames {
                frames: format.max_frames_per_callback,
            },
        )?;
        let pending = self.pending_renderer.as_ref().ok_or(
            "audio renderer could not be safely recovered; rebuild the endpoint before starting",
        )?;
        if pending.sample_rate() != format.sample_rate_hz {
            return Err(format!(
                "the session runs at {} Hz, but the route requests {} Hz",
                pending.sample_rate(),
                format.sample_rate_hz
            )
            .into());
        }
        let renderer = self.pending_renderer.take().expect("checked above");
        let returned = RendererReturn::default();
        let telemetry = Arc::new(TelemetryCell::default());
        let callback = ProductCallback::new(
            renderer,
            Arc::clone(&returned),
            frames,
            input_channels,
            format.channel_count,
            stereo_output,
            Arc::clone(&telemetry),
        );
        let config = StreamConfig {
            device: Some(DeviceId::Jack),
            sample_rate: format.sample_rate_hz,
            frames_per_buffer: frames.as_u32(),
            input_channels,
            output_channels: format.channel_count,
            auto_connect: true,
        };
        let stream = match Stream::open(config, callback)
            .map_err(BoxError::from)
            .and_then(|stream| start_at_quantum(stream, frames))
        {
            Ok(stream) => stream,
            Err(error) => {
                // A failed open or a dropped stream has already returned the callback.
                self.recover_renderer(&returned)?;
                return Err(error);
            }
        };
        let counters = stream.counters();
        self.stream = Some(ActiveStream {
            stream,
            counters,
            returned,
            telemetry,
            device: route.output.clone(),
            reported_invalid_frames: 0,
            reported_failure: false,
        });
        self.active_format = Some(format);
        self.selected_route = Some(route);
        Ok(())
    }

    fn recover_renderer(&mut self, returned: &RendererReturn) -> Result<(), BoxError> {
        let renderer = returned
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if renderer.is_none() {
            self.retirement_failed = true;
            return Err(
                "the JACK server stopped before the audio callback could be retired; \
                        restart the endpoint"
                    .into(),
            );
        }
        self.pending_renderer = renderer;
        Ok(())
    }
}

impl Default for LinuxAudioEndpoint {
    fn default() -> Self {
        Self::new()
    }
}

/// Starts an opened stream only when the server's quantum is the product block size.
/// On failure the stream is dropped, which returns its callback.
fn start_at_quantum(mut stream: Stream, frames: BlockFrames) -> Result<Stream, BoxError> {
    let quantum = stream.info().frames_per_buffer;
    if quantum != frames.as_u32() {
        let frames = frames.as_u32();
        return Err(format!(
            "the JACK server runs {quantum}-frame blocks, but {frames} were requested; set the \
             quantum first, for example `pw-metadata -n settings 0 clock.force-quantum {frames}`"
        )
        .into());
    }
    stream.start()?;
    Ok(stream)
}

fn device(id: &AudioDeviceId) -> Result<LinuxAudioDevice, BoxError> {
    enumerate_devices()?
        .into_iter()
        .find(|device| &device.info.id == id)
        .ok_or_else(|| format!("selected audio device {id} was not found").into())
}

fn validate_product_format(format: AudioFormat) -> Result<AudioFormat, BoxError> {
    let format = format.validate()?;
    if !PRODUCT_SAMPLE_RATES_HZ.contains(&format.sample_rate_hz) {
        return Err(Box::new(AudioFormatError::UnsupportedProductSampleRate {
            sample_rate_hz: format.sample_rate_hz,
        }));
    }
    if format.channel_count == 0 || format.channel_count > MAX_PRODUCT_CHANNELS.min(MAX_CHANNELS) {
        return Err(format!(
            "product audio supports 1 to {MAX_PRODUCT_CHANNELS} output channels; requested {}",
            format.channel_count
        )
        .into());
    }
    if BlockFrames::from_u32(format.max_frames_per_callback).is_none() {
        return Err(Box::new(AudioFormatError::UnsupportedProductFrames {
            frames: format.max_frames_per_callback,
        }));
    }
    Ok(format)
}

impl AudioEndpoint for LinuxAudioEndpoint {
    fn enumerate_outputs(&self) -> Result<Vec<AudioDeviceInfo>, BoxError> {
        Ok(enumerate_devices()?
            .into_iter()
            .filter(|device| device.capabilities.max_output_channels > 0)
            .map(|device| device.info)
            .collect())
    }

    fn enumerate_inputs(&self) -> Result<Vec<AudioDeviceInfo>, BoxError> {
        Ok(enumerate_devices()?
            .into_iter()
            .filter(|device| device.capabilities.max_input_channels > 0)
            .map(|device| device.info)
            .collect())
    }

    fn start_route(&mut self, route: AudioRouteConfig) -> Result<(), BoxError> {
        let format = validate_product_format(route.format)?;
        let output = device(&route.output)?;
        if format.channel_count > output.capabilities.max_output_channels {
            return Err(format!(
                "selected output device {} has {} channels, but {} were requested",
                route.output, output.capabilities.max_output_channels, format.channel_count
            )
            .into());
        }
        let input_channels = match &route.input {
            Some(input) => device(input)?.capabilities.max_input_channels,
            None => 0,
        };
        self.stop()?;
        self.pending_renderer
            .as_ref()
            .ok_or("audio renderer could not be safely recovered; rebuild the endpoint")?
            .validate_routes(
                usize::from(input_channels),
                usize::from(format.channel_count),
            )?;
        self.open(AudioRouteConfig { format, ..route }, input_channels, false)
    }

    fn poll_event(&mut self) -> Option<AudioEndpointEvent> {
        let active = self.stream.as_mut()?;
        if !active.reported_failure
            && let StreamState::Failed(reason) = active.counters.snapshot().state
        {
            active.reported_failure = true;
            let device = active.device.clone();
            return Some(match reason {
                FailureReason::FormatChanged | FailureReason::BufferTooLarge => {
                    AudioEndpointEvent::DeviceConfigurationChanged { device }
                }
                FailureReason::ServerShutdown
                | FailureReason::DeviceLost
                | FailureReason::BackendFailure => AudioEndpointEvent::DeviceLost { device },
            });
        }
        let invalid_frames = active.telemetry.snapshot().invalid_frames;
        if invalid_frames > active.reported_invalid_frames {
            // The server changed its quantum under a running stream; the callback is silent.
            active.reported_invalid_frames = invalid_frames;
            return Some(AudioEndpointEvent::DeviceConfigurationChanged {
                device: active.device.clone(),
            });
        }
        None
    }

    fn start(&mut self, format: AudioFormat) -> Result<(), BoxError> {
        let format = validate_product_format(format)?;
        if !format.is_product_format() {
            return Err(Box::new(AudioFormatError::UnsupportedProductFrames {
                frames: format.max_frames_per_callback,
            }));
        }
        let output = enumerate_devices()?
            .into_iter()
            .find(|device| device.capabilities.max_output_channels >= format.channel_count)
            .ok_or("no running JACK server has stereo playback ports")?;
        self.stop()?;
        self.open(
            AudioRouteConfig {
                input: None,
                output: output.info.id,
                format,
            },
            0,
            true,
        )?;
        self.selected_route = None;
        Ok(())
    }

    fn stop(&mut self) -> Result<(), BoxError> {
        if self.retirement_failed {
            return Err("native audio cleanup failed; this endpoint cannot be reused".into());
        }
        self.active_format = None;
        self.selected_route = None;
        let Some(mut active) = self.stream.take() else {
            return Ok(());
        };
        // A failed stop handshake is not fatal: dropping the stream deactivates the client,
        // which waits for any running callback before the callback is freed.
        let _ = active.stream.stop();
        let returned = Arc::clone(&active.returned);
        drop(active);
        self.recover_renderer(&returned)
    }

    fn active_format(&self) -> Option<AudioFormat> {
        self.active_format
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sp_audio_io::{AudioEndpoint, AudioFormat, BlockFrames, ProductRenderer};
    use sp_engine::PreparedGraph;

    use super::{LinuxAudioEndpoint, ProductCallback, RendererReturn, TelemetryCell};

    #[test]
    fn dropping_the_callback_returns_the_original_renderer() {
        let renderer = Box::new(ProductRenderer::new(PreparedGraph::empty()));
        let pointer = std::ptr::from_ref(renderer.as_ref());
        let returned = RendererReturn::default();
        let callback = ProductCallback::new(
            renderer,
            Arc::clone(&returned),
            BlockFrames::Frames64,
            2,
            2,
            false,
            Arc::new(TelemetryCell::default()),
        );
        drop(callback);
        let renderer = returned.lock().unwrap().take().expect("renderer returned");
        assert_eq!(std::ptr::from_ref(renderer.as_ref()), pointer);
    }

    #[test]
    fn rejects_non_product_formats_without_touching_jack() {
        let mut endpoint = LinuxAudioEndpoint::new();
        for format in [
            AudioFormat {
                sample_rate_hz: 44_100,
                ..AudioFormat::product_stereo(64).unwrap()
            },
            AudioFormat {
                max_frames_per_callback: 100,
                ..AudioFormat::product_stereo(64).unwrap()
            },
        ] {
            assert!(endpoint.start(format).is_err());
            assert!(endpoint.active_format().is_none());
            assert!(!endpoint.is_faulted());
        }
        assert!(endpoint.service_stopped_recoveries().is_some());
    }
}
