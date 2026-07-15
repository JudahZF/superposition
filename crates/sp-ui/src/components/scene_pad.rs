//! A model for a scene trigger pad.

use std::fmt;

/// The presentation state of a [`ScenePadState`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScenePadStatus {
    /// The scene is available but inactive.
    Idle,
    /// The scene is currently active.
    Active,
    /// The scene is being recalled.
    Recalling,
}

impl ScenePadStatus {
    /// Returns the short UI label for this state.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Idle => "Idle",
            Self::Active => "Active",
            Self::Recalling => "Recalling",
        }
    }
}

impl fmt::Display for ScenePadStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// Scene pad data for a renderer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScenePadState {
    /// User-visible scene name.
    pub name: String,
    /// Current presentation state.
    pub status: ScenePadStatus,
}

impl ScenePadState {
    /// Marks this scene as active.
    pub fn activate(&mut self) {
        self.status = ScenePadStatus::Active;
    }
}

#[cfg(test)]
mod tests {
    use super::{ScenePadState, ScenePadStatus};
    #[test]
    fn scene_can_become_active() {
        let mut scene = ScenePadState {
            name: String::from("Verse"),
            status: ScenePadStatus::Idle,
        };
        scene.activate();
        assert_eq!(scene.status.to_string(), "Active");
    }
}
