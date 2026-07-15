//! A model for the health of an isolated helper worker.

use std::fmt;

/// The presentation state of a [`WorkerHealth`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerHealthState {
    /// The worker is serving requests.
    Healthy,
    /// The worker is starting or recovering.
    Recovering,
    /// The worker is unavailable.
    Unavailable,
}

impl WorkerHealthState {
    /// Returns the short UI label for this state.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Healthy => "Healthy",
            Self::Recovering => "Recovering",
            Self::Unavailable => "Unavailable",
        }
    }
}

impl fmt::Display for WorkerHealthState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// Isolated worker health data for a renderer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerHealth {
    /// Current presentation state.
    pub state: WorkerHealthState,
}

impl WorkerHealth {
    /// Updates the worker health state.
    pub fn set_state(&mut self, state: WorkerHealthState) {
        self.state = state;
    }
}

#[cfg(test)]
mod tests {
    use super::{WorkerHealth, WorkerHealthState};
    #[test]
    fn worker_health_transitions_have_labels() {
        let mut health = WorkerHealth {
            state: WorkerHealthState::Recovering,
        };
        health.set_state(WorkerHealthState::Healthy);
        assert_eq!(health.state.to_string(), "Healthy");
    }
}
