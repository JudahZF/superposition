//! Audio device boundary types used by the application and engine layers.

use std::fmt;

/// Product sample rate required by the alpha audio path.
pub const PRODUCT_SAMPLE_RATE_HZ: u32 = 48_000;
/// Product channel count (stereo main bus).
pub const PRODUCT_CHANNEL_COUNT: u16 = 2;

/// Identifies an audio device without exposing a platform-specific handle.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AudioDeviceId(String);

impl AudioDeviceId {
    /// Creates an identifier from a stable backend-provided value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the backend-provided identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AudioDeviceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// User-visible device metadata discovered without opening a stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioDeviceInfo {
    /// Stable backend identifier.
    pub id: AudioDeviceId,
    /// Display name.
    pub name: String,
    /// Maximum output channels advertised by the device.
    pub max_output_channels: u16,
}

/// Directional capabilities for a device returned by a backend-specific enumeration API.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioDeviceCapabilities {
    /// Maximum capture channels advertised by the device.
    pub max_input_channels: u16,
    /// Maximum playback channels advertised by the device.
    pub max_output_channels: u16,
    /// Whether this is the system's current default input device.
    pub is_default_input: bool,
    /// Whether this is the system's current default output device.
    pub is_default_output: bool,
}

/// A selected hardware route. Backends own clock synchronization for cross-device routes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioRouteConfig {
    /// Device supplying capture samples. `None` requests an output-only route.
    pub input: Option<AudioDeviceId>,
    /// Device receiving the selected output channels.
    pub output: AudioDeviceId,
    /// Fixed stream format requested from the backend.
    pub format: AudioFormat,
}

/// A control-plane notification from an endpoint. Events are deliberately polled rather than
/// delivered from a platform listener, so application recovery never executes on an audio thread.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AudioEndpointEvent {
    /// A selected device ceased to be usable. The endpoint has muted its callback.
    DeviceLost {
        /// Stable identifier of the device that became unavailable.
        device: AudioDeviceId,
    },
    /// A selected device changed format or topology. The endpoint has muted its callback.
    DeviceConfigurationChanged {
        /// Stable identifier of the device whose configuration changed.
        device: AudioDeviceId,
    },
}

/// The stream parameters negotiated with an audio endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioFormat {
    /// Sample rate in hertz.
    pub sample_rate_hz: u32,
    /// Interleaved output channel count.
    pub channel_count: u16,
    /// Maximum frames supplied in one callback.
    pub max_frames_per_callback: u32,
}

impl AudioFormat {
    /// Returns the fixed 48 kHz stereo product format for `frames` (32, 64, 128, or 256).
    ///
    /// # Errors
    ///
    /// Returns [`AudioFormatError::UnsupportedProductFrames`] for other frame counts.
    pub fn product_stereo(frames: u32) -> Result<Self, AudioFormatError> {
        if !matches!(frames, 32 | 64 | 128 | 256) {
            return Err(AudioFormatError::UnsupportedProductFrames { frames });
        }
        Ok(Self {
            sample_rate_hz: PRODUCT_SAMPLE_RATE_HZ,
            channel_count: PRODUCT_CHANNEL_COUNT,
            max_frames_per_callback: frames,
        })
    }

    /// Validates the format before it crosses into a platform backend.
    ///
    /// # Errors
    ///
    /// Returns [`AudioFormatError`] when a required stream dimension is zero.
    pub fn validate(self) -> Result<Self, AudioFormatError> {
        if self.sample_rate_hz == 0 {
            return Err(AudioFormatError::ZeroSampleRate);
        }
        if self.channel_count == 0 {
            return Err(AudioFormatError::ZeroChannels);
        }
        if self.max_frames_per_callback == 0 {
            return Err(AudioFormatError::ZeroCallbackFrames);
        }
        Ok(self)
    }

    /// Returns whether this format matches the alpha product contract.
    #[must_use]
    pub const fn is_product_format(self) -> bool {
        self.sample_rate_hz == PRODUCT_SAMPLE_RATE_HZ
            && self.channel_count == PRODUCT_CHANNEL_COUNT
            && matches!(self.max_frames_per_callback, 32 | 64 | 128 | 256)
    }
}

/// Validation failure for [`AudioFormat`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioFormatError {
    /// A stream must have a nonzero sample rate.
    ZeroSampleRate,
    /// A stream must have at least one channel.
    ZeroChannels,
    /// A callback must contain at least one frame.
    ZeroCallbackFrames,
    /// Product mode only accepts 32-, 64-, 128-, or 256-frame callbacks.
    UnsupportedProductFrames {
        /// Requested frames.
        frames: u32,
    },
}

impl fmt::Display for AudioFormatError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroSampleRate => formatter.write_str("sample rate must not be zero"),
            Self::ZeroChannels => formatter.write_str("channel count must not be zero"),
            Self::ZeroCallbackFrames => {
                formatter.write_str("callback frame count must not be zero")
            }
            Self::UnsupportedProductFrames { frames } => write!(
                formatter,
                "product audio requires 32, 64, 128, or 256 frames, got {frames}"
            ),
        }
    }
}

impl std::error::Error for AudioFormatError {}

/// Platform-neutral contract for an audio endpoint implementation.
pub trait AudioEndpoint {
    /// Lists output devices available to this endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when the platform cannot enumerate devices.
    fn enumerate_outputs(
        &self,
    ) -> Result<Vec<AudioDeviceInfo>, Box<dyn std::error::Error + Send + Sync>>;

    /// Lists capture-capable devices. The default implementation preserves output-only backends.
    ///
    /// # Errors
    ///
    /// Returns an error when the platform cannot enumerate capture devices.
    fn enumerate_inputs(
        &self,
    ) -> Result<Vec<AudioDeviceInfo>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Vec::new())
    }

    /// Starts a selected route. Backends that only support the legacy default-output path may
    /// reject this request; this default deliberately does not silently discard the selection.
    ///
    /// # Errors
    ///
    /// Returns an error when the route is unsupported or cannot be started.
    fn start_route(
        &mut self,
        _route: AudioRouteConfig,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Err("selected audio routes are not supported by this endpoint".into())
    }

    /// Returns a pending device event, if any. This must be non-blocking.
    fn poll_event(&mut self) -> Option<AudioEndpointEvent> {
        None
    }

    /// Starts the endpoint with a validated format.
    ///
    /// # Errors
    ///
    /// Returns an error when the platform endpoint cannot start with `format`.
    fn start(
        &mut self,
        format: AudioFormat,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Stops the endpoint and releases its backend resources.
    ///
    /// # Errors
    ///
    /// Returns an error when the platform endpoint cannot stop cleanly.
    fn stop(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Returns the format currently negotiated with the device, if running.
    fn active_format(&self) -> Option<AudioFormat>;
}

#[cfg(test)]
mod tests {
    use super::{AudioFormat, AudioFormatError, PRODUCT_SAMPLE_RATE_HZ};

    #[test]
    fn validates_stream_format() {
        assert_eq!(
            AudioFormat {
                sample_rate_hz: 0,
                channel_count: 2,
                max_frames_per_callback: 128,
            }
            .validate(),
            Err(AudioFormatError::ZeroSampleRate)
        );
    }

    #[test]
    fn product_stereo_accepts_only_supported_frames() {
        let format = AudioFormat::product_stereo(128).expect("128 frames");
        assert_eq!(format.sample_rate_hz, PRODUCT_SAMPLE_RATE_HZ);
        assert!(format.is_product_format());
        assert_eq!(
            AudioFormat::product_stereo(16),
            Err(AudioFormatError::UnsupportedProductFrames { frames: 16 })
        );
    }
}
