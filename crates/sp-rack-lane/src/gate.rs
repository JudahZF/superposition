//! The rack gate: one live ticket per lane, and deterministic fallback after a failure.

use sp_shared_memory::{BlockTicket, ProtocolError};

/// Allocation-free state for one rack's real-time worker gate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RackGateState {
    /// The rack may publish its next request.
    Open,
    /// The rack is waiting for exactly one published ticket.
    Awaiting {
        /// Ticket whose completion may be accepted.
        ticket: BlockTicket,
        /// Host block index which published the request.
        dispatch_block: u64,
    },
    /// The rack is latched to deterministic fallback until its worker and bank are replaced.
    Closed {
        /// Failure which closed the gate.
        reason: FallbackReason,
        /// Host block index which observed the failure.
        closed_block: u64,
    },
}

/// One nonblocking observation supplied to a [`RackGate`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerObservation {
    /// No matching completion was visible during this bounded observation.
    Pending,
    /// A completion was validated and consumed for the supplied ticket.
    Completed(BlockTicket),
    /// Shared protocol validation rejected the worker result.
    ProtocolFault(ProtocolError),
    /// The control plane observed that the worker exited or became unavailable.
    WorkerExited,
}

/// Rack-local reasons for selecting deterministic fallback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FallbackReason {
    /// The expected completion was absent at its deadline.
    DeadlineMiss,
    /// The worker exited or failed its liveness contract.
    WorkerExited,
    /// Completion metadata exceeded a fixed protocol capacity.
    MalformedCompletion,
    /// A completion generation or sequence did not match the live request.
    StaleCompletion,
    /// The protocol reported another invalid state or ownership transition.
    InvalidProtocolState,
    /// A previous request was still pending when a new dispatch was attempted.
    SlotUnavailable,
}

/// Result of one bounded gate operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GateOutcome {
    /// A request was accepted as the rack's single live ticket.
    DispatchAllowed,
    /// No completion was visible; the caller may continue the block without waiting.
    Awaiting,
    /// The expected worker result was accepted exactly once.
    WorkerResultAccepted,
    /// The rack must render its configured fallback for this and later blocks.
    UseFallback(FallbackReason),
}

/// Deterministic, nonblocking state machine for one isolated rack worker.
///
/// The gate accepts at most one live ticket. A deadline, worker loss, or protocol fault
/// closes only this rack and permanently rejects late results. Reopening requires an
/// explicit control-thread reset after the old worker is reaped and a fresh bank is
/// installed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RackGate {
    state: RackGateState,
    consecutive_deadline_misses: u8,
    expired_ticket: Option<BlockTicket>,
}

impl RackGate {
    /// Creates an open gate with no live request.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: RackGateState::Open,
            consecutive_deadline_misses: 0,
            expired_ticket: None,
        }
    }

    /// Returns the current fixed-size gate state.
    #[must_use]
    pub const fn state(self) -> RackGateState {
        self.state
    }

    /// Records one published ticket without waiting for its completion.
    #[must_use]
    pub fn dispatch(&mut self, ticket: BlockTicket, block_index: u64) -> GateOutcome {
        match self.state {
            RackGateState::Open if ticket.is_valid() => {
                self.state = RackGateState::Awaiting {
                    ticket,
                    dispatch_block: block_index,
                };
                GateOutcome::DispatchAllowed
            }
            RackGateState::Open => self.close(FallbackReason::InvalidProtocolState, block_index),
            RackGateState::Awaiting { .. } => {
                self.close(FallbackReason::SlotUnavailable, block_index)
            }
            RackGateState::Closed { reason, .. } => GateOutcome::UseFallback(reason),
        }
    }

    /// Applies one bounded worker observation to the live ticket.
    #[must_use]
    pub fn observe(&mut self, observation: WorkerObservation, block_index: u64) -> GateOutcome {
        match self.state {
            RackGateState::Closed { reason, .. } => GateOutcome::UseFallback(reason),
            RackGateState::Open => match observation {
                WorkerObservation::WorkerExited => {
                    self.close(FallbackReason::WorkerExited, block_index)
                }
                // A completion for a ticket that already expired is late, not a protocol
                // violation: the block was rendered with fallback and the gate stays usable.
                WorkerObservation::Completed(completed)
                    if self.expired_ticket == Some(completed) =>
                {
                    self.expired_ticket = None;
                    GateOutcome::UseFallback(FallbackReason::DeadlineMiss)
                }
                _ => self.close(FallbackReason::InvalidProtocolState, block_index),
            },
            RackGateState::Awaiting { ticket, .. } => match observation {
                WorkerObservation::Pending => GateOutcome::Awaiting,
                WorkerObservation::Completed(completed) if completed == ticket => {
                    self.state = RackGateState::Open;
                    self.consecutive_deadline_misses = 0;
                    GateOutcome::WorkerResultAccepted
                }
                WorkerObservation::Completed(completed)
                    if self.expired_ticket == Some(completed) =>
                {
                    self.expired_ticket = None;
                    GateOutcome::UseFallback(FallbackReason::DeadlineMiss)
                }
                WorkerObservation::Completed(_) => {
                    self.close(FallbackReason::StaleCompletion, block_index)
                }
                WorkerObservation::ProtocolFault(error) => {
                    self.close(fallback_reason(error), block_index)
                }
                WorkerObservation::WorkerExited => {
                    self.close(FallbackReason::WorkerExited, block_index)
                }
            },
        }
    }

    /// Falls back for an expired block and closes after three consecutive misses.
    #[must_use]
    pub fn deadline_expired(&mut self, block_index: u64) -> GateOutcome {
        match self.state {
            RackGateState::Awaiting { ticket, .. } => {
                self.consecutive_deadline_misses =
                    self.consecutive_deadline_misses.saturating_add(1);
                self.expired_ticket = Some(ticket);
                if self.consecutive_deadline_misses >= 3 {
                    self.close(FallbackReason::DeadlineMiss, block_index)
                } else {
                    self.state = RackGateState::Open;
                    GateOutcome::UseFallback(FallbackReason::DeadlineMiss)
                }
            }
            RackGateState::Closed { reason, .. } => GateOutcome::UseFallback(reason),
            RackGateState::Open => self.close(FallbackReason::InvalidProtocolState, block_index),
        }
    }

    /// Closes this rack immediately for a hard deadline failure, such as an active device
    /// callback exhausting its completion budget. Unlike [`RackGate::deadline_expired`], no
    /// consecutive-miss tolerance applies: the abandoned slot cannot be reused, so the rack
    /// stays latched to fallback until the control plane replaces its worker and bank.
    #[must_use]
    pub fn deadline_expired_hard(&mut self, block_index: u64) -> GateOutcome {
        match self.state {
            RackGateState::Closed { reason, .. } => GateOutcome::UseFallback(reason),
            RackGateState::Awaiting { ticket, .. } => {
                self.expired_ticket = Some(ticket);
                self.close(FallbackReason::DeadlineMiss, block_index)
            }
            RackGateState::Open => self.close(FallbackReason::InvalidProtocolState, block_index),
        }
    }

    /// Closes this rack after an off-thread liveness monitor observes worker loss.
    #[must_use]
    pub fn worker_lost(&mut self, block_index: u64) -> GateOutcome {
        match self.state {
            RackGateState::Closed { reason, .. } => GateOutcome::UseFallback(reason),
            RackGateState::Open | RackGateState::Awaiting { .. } => {
                self.close(FallbackReason::WorkerExited, block_index)
            }
        }
    }

    /// Reopens the gate after the control plane installs a fresh worker and bank.
    pub fn reset_after_replacement(&mut self) {
        self.state = RackGateState::Open;
        self.consecutive_deadline_misses = 0;
        self.expired_ticket = None;
    }

    fn close(&mut self, reason: FallbackReason, block_index: u64) -> GateOutcome {
        self.state = RackGateState::Closed {
            reason,
            closed_block: block_index,
        };
        GateOutcome::UseFallback(reason)
    }
}

impl Default for RackGate {
    fn default() -> Self {
        Self::new()
    }
}

const fn fallback_reason(error: ProtocolError) -> FallbackReason {
    match error {
        ProtocolError::MalformedCompletion => FallbackReason::MalformedCompletion,
        ProtocolError::StaleTicket | ProtocolError::StaleCompletion => {
            FallbackReason::StaleCompletion
        }
        ProtocolError::InvalidState
        | ProtocolError::InvalidRequest
        | ProtocolError::InvalidTicket
        | ProtocolError::InvalidTimestamp
        | ProtocolError::InvalidOwner
        | ProtocolError::UnexpectedState
        | ProtocolError::Owned
        | ProtocolError::NotOwner => FallbackReason::InvalidProtocolState,
    }
}
