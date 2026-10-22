//! The Superposition Tunnel device: lives longer than one audio session.

use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicU64, AtomicUsize};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rtrb::{Consumer, Producer, RingBuffer};
use sp_audio_io::{AudioDeviceCapabilities, AudioDeviceId, AudioDeviceInfo};
use tunnel_device::{Device, DeviceConfig};
use tunnel_host::audio::{AudioApp, Block};

use crate::bridge::NetworkSide;
use crate::{DEVICE_ID_PREFIX, Endpoints, TunnelChannels, TunnelConfig, TunnelError, TunnelStatus};

/// The name of the endpoint ID file in the Tunnel directory.
const ENDPOINTS_FILE: &str = "endpoints";

/// Session changes that may wait for the network thread.
const COMMAND_RING: usize = 4;

/// Network sides that the network thread may give back before the control thread drops them.
const RETIRED_RING: usize = 4;

/// Counters that the network thread writes and the control thread reads.
#[derive(Debug, Default)]
struct Cycles {
    count: AtomicU64,
    /// The frames of the last cycle; 0 before the first.
    frames: AtomicUsize,
}

/// The device's [`AudioApp`]: runs the attached session's network side, or plays silence.
struct DeviceApp {
    session: Option<NetworkSide>,
    /// `Some` attaches a session, `None` detaches the current one.
    commands: Consumer<Option<NetworkSide>>,
    retired: Producer<NetworkSide>,
    cycles: Arc<Cycles>,
}

impl AudioApp for DeviceApp {
    /// Installs waiting session changes while the return ring has room for the side each one
    /// replaces, so nothing is freed here, then runs one cycle.
    ///
    /// Real-time: no allocation, no free, no lock, no wait.
    fn process(&mut self, block: &mut Block<'_>) {
        self.cycles
            .count
            .store(self.cycles.count.load(Relaxed).wrapping_add(1), Relaxed);
        self.cycles.frames.store(block.frames(), Relaxed);
        while !self.retired.is_full() {
            let Ok(command) = self.commands.pop() else {
                break;
            };
            if let Some(old) = std::mem::replace(&mut self.session, command) {
                let pushed = self.retired.push(old);
                debug_assert!(pushed.is_ok(), "the return ring had room");
            }
        }
        if let Some(session) = &mut self.session {
            session.process(block);
        }
    }
}

/// The control thread's end of a [`DeviceApp`].
struct AppLink {
    commands: Producer<Option<NetworkSide>>,
    retired: Consumer<NetworkSide>,
}

impl AppLink {
    /// Sends a session change after dropping the sides that the network thread gave back.
    /// Returns whether it fit.
    fn send(&mut self, command: Option<NetworkSide>) -> bool {
        while self.retired.pop().is_ok() {}
        self.commands.push(command).is_ok()
    }
}

/// Superposition's Tunnel device, for the life of the app or the Tunnel setting.
///
/// It declares "Superposition inputs" and "Superposition outputs" and joins whichever system a
/// Tunnel master adopts it into. The master (for example Lambda) makes every route; a
/// [`crate::TunnelAudioEndpoint`] attaches an audio session to it.
pub struct TunnelDevice {
    device: Mutex<Device>,
    link: Mutex<AppLink>,
    cycles: Arc<Cycles>,
    endpoints: Endpoints,
    channels: TunnelChannels,
    name: String,
    id: AudioDeviceId,
}

impl std::fmt::Debug for TunnelDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TunnelDevice")
            .field("id", &self.id)
            .field("endpoints", &self.endpoints)
            .finish_non_exhaustive()
    }
}

impl TunnelDevice {
    /// Loads or makes the endpoint IDs in `config.directory`, then starts the device with its
    /// state there. No session is attached: the device plays silence.
    ///
    /// # Errors
    ///
    /// The channel counts are out of range, the endpoint file cannot be used, or the device
    /// did not start (for example, a port cannot be bound).
    pub fn start(config: &TunnelConfig) -> Result<Self, TunnelError> {
        let channels = config.channels.validate()?;
        std::fs::create_dir_all(&config.directory).map_err(TunnelError::Endpoints)?;
        let endpoints = Endpoints::load_or_create(&config.directory.join(ENDPOINTS_FILE))
            .map_err(TunnelError::Endpoints)?;
        let mut device_config = DeviceConfig::host(&config.name, config.interfaces);
        device_config.info.model = "Superposition".into();
        device_config.info.firmware = env!("CARGO_PKG_VERSION").into();
        device_config.endpoints = endpoints.decls(channels);
        let (commands_tx, commands_rx) = RingBuffer::new(COMMAND_RING);
        let (retired_tx, retired_rx) = RingBuffer::new(RETIRED_RING);
        let cycles = Arc::new(Cycles::default());
        let app = DeviceApp {
            session: None,
            commands: commands_rx,
            retired: retired_tx,
            cycles: Arc::clone(&cycles),
        };
        let device = Device::start(device_config, &config.directory, Box::new(app))?;
        Ok(Self {
            device: Mutex::new(device),
            link: Mutex::new(AppLink {
                commands: commands_tx,
                retired: retired_rx,
            }),
            cycles,
            endpoints,
            channels,
            name: config.name.clone(),
            id: AudioDeviceId::new(format!("{DEVICE_ID_PREFIX}{}", config.name)),
        })
    }

    /// The stable audio device ID: `tunnel:<device name>`.
    pub const fn id(&self) -> &AudioDeviceId {
        &self.id
    }

    /// The device as the app lists it.
    pub fn info(&self) -> AudioDeviceInfo {
        AudioDeviceInfo {
            id: self.id.clone(),
            name: format!("Tunnel ({})", self.name),
            max_output_channels: self.channels.outputs,
        }
    }

    /// The endpoint channel counts. The system's routes decide which carry audio.
    pub const fn capabilities(&self) -> AudioDeviceCapabilities {
        AudioDeviceCapabilities {
            max_input_channels: self.channels.inputs,
            max_output_channels: self.channels.outputs,
            is_default_input: false,
            is_default_output: false,
        }
    }

    /// The IDs of "Superposition inputs" and "Superposition outputs".
    pub const fn endpoints(&self) -> Endpoints {
        self.endpoints
    }

    /// The declared channel counts.
    pub const fn channels(&self) -> TunnelChannels {
        self.channels
    }

    /// Adoption, PTP clock, registration, media statistics and cycles now.
    pub fn status(&self) -> TunnelStatus {
        let frames = self.cycles.frames.load(Relaxed);
        TunnelStatus {
            device: lock(&self.device).status(),
            cycles: self.cycles.count.load(Relaxed),
            cycle_frames: (frames > 0).then_some(frames),
        }
    }

    /// Gives a session's network side to the network thread, or detaches the current one with
    /// `None`. The network thread installs it at its next cycle. Returns `false`, and drops
    /// the command, when the network thread has not taken the earlier changes (it runs no
    /// cycles).
    pub(crate) fn attach(&self, command: Option<NetworkSide>) -> bool {
        lock(&self.link).send(command)
    }
}

/// Locks control-thread state. A panic while it was locked left no broken state.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
