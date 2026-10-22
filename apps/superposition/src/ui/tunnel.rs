//! Linux: the Tunnel network-audio device in Setup.
//!
//! Starting the device makes `tunnel:<name>` an audio device; routing to and from Lambda is done
//! on the Tunnel master. The device runs until stopped, because adoption and PTP lock take
//! seconds. Settings persist in `<app support>/tunnel.json`.

use std::{net::Ipv4Addr, path::PathBuf, sync::Arc};

use eframe::egui::{self, Ui};
use serde::{Deserialize, Serialize};
use sp_audio_io::BlockFrames;
use sp_audio_io_tunnel::{
    AddedLatency, Interface, Interfaces, TunnelChannels, TunnelConfig, TunnelDevice,
};
use sp_ui::design::{ColorToken, Spacing};

use super::{
    LiveRackApp,
    style::{action, note, sp},
};
use crate::platform_audio::{set_tunnel_device, tunnel_device};

/// What the user entered, as saved.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct TunnelSettings {
    name: String,
    /// Path A as `address/prefix`, for example `10.0.0.20/24`.
    path_a: String,
    /// Path B as `address/prefix`, or empty for a single-path device.
    path_b: String,
    channels: u16,
}

impl Default for TunnelSettings {
    fn default() -> Self {
        Self {
            name: "Superposition".to_owned(),
            path_a: String::new(),
            path_b: String::new(),
            channels: 16,
        }
    }
}

fn settings_path() -> PathBuf {
    crate::default_application_support().join("tunnel.json")
}

impl TunnelSettings {
    pub(crate) fn load() -> Self {
        std::fs::read(settings_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn save(&self) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(self).map_err(|error| error.to_string())?;
        std::fs::write(settings_path(), bytes)
            .map_err(|error| format!("cannot save Tunnel settings: {error}"))
    }

    fn config(&self) -> Result<TunnelConfig, String> {
        let parse = |text: &str| -> Result<Interface, String> {
            let (address, prefix) = text
                .trim()
                .split_once('/')
                .ok_or_else(|| format!("`{text}` is not address/prefix"))?;
            let address: Ipv4Addr = address
                .parse()
                .map_err(|error| format!("`{address}`: {error}"))?;
            let prefix: u8 = prefix
                .parse()
                .ok()
                .filter(|prefix| *prefix <= 32)
                .ok_or_else(|| format!("`{prefix}` is not a prefix length 0–32"))?;
            Ok(Interface { address, prefix })
        };
        let b = if self.path_b.trim().is_empty() {
            None
        } else {
            Some(parse(&self.path_b)?)
        };
        Ok(TunnelConfig {
            name: self.name.trim().to_owned(),
            interfaces: Interfaces {
                a: parse(&self.path_a)?,
                b,
            },
            directory: crate::default_application_support().join("tunnel"),
            channels: TunnelChannels {
                inputs: self.channels,
                outputs: self.channels,
            },
        })
    }
}

impl LiveRackApp {
    /// The Tunnel section of Setup → Audio. `locked` while the engine runs.
    pub(super) fn draw_tunnel(&mut self, ui: &mut Ui, locked: bool) {
        super::setup::section(ui, "Tunnel network audio");
        if let Some(device) = tunnel_device() {
            let status = device.status();
            let rate = status.sample_rate_hz();
            let state = match (rate, status.is_locked()) {
                (None, _) => "waiting for adoption by a Tunnel master".to_owned(),
                (Some(rate), false) => format!("adopted at {} kHz, PTP locking", rate / 1_000),
                (Some(rate), true) => format!("adopted at {} kHz, PTP locked", rate / 1_000),
            };
            note(ui, &format!("{}: {state}", device.id()), ColorToken::Text);
            if let (Some(rate), Some(block)) = (rate, BlockFrames::from_u32(self.buffer_frames)) {
                let latency = AddedLatency::new(block, rate);
                note(
                    ui,
                    &format!(
                        "A Lambda insert adds {} frames ({:.2} ms) plus the Tunnel flow latencies.",
                        latency.frames,
                        latency.duration().as_secs_f64() * 1_000.0
                    ),
                    ColorToken::Dim,
                );
            }
            ui.add_space(sp(Spacing::S8));
            if action(ui, "Stop Tunnel device", None, !locked).clicked() {
                set_tunnel_device(None);
                self.refresh_audio_devices();
            }
            return;
        }
        let settings = &mut self.tunnel_settings;
        egui::Grid::new("tunnel-settings")
            .num_columns(2)
            .spacing([sp(Spacing::S10), sp(Spacing::S4)])
            .show(ui, |ui| {
                ui.label("Device name");
                ui.text_edit_singleline(&mut settings.name);
                ui.end_row();
                ui.label("Path A (address/prefix)");
                ui.text_edit_singleline(&mut settings.path_a);
                ui.end_row();
                ui.label("Path B (optional)");
                ui.text_edit_singleline(&mut settings.path_b);
                ui.end_row();
                ui.label("Channels each way");
                ui.add(egui::DragValue::new(&mut settings.channels).range(1..=64));
                ui.end_row();
            });
        ui.add_space(sp(Spacing::S8));
        if action(ui, "Start Tunnel device", None, !locked).clicked() {
            let started = settings
                .config()
                .and_then(|config| TunnelDevice::start(&config).map_err(|error| error.to_string()))
                .and_then(|device| settings.save().map(|()| device));
            match started {
                Ok(device) => {
                    set_tunnel_device(Some(Arc::new(device)));
                    self.refresh_audio_devices();
                }
                Err(error) => self.show_fault(format!("Tunnel device did not start: {error}")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::TunnelSettings;

    #[test]
    fn paths_parse_as_address_and_prefix() {
        let settings = TunnelSettings {
            path_a: " 10.0.0.20/24 ".into(),
            ..TunnelSettings::default()
        };
        let config = settings.config().unwrap();
        assert_eq!(config.interfaces.a.address, Ipv4Addr::new(10, 0, 0, 20));
        assert_eq!(config.interfaces.a.prefix, 24);
        assert!(config.interfaces.b.is_none());
        for bad in ["10.0.0.20", "10.0.0.20/33", "host/24"] {
            let settings = TunnelSettings {
                path_a: bad.into(),
                ..TunnelSettings::default()
            };
            assert!(settings.config().is_err(), "{bad}");
        }
    }
}
