//! The device backend for this platform: AUHAL on macOS, JACK on Linux.

#[cfg(target_os = "linux")]
pub(crate) use sp_audio_io_linux::{
    LinuxAudioDevice as PlatformAudioDevice, LinuxAudioEndpoint as PlatformAudioEndpoint,
    enumerate_devices,
};
#[cfg(target_os = "macos")]
pub(crate) use sp_audio_io_macos::{
    MacOsAudioDevice as PlatformAudioDevice, MacOsAudioEndpoint as PlatformAudioEndpoint,
    enumerate_devices,
};
