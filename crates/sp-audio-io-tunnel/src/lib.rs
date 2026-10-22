//! Superposition as a Tunnel device, so its racks can be inserts and send-returns of a Tunnel
//! mixer such as Lambda.
//!
//! [`TunnelDevice`] runs one `tunnel_device::Device` for as long as the app wants Tunnel. It
//! declares two endpoints, fixed for its life:
//!
//! | Position | Endpoint | Kind | Channels |
//! | --- | --- | --- | --- |
//! | [`INPUTS`] | "Superposition inputs" | sink (network to renderer) | 1 to [`MAX_CHANNELS`] |
//! | [`OUTPUTS`] | "Superposition outputs" | source (renderer to network) | 1 to [`MAX_CHANNELS`] |
//!
//! Their IDs are made once and kept in the Tunnel directory ([`Endpoints`]). The Tunnel master
//! adopts the device and makes every route; Superposition only exposes its endpoints.
//!
//! [`TunnelAudioEndpoint`] implements [`sp_audio_io::AudioEndpoint`] on the device. The Tunnel
//! system is the session clock: one cycle of 12 or 24 frames on the Tunnel network thread. That
//! is too short for the renderer's worker round trips, so a session is split in two (see
//! `bridge`): the network thread only moves samples between Tunnel buffers and lock-free rings,
//! and a render thread runs the [`ProductRenderer`](sp_audio_io::ProductRenderer) once per
//! product block. The session adds a fixed [`AddedLatency`]. There is one clock, so there is no
//! drift and no resampling.

mod bridge;
mod device;
mod endpoint;
mod endpoints;
mod session;

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use sp_audio_io::BlockFrames;
use tunnel_device::{Adoption, DeviceError};
use tunnel_host::audio::MAX_FRAMES;

pub use bridge::{InlineApp, SessionCounters, SessionFormat, SessionTelemetry};
pub use device::TunnelDevice;
pub use endpoint::TunnelAudioEndpoint;
pub use endpoints::Endpoints;
pub use sp_shared_memory_os::SchedulingMode;
pub use tunnel_device::DeviceStatus;
pub use tunnel_host::net::Interfaces;
pub use tunnel_types::{ClockState, Interface};

/// The largest channel count of each endpoint: the product's physical channel limit.
pub const MAX_CHANNELS: u16 = 64;

/// The position of "Superposition inputs" in the device's endpoint declarations.
pub const INPUTS: usize = 0;

/// The position of "Superposition outputs" in the device's endpoint declarations.
pub const OUTPUTS: usize = 1;

/// The prefix of the device's [`AudioDeviceId`](sp_audio_io::AudioDeviceId).
pub const DEVICE_ID_PREFIX: &str = "tunnel:";

/// How to start the Tunnel device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TunnelConfig {
    /// The device name, as the master lists it. Also the audio device ID: `tunnel:<name>`.
    pub name: String,
    /// The network interfaces of paths A and B.
    pub interfaces: Interfaces,
    /// The Tunnel directory: device ID, adoption state and endpoint IDs.
    pub directory: PathBuf,
    /// The endpoint channel counts.
    pub channels: TunnelChannels,
}

/// The channel counts of "Superposition inputs" and "Superposition outputs".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TunnelChannels {
    /// Channels of "Superposition inputs", 1 to [`MAX_CHANNELS`].
    pub inputs: u16,
    /// Channels of "Superposition outputs", 1 to [`MAX_CHANNELS`].
    pub outputs: u16,
}

impl TunnelChannels {
    /// [`MAX_CHANNELS`] each way.
    pub const MAX: Self = Self {
        inputs: MAX_CHANNELS,
        outputs: MAX_CHANNELS,
    };

    fn validate(self) -> Result<Self, TunnelError> {
        let valid = 1..=MAX_CHANNELS;
        if valid.contains(&self.inputs) && valid.contains(&self.outputs) {
            Ok(self)
        } else {
            Err(TunnelError::Channels(self))
        }
    }
}

/// The Tunnel device did not start.
#[derive(Debug, thiserror::Error)]
pub enum TunnelError {
    /// An endpoint has no channels or more than [`MAX_CHANNELS`].
    #[error("Tunnel endpoints take 1 to {MAX_CHANNELS} channels; requested {0:?}")]
    Channels(TunnelChannels),
    /// The Tunnel directory or its endpoint ID file cannot be used.
    #[error("cannot read or write the Tunnel endpoint IDs: {0}")]
    Endpoints(#[source] io::Error),
    /// The device did not start.
    #[error(transparent)]
    Device(#[from] DeviceError),
}

/// The state of the Tunnel device, for the app to show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TunnelStatus {
    /// Adoption, PTP clock state, registration, master role and media statistics.
    pub device: DeviceStatus,
    /// Tunnel cycles since the device started. They run only while it is in a system.
    pub cycles: u64,
    /// The size of the last cycle (12 or 24 frames), or `None` before the first.
    pub cycle_frames: Option<usize>,
}

impl TunnelStatus {
    /// The system rate, or `None` while the device is in no system.
    pub const fn sample_rate_hz(&self) -> Option<u32> {
        match self.device.adoption {
            Adoption::Adopted { sample_rate, .. } => Some(sample_rate.hz()),
            Adoption::Unadopted => None,
        }
    }

    /// Whether the PTP clock follows its parent within bound, or this device is the PTP
    /// master.
    pub const fn is_locked(&self) -> bool {
        matches!(
            self.device.clock,
            Some(ClockState::Locked | ClockState::Master)
        )
    }
}

/// The latency that a session adds from "Superposition inputs" to "Superposition outputs":
/// output media time `t` carries the input presented at `t - frames`. Tunnel flow latencies
/// come on top.
///
/// It is two product blocks and the largest Tunnel cycle: one block to gather the input, up to
/// one block for the render (workers have 75% of a block), and one cycle because blocks and
/// cycles do not divide. It does not depend on the system's cycle, so it is known before
/// adoption and stays the same when the master changes the packet time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AddedLatency {
    /// The latency in frames.
    pub frames: u32,
    /// The session rate in hertz.
    pub sample_rate_hz: u32,
}

impl AddedLatency {
    /// The latency of a session with `block` at `sample_rate_hz`.
    #[expect(clippy::cast_possible_truncation, reason = "MAX_FRAMES is 24")]
    pub const fn new(block: BlockFrames, sample_rate_hz: u32) -> Self {
        Self {
            frames: 2 * block.as_u32() + MAX_FRAMES as u32,
            sample_rate_hz,
        }
    }

    /// The latency in time.
    pub fn duration(self) -> Duration {
        Duration::from_nanos(
            u64::from(self.frames) * 1_000_000_000 / u64::from(self.sample_rate_hz.max(1)),
        )
    }
}
