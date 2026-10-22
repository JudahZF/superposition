//! The device backends for this platform: AUHAL on macOS; JACK and Tunnel on Linux.
//!
//! On Linux a route's output device ID selects the backend: `tunnel:<name>` routes run on the
//! embedded Tunnel device when it is started (see [`set_tunnel_device`]); every other route runs on
//! JACK. Both keep their renderer across stop, so the wrapper only moves it between them.

#[cfg(target_os = "macos")]
pub(crate) use sp_audio_io_macos::{
    MacOsAudioDevice as PlatformAudioDevice, MacOsAudioEndpoint as PlatformAudioEndpoint,
    enumerate_devices,
};

#[cfg(target_os = "linux")]
pub(crate) use linux::{
    PlatformAudioDevice, PlatformAudioEndpoint, enumerate_devices, set_tunnel_device, tunnel_device,
};

#[cfg(target_os = "linux")]
mod linux {
    use std::{
        error::Error,
        sync::{Arc, Mutex, PoisonError},
    };

    use sp_audio_io::{
        AudioDeviceInfo, AudioEndpoint, AudioEndpointEvent, AudioFormat, AudioRouteConfig,
        CallbackTelemetry, ProductRenderer, StoppedRecoveryAccess,
    };
    use sp_audio_io_linux::LinuxAudioEndpoint;
    use sp_audio_io_tunnel::{DEVICE_ID_PREFIX, TunnelAudioEndpoint, TunnelDevice};
    use sp_engine::PreparedGraph;

    pub(crate) use sp_audio_io_linux::LinuxAudioDevice as PlatformAudioDevice;

    type BoxError = Box<dyn Error + Send + Sync>;

    /// The running Tunnel device. It outlives sessions: adoption and PTP lock take seconds.
    static TUNNEL: Mutex<Option<Arc<TunnelDevice>>> = Mutex::new(None);

    /// Makes Tunnel routes available (or removes them with `None`).
    pub(crate) fn set_tunnel_device(device: Option<Arc<TunnelDevice>>) {
        *TUNNEL.lock().unwrap_or_else(PoisonError::into_inner) = device;
    }

    /// The running Tunnel device, if any.
    pub(crate) fn tunnel_device() -> Option<Arc<TunnelDevice>> {
        TUNNEL
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// JACK servers, then the Tunnel device when it runs. A JACK error is reported only when
    /// there is no Tunnel device to offer instead.
    pub(crate) fn enumerate_devices() -> Result<Vec<PlatformAudioDevice>, BoxError> {
        let tunnel = tunnel_device().map(|device| PlatformAudioDevice {
            info: device.info(),
            capabilities: device.capabilities(),
            // The Tunnel adapter decouples the product block from the Tunnel cycle.
            supported_buffer_frames: vec![32, 64, 128, 256],
        });
        let mut devices = match sp_audio_io_linux::enumerate_devices() {
            Ok(devices) => devices,
            Err(_) if tunnel.is_some() => Vec::new(),
            Err(error) => return Err(Box::new(error)),
        };
        devices.extend(tunnel);
        Ok(devices)
    }

    enum Backend {
        Jack(LinuxAudioEndpoint),
        Tunnel(TunnelAudioEndpoint),
    }

    /// The Linux product endpoint: JACK or Tunnel, chosen by each route.
    pub(crate) struct PlatformAudioEndpoint {
        backend: Backend,
    }

    impl PlatformAudioEndpoint {
        pub(crate) fn new() -> Self {
            Self::with_renderer(ProductRenderer::new(PreparedGraph::empty()))
        }

        pub(crate) fn with_renderer(renderer: ProductRenderer) -> Self {
            Self {
                backend: Backend::Jack(LinuxAudioEndpoint::with_renderer(renderer)),
            }
        }

        pub(crate) const fn allow_device_reconfiguration(self) -> Self {
            self
        }

        pub(crate) fn service_stopped_recoveries(&mut self) -> Option<StoppedRecoveryAccess<'_>> {
            match &mut self.backend {
                Backend::Jack(endpoint) => endpoint.service_stopped_recoveries(),
                Backend::Tunnel(endpoint) => endpoint.service_stopped_recoveries(),
            }
        }

        pub(crate) fn callback_telemetry(&self) -> Option<CallbackTelemetry> {
            match &self.backend {
                Backend::Jack(endpoint) => endpoint.callback_telemetry(),
                Backend::Tunnel(endpoint) => endpoint.callback_telemetry(),
            }
        }

        fn endpoint(&mut self) -> &mut dyn AudioEndpoint {
            match &mut self.backend {
                Backend::Jack(endpoint) => endpoint,
                Backend::Tunnel(endpoint) => endpoint,
            }
        }

        /// Moves the stopped renderer to the backend a route needs.
        fn select_backend(&mut self, route: &AudioRouteConfig) -> Result<(), BoxError> {
            let wants_tunnel = route.output.as_str().starts_with(DEVICE_ID_PREFIX);
            if wants_tunnel == matches!(self.backend, Backend::Tunnel(_)) {
                return Ok(());
            }
            let device = if wants_tunnel {
                Some(tunnel_device().ok_or("the Tunnel device is not running")?)
            } else {
                None
            };
            self.endpoint().stop()?;
            let previous =
                std::mem::replace(&mut self.backend, Backend::Jack(LinuxAudioEndpoint::new()));
            let renderer = match previous {
                Backend::Jack(endpoint) => endpoint.into_renderer(),
                Backend::Tunnel(endpoint) => endpoint.into_renderer(),
            }
            .ok_or("the stopped audio renderer could not be recovered; restart the engine")?;
            self.backend = match device {
                Some(device) => {
                    Backend::Tunnel(TunnelAudioEndpoint::with_renderer(device, *renderer))
                }
                None => Backend::Jack(LinuxAudioEndpoint::with_renderer(*renderer)),
            };
            Ok(())
        }
    }

    impl AudioEndpoint for PlatformAudioEndpoint {
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
            self.select_backend(&route)?;
            self.endpoint().start_route(route)
        }

        fn poll_event(&mut self) -> Option<AudioEndpointEvent> {
            self.endpoint().poll_event()
        }

        fn start(&mut self, format: AudioFormat) -> Result<(), BoxError> {
            self.endpoint().start(format)
        }

        fn stop(&mut self) -> Result<(), BoxError> {
            self.endpoint().stop()
        }

        fn active_format(&self) -> Option<AudioFormat> {
            match &self.backend {
                Backend::Jack(endpoint) => endpoint.active_format(),
                Backend::Tunnel(endpoint) => endpoint.active_format(),
            }
        }
    }
}
