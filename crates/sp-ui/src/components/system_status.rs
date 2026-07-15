//! A compact model for the application's global status indicator.

use std::fmt;

/// The state shown by [`SystemStatus`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SystemStatusState {
    /// All required subsystems are ready.
    Online,
    /// Startup or reconnection is in progress.
    Connecting,
    /// A required subsystem is unavailable.
    Offline,
}

impl SystemStatusState {
    /// Returns the short UI label for this state.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Online => "Online",
            Self::Connecting => "Connecting",
            Self::Offline => "Offline",
        }
    }
}

impl fmt::Display for SystemStatusState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// Global status indicator model.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SystemStatus {
    /// Current state.
    pub state: SystemStatusState,
}

impl SystemStatus {
    /// Creates a status model in `state`.
    #[must_use]
    pub const fn new(state: SystemStatusState) -> Self {
        Self { state }
    }
    /// Updates the displayed state.
    pub fn set_state(&mut self, state: SystemStatusState) {
        self.state = state;
    }
}

#[cfg(test)]
mod tests {
    use super::{SystemStatus, SystemStatusState};
    #[test]
    fn transitions_and_labels_are_explicit() {
        let mut status = SystemStatus::new(SystemStatusState::Connecting);
        status.set_state(SystemStatusState::Online);
        assert_eq!(status.state.to_string(), "Online");
    }
}
