//! A model for one plug-in slot inside a rack.

use std::fmt;

/// The presentation state of a [`PluginSlotState`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PluginSlotStatus {
    /// A plug-in is ready to process audio.
    Ready,
    /// A plug-in is loading.
    Loading,
    /// The slot is bypassed.
    Bypassed,
    /// Loading or processing failed.
    Faulted,
    /// The expected plug-in fingerprint is unavailable on this machine.
    Missing,
}

impl PluginSlotStatus {
    /// Returns the short UI label for this state.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Ready => "Ready",
            Self::Loading => "Loading",
            Self::Bypassed => "Bypassed",
            Self::Faulted => "Faulted",
            Self::Missing => "Missing",
        }
    }
}

impl fmt::Display for PluginSlotStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// Plug-in slot data for a renderer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginSlotState {
    /// User-visible plug-in name.
    pub name: String,
    /// Current presentation state.
    pub status: PluginSlotStatus,
}

impl PluginSlotState {
    /// Updates the presentation state.
    pub fn set_status(&mut self, status: PluginSlotStatus) {
        self.status = status;
    }
}

#[cfg(test)]
mod tests {
    use super::{PluginSlotState, PluginSlotStatus};
    #[test]
    fn plug_in_status_transitions_have_labels() {
        let mut slot = PluginSlotState {
            name: String::from("Compressor"),
            status: PluginSlotStatus::Loading,
        };
        slot.set_status(PluginSlotStatus::Ready);
        assert_eq!(slot.status.to_string(), "Ready");
    }
}
