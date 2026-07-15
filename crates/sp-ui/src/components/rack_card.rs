//! A model for a rack card in the main workspace.

use std::fmt;

/// The presentation state of a [`RackCardState`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RackCardStatus {
    /// The rack is active.
    Active,
    /// The rack is bypassed.
    Bypassed,
    /// The rack has no plug-in slots.
    Empty,
}

impl RackCardStatus {
    /// Returns the short UI label for this state.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Active => "Active",
            Self::Bypassed => "Bypassed",
            Self::Empty => "Empty",
        }
    }
}

impl fmt::Display for RackCardStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// Rack card data for a renderer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RackCardState {
    /// User-visible rack name.
    pub name: String,
    /// Current presentation state.
    pub status: RackCardStatus,
}

impl RackCardState {
    /// Updates the presentation state.
    pub fn set_status(&mut self, status: RackCardStatus) {
        self.status = status;
    }
}

#[cfg(test)]
mod tests {
    use super::{RackCardState, RackCardStatus};
    #[test]
    fn rack_state_transitions_have_labels() {
        let mut rack = RackCardState {
            name: String::from("Main"),
            status: RackCardStatus::Empty,
        };
        rack.set_status(RackCardStatus::Active);
        assert_eq!(rack.status.to_string(), "Active");
    }
}
