//! The product audio endpoint on the Tunnel device.

use std::error::Error;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use sp_audio_io::{
    AudioDeviceId, AudioDeviceInfo, AudioEndpoint, AudioEndpointEvent, AudioFormat,
    AudioFormatError, AudioRouteConfig, BlockFrames, CallbackTelemetry, PRODUCT_SAMPLE_RATES_HZ,
    ProductRenderer, StoppedRecoveryAccess,
};
use sp_engine::{PreparedGraph, RealtimeRackMixer};
use sp_shared_memory_os::SchedulingMode;

use crate::bridge::{SessionCounters, SessionFormat};
use crate::session::Session;
use crate::{AddedLatency, TunnelDevice};

type BoxError = Box<dyn Error + Send + Sync>;

struct Running {
    session: Session,
    /// The route; its rate must stay the system rate.
    route: AudioRouteConfig,
    /// Started by `start_route`, not by the legacy stereo `start`.
    selected: bool,
    /// The configuration change has been reported and the output muted.
    reported_change: bool,
}

/// Product audio on Tunnel: the renderer runs once per session block on its own thread, fed
/// from "Superposition inputs" and feeding "Superposition outputs". The Tunnel system is the
/// clock; [`TunnelAudioEndpoint::latency`] is what the session adds between the endpoints.
///
/// Stopping returns the original renderer. If the render thread cannot be retired, the
/// endpoint latches [`TunnelAudioEndpoint::is_faulted`] and must be replaced.
pub struct TunnelAudioEndpoint {
    device: Arc<TunnelDevice>,
    pending_renderer: Option<Box<ProductRenderer>>,
    running: Option<Running>,
    retirement_failed: bool,
}

impl std::fmt::Debug for TunnelAudioEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TunnelAudioEndpoint")
            .field("device", self.device.id())
            .field("running", &self.running.is_some())
            .field("faulted", &self.retirement_failed)
            .finish_non_exhaustive()
    }
}

impl TunnelAudioEndpoint {
    /// Creates a stopped endpoint on `device` with an empty graph.
    #[must_use]
    pub fn new(device: Arc<TunnelDevice>) -> Self {
        Self::with_renderer(device, ProductRenderer::new(PreparedGraph::empty()))
    }

    /// Creates a stopped endpoint on `device` with a caller-configured renderer.
    #[must_use]
    pub fn with_renderer(device: Arc<TunnelDevice>, renderer: ProductRenderer) -> Self {
        Self {
            device,
            pending_renderer: Some(Box::new(renderer)),
            running: None,
            retirement_failed: false,
        }
    }

    /// Accepted for API parity. The Tunnel master owns the system rate and cycle, and any
    /// product block runs on any cycle, so there is nothing to reconfigure.
    #[must_use]
    pub const fn allow_device_reconfiguration(self) -> Self {
        self
    }

    /// The device this endpoint runs on.
    pub const fn device(&self) -> &Arc<TunnelDevice> {
        &self.device
    }

    /// Returns the explicitly selected route while the endpoint is running.
    #[must_use]
    pub fn selected_route(&self) -> Option<&AudioRouteConfig> {
        self.running
            .as_ref()
            .filter(|running| running.selected)
            .map(|running| &running.route)
    }

    /// Whether the render thread could not be retired and this endpoint must be replaced.
    #[must_use]
    pub const fn is_faulted(&self) -> bool {
        self.retirement_failed
    }

    /// Services worker bank handoffs without audio after proven render-thread retirement.
    ///
    /// Returns `None` while audio is active or retirement is uncertain.
    pub fn service_stopped_recoveries(&mut self) -> Option<StoppedRecoveryAccess<'_>> {
        if self.retirement_failed || self.running.is_some() {
            return None;
        }
        Some(self.pending_renderer.as_mut()?.stopped_recovery_access())
    }

    /// Returns coherent counters while a session runs. See
    /// [`SessionCounters::callback_telemetry`] for the mapping.
    #[must_use]
    pub fn callback_telemetry(&self) -> Option<CallbackTelemetry> {
        self.running.as_ref().map(|running| {
            let format = running.session.format();
            running
                .session
                .telemetry()
                .counters()
                .callback_telemetry(format.block)
        })
    }

    /// The session counters, with underruns and overruns apart, while a session runs.
    #[must_use]
    pub fn session_counters(&self) -> Option<SessionCounters> {
        self.running
            .as_ref()
            .map(|running| running.session.telemetry().counters())
    }

    /// The latency between "Superposition inputs" and "Superposition outputs" while a session
    /// runs. Tunnel flow latencies come on top.
    #[must_use]
    pub fn latency(&self) -> Option<AddedLatency> {
        self.running
            .as_ref()
            .map(|running| running.session.format().latency())
    }

    /// The render thread's scheduling while a session runs.
    #[must_use]
    pub fn render_scheduling(&self) -> Option<SchedulingMode> {
        self.running
            .as_ref()
            .map(|running| running.session.scheduling())
    }

    /// The stopped renderer, for a caller that moves it to another endpoint. `None` while
    /// audio runs or after a retirement fault.
    #[must_use]
    pub fn into_renderer(mut self) -> Option<Box<ProductRenderer>> {
        if self.is_faulted() || self.running.is_some() {
            return None;
        }
        self.pending_renderer.take()
    }

    /// Configures the pending mixer while the endpoint is stopped.
    ///
    /// # Errors
    ///
    /// Returns an error when the session is already running.
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

    fn check_device(&self, id: &AudioDeviceId) -> Result<(), BoxError> {
        if id == self.device.id() {
            Ok(())
        } else {
            Err(format!("selected audio device {id} was not found").into())
        }
    }

    /// Checks the system rate and starts a session on the device.
    fn open(&mut self, route: AudioRouteConfig, inputs: u16, stereo: bool) -> Result<(), BoxError> {
        let format = route.format;
        let block = BlockFrames::from_u32(format.max_frames_per_callback).ok_or(
            AudioFormatError::UnsupportedProductFrames {
                frames: format.max_frames_per_callback,
            },
        )?;
        let system_rate =
            self.device.status().sample_rate_hz().ok_or(
                "the Tunnel device is not in a system; adopt it from the Tunnel master first",
            )?;
        if system_rate != format.sample_rate_hz {
            return Err(format!(
                "the Tunnel system runs at {system_rate} Hz, but the route requests {} Hz",
                format.sample_rate_hz
            )
            .into());
        }
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
        let session_format = SessionFormat {
            block,
            sample_rate_hz: format.sample_rate_hz,
            inputs,
            outputs: format.channel_count,
        };
        let (session, network) = match Session::start(renderer, session_format, stereo) {
            Ok(started) => started,
            Err(failed) => {
                self.retirement_failed = failed.renderer.is_none();
                self.pending_renderer = failed.renderer;
                return Err(failed.error.into());
            }
        };
        if !self.device.attach(Some(network)) {
            self.retire(session)?;
            return Err(
                "the Tunnel device runs no cycles and has not taken the last session change; \
                 check that it is in a system"
                    .into(),
            );
        }
        self.running = Some(Running {
            session,
            route,
            selected: !stereo,
            reported_change: false,
        });
        Ok(())
    }

    /// Stops `session`'s render thread and keeps its renderer, or latches the fault.
    fn retire(&mut self, session: Session) -> Result<(), BoxError> {
        match session.stop() {
            Ok(renderer) => {
                self.pending_renderer = Some(renderer);
                Ok(())
            }
            Err(error) => {
                self.retirement_failed = true;
                Err(error.into())
            }
        }
    }
}

fn validate_product_format(format: AudioFormat, outputs: u16) -> Result<AudioFormat, BoxError> {
    let format = format.validate()?;
    if !PRODUCT_SAMPLE_RATES_HZ.contains(&format.sample_rate_hz) {
        return Err(Box::new(AudioFormatError::UnsupportedProductSampleRate {
            sample_rate_hz: format.sample_rate_hz,
        }));
    }
    if format.channel_count > outputs {
        return Err(format!(
            "Superposition outputs has {outputs} channels, but {} were requested",
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

impl AudioEndpoint for TunnelAudioEndpoint {
    fn enumerate_outputs(&self) -> Result<Vec<AudioDeviceInfo>, BoxError> {
        Ok(vec![self.device.info()])
    }

    fn enumerate_inputs(&self) -> Result<Vec<AudioDeviceInfo>, BoxError> {
        Ok(vec![self.device.info()])
    }

    fn start_route(&mut self, route: AudioRouteConfig) -> Result<(), BoxError> {
        let channels = self.device.channels();
        let format = validate_product_format(route.format, channels.outputs)?;
        self.check_device(&route.output)?;
        let inputs = match &route.input {
            Some(input) => {
                self.check_device(input)?;
                channels.inputs
            }
            None => 0,
        };
        self.stop()?;
        self.pending_renderer
            .as_ref()
            .ok_or("audio renderer could not be safely recovered; rebuild the endpoint")?
            .validate_routes(usize::from(inputs), usize::from(format.channel_count))?;
        self.open(AudioRouteConfig { format, ..route }, inputs, false)
    }

    fn poll_event(&mut self) -> Option<AudioEndpointEvent> {
        let running = self.running.as_mut()?;
        if running.reported_change {
            return None;
        }
        let rate = self.device.status().sample_rate_hz();
        if rate == Some(running.route.format.sample_rate_hz) {
            return None;
        }
        // Released, or adopted into a system at another rate: mute until the app restarts.
        running.reported_change = true;
        running
            .session
            .telemetry()
            .shared()
            .muted
            .store(true, Relaxed);
        Some(AudioEndpointEvent::DeviceConfigurationChanged {
            device: self.device.id().clone(),
        })
    }

    fn start(&mut self, format: AudioFormat) -> Result<(), BoxError> {
        let format = validate_product_format(format, self.device.channels().outputs)?;
        if !format.is_product_format() {
            return Err(Box::new(AudioFormatError::UnsupportedProductFrames {
                frames: format.max_frames_per_callback,
            }));
        }
        self.stop()?;
        let route = AudioRouteConfig {
            input: None,
            output: self.device.id().clone(),
            format,
        };
        self.open(route, 0, true)
    }

    fn stop(&mut self) -> Result<(), BoxError> {
        if self.retirement_failed {
            return Err(
                "the Tunnel render thread was not retired; this endpoint cannot be reused".into(),
            );
        }
        let Some(running) = self.running.take() else {
            return Ok(());
        };
        // A detach that does not fit leaves a network side without a render thread: it plays
        // silence until the next session replaces it.
        self.device.attach(None);
        self.retire(running.session)
    }

    fn active_format(&self) -> Option<AudioFormat> {
        self.running.as_ref().map(|running| running.route.format)
    }
}

impl Drop for TunnelAudioEndpoint {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
