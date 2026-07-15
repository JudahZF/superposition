//! A model for a banner describing a recoverable or blocking fault.

use std::fmt;

/// The visibility state of a [`FaultBannerModel`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultBannerState {
    /// No banner is shown.
    Hidden,
    /// An issue is shown and can be dismissed.
    Visible,
}

impl FaultBannerState {
    /// Returns the short UI label for this state.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Hidden => "Hidden",
            Self::Visible => "Visible",
        }
    }
}

impl fmt::Display for FaultBannerState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// Fault banner data for a renderer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FaultBannerModel {
    /// Current visibility state.
    pub state: FaultBannerState,
    /// Human-readable fault detail.
    pub message: String,
}

impl FaultBannerModel {
    /// Dismisses this banner.
    pub fn dismiss(&mut self) {
        self.state = FaultBannerState::Hidden;
    }
}

#[cfg(test)]
mod tests {
    use super::{FaultBannerModel, FaultBannerState};
    #[test]
    fn banner_can_be_dismissed() {
        let mut banner = FaultBannerModel {
            state: FaultBannerState::Visible,
            message: String::from("Worker stopped"),
        };
        banner.dismiss();
        assert_eq!(banner.state.to_string(), "Hidden");
    }
}
