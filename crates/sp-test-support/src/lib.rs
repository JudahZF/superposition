//! Deterministic primitives shared by workspace tests and feasibility tools.

use std::{fmt, str::FromStr, time::Duration};

/// Command-line option selecting a feasibility fault mode.
pub const FAULT_MODE_OPTION: &str = "--fault-mode";
/// Command-line option selecting the exact request sequence which triggers a fault.
pub const FAULT_TRIGGER_SEQUENCE_OPTION: &str = "--fault-trigger-sequence";
/// Command-line option specifying fault-specific delay in microseconds.
pub const FAULT_DELAY_MICROS_OPTION: &str = "--fault-delay-micros";
/// Command-line option specifying simulated work duration in microseconds.
pub const WORK_DURATION_MICROS_OPTION: &str = "--work-duration-micros";
/// Maximum accepted fault delay or simulated work duration.
pub const MAX_FEASIBILITY_DURATION: Duration = Duration::from_mins(1);

/// Deterministic fault selected for one feasibility worker.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FaultMode {
    /// Process every request normally.
    #[default]
    None,
    /// Claim the triggering request and then stop making progress.
    HangAfterClaim,
    /// Delay the triggering request before claiming it.
    DelayBeforeClaim,
    /// Claim and process the triggering request, then publish its completion late.
    LateCompletion,
    /// Publish bounded audio with deliberately invalid completion metadata.
    MalformedCompletion,
    /// Publish bounded audio under a deliberately mismatched completion ticket.
    StaleCompletion,
}

impl FaultMode {
    /// Returns the stable command-line spelling for this mode.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::HangAfterClaim => "hang-after-claim",
            Self::DelayBeforeClaim => "delay-before-claim",
            Self::LateCompletion => "late-completion",
            Self::MalformedCompletion => "malformed-completion",
            Self::StaleCompletion => "stale-completion",
        }
    }
}

impl FromStr for FaultMode {
    type Err = FaultConfigurationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "none" => Ok(Self::None),
            "hang-after-claim" => Ok(Self::HangAfterClaim),
            "delay-before-claim" => Ok(Self::DelayBeforeClaim),
            "late-completion" => Ok(Self::LateCompletion),
            "malformed-completion" => Ok(Self::MalformedCompletion),
            "stale-completion" => Ok(Self::StaleCompletion),
            _ => Err(FaultConfigurationError::new(format!(
                "unknown fault mode `{value}`"
            ))),
        }
    }
}

/// Validated deterministic fault and work settings for a feasibility worker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FaultConfiguration {
    /// Fault applied to the exact configured request sequence.
    pub mode: FaultMode,
    /// Nonzero request sequence which triggers `mode`.
    pub trigger_request_sequence: u64,
    /// Fault-specific delay used before claim or before late completion.
    pub fault_delay: Duration,
    /// Simulated processing duration applied to every claimed request.
    pub work_duration: Duration,
}

impl FaultConfiguration {
    /// Validates trigger and duration bounds plus mode-specific delay rules.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero trigger, an excessive duration, a missing delay for
    /// a delay mode, or a delay supplied to a mode which does not use it.
    pub fn validate(self) -> Result<Self, FaultConfigurationError> {
        if self.trigger_request_sequence == 0 {
            return Err(FaultConfigurationError::new(
                "fault trigger request sequence must be nonzero",
            ));
        }
        validate_duration(WORK_DURATION_MICROS_OPTION, self.work_duration)?;
        validate_duration(FAULT_DELAY_MICROS_OPTION, self.fault_delay)?;
        match self.mode {
            FaultMode::DelayBeforeClaim | FaultMode::LateCompletion
                if self.fault_delay.is_zero() =>
            {
                Err(FaultConfigurationError::new(format!(
                    "fault mode `{}` requires a nonzero {FAULT_DELAY_MICROS_OPTION}",
                    self.mode.as_str()
                )))
            }
            FaultMode::None
            | FaultMode::HangAfterClaim
            | FaultMode::MalformedCompletion
            | FaultMode::StaleCompletion
                if !self.fault_delay.is_zero() =>
            {
                Err(FaultConfigurationError::new(format!(
                    "fault mode `{}` does not use {FAULT_DELAY_MICROS_OPTION}",
                    self.mode.as_str()
                )))
            }
            _ => Ok(self),
        }
    }

    /// Selects the configured fault only for the exact trigger sequence.
    #[must_use]
    pub const fn selected_mode(self, request_sequence: u64) -> FaultMode {
        if request_sequence == self.trigger_request_sequence {
            self.mode
        } else {
            FaultMode::None
        }
    }
}

impl Default for FaultConfiguration {
    fn default() -> Self {
        Self {
            mode: FaultMode::None,
            trigger_request_sequence: 1,
            fault_delay: Duration::ZERO,
            work_duration: Duration::ZERO,
        }
    }
}

/// Error returned while parsing or validating feasibility fault options.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FaultConfigurationError {
    message: String,
}

impl FaultConfigurationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for FaultConfigurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for FaultConfigurationError {}

/// Parses only the shared feasibility fault options from alternating option/value tokens.
///
/// Omitted options use [`FaultConfiguration::default`]. Callers may pass these tokens
/// through from a larger command parser used by either the worker or `xtask`.
///
/// # Errors
///
/// Returns an error for unknown or duplicate options, missing values, invalid numbers,
/// or a configuration rejected by [`FaultConfiguration::validate`].
pub fn parse_fault_configuration<I, S>(
    arguments: I,
) -> Result<FaultConfiguration, FaultConfigurationError>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut configuration = FaultConfiguration::default();
    let mut arguments = arguments.into_iter().map(Into::into);
    let mut mode_seen = false;
    let mut trigger_seen = false;
    let mut delay_seen = false;
    let mut work_seen = false;

    while let Some(option) = arguments.next() {
        let value = arguments.next().ok_or_else(|| {
            FaultConfigurationError::new(format!("missing value after `{option}`"))
        })?;
        match option.as_str() {
            FAULT_MODE_OPTION => {
                reject_duplicate(option.as_str(), &mut mode_seen)?;
                configuration.mode = value.parse()?;
            }
            FAULT_TRIGGER_SEQUENCE_OPTION => {
                reject_duplicate(option.as_str(), &mut trigger_seen)?;
                configuration.trigger_request_sequence = parse_u64(option.as_str(), &value)?;
            }
            FAULT_DELAY_MICROS_OPTION => {
                reject_duplicate(option.as_str(), &mut delay_seen)?;
                configuration.fault_delay =
                    Duration::from_micros(parse_u64(option.as_str(), &value)?);
            }
            WORK_DURATION_MICROS_OPTION => {
                reject_duplicate(option.as_str(), &mut work_seen)?;
                configuration.work_duration =
                    Duration::from_micros(parse_u64(option.as_str(), &value)?);
            }
            _ => {
                return Err(FaultConfigurationError::new(format!(
                    "unknown fault option `{option}`"
                )));
            }
        }
    }

    configuration.validate()
}

fn reject_duplicate(option: &str, seen: &mut bool) -> Result<(), FaultConfigurationError> {
    if *seen {
        Err(FaultConfigurationError::new(format!(
            "duplicate fault option `{option}`"
        )))
    } else {
        *seen = true;
        Ok(())
    }
}

fn parse_u64(option: &str, value: &str) -> Result<u64, FaultConfigurationError> {
    value.parse().map_err(|_| {
        FaultConfigurationError::new(format!("`{option}` requires an unsigned integer"))
    })
}

fn validate_duration(option: &str, duration: Duration) -> Result<(), FaultConfigurationError> {
    if duration > MAX_FEASIBILITY_DURATION {
        Err(FaultConfigurationError::new(format!(
            "`{option}` must not exceed {} microseconds",
            MAX_FEASIBILITY_DURATION.as_micros()
        )))
    } else {
        Ok(())
    }
}

/// A manually advanced clock for tests that must not read wall-clock time.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DeterministicClock {
    frame: u64,
}

impl DeterministicClock {
    /// Creates a clock at frame zero.
    #[must_use]
    pub const fn new() -> Self {
        Self { frame: 0 }
    }

    /// Returns the current frame.
    #[must_use]
    pub const fn frame(self) -> u64 {
        self.frame
    }

    /// Advances the clock by a known number of frames.
    pub fn advance(&mut self, frames: u64) {
        self.frame = self.frame.saturating_add(frames);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advances_without_wall_clock() {
        let mut clock = DeterministicClock::new();
        clock.advance(128);
        assert_eq!(clock.frame(), 128);
    }

    #[test]
    fn parses_every_fault_mode_and_shared_duration() {
        for mode in [
            FaultMode::None,
            FaultMode::HangAfterClaim,
            FaultMode::MalformedCompletion,
            FaultMode::StaleCompletion,
        ] {
            let configuration = parse_fault_configuration([
                FAULT_MODE_OPTION,
                mode.as_str(),
                FAULT_TRIGGER_SEQUENCE_OPTION,
                "17",
                WORK_DURATION_MICROS_OPTION,
                "250",
            ])
            .unwrap();
            assert_eq!(configuration.mode, mode);
            assert_eq!(configuration.trigger_request_sequence, 17);
            assert_eq!(configuration.work_duration, Duration::from_micros(250));
        }

        for mode in [FaultMode::DelayBeforeClaim, FaultMode::LateCompletion] {
            let configuration = parse_fault_configuration([
                FAULT_MODE_OPTION,
                mode.as_str(),
                FAULT_TRIGGER_SEQUENCE_OPTION,
                "9",
                FAULT_DELAY_MICROS_OPTION,
                "500",
            ])
            .unwrap();
            assert_eq!(configuration.mode, mode);
            assert_eq!(configuration.fault_delay, Duration::from_micros(500));
        }
    }

    #[test]
    fn selection_uses_exact_request_sequence_equality() {
        let configuration = FaultConfiguration {
            mode: FaultMode::HangAfterClaim,
            trigger_request_sequence: 12,
            ..FaultConfiguration::default()
        };
        assert_eq!(configuration.selected_mode(11), FaultMode::None);
        assert_eq!(configuration.selected_mode(12), FaultMode::HangAfterClaim);
        assert_eq!(configuration.selected_mode(13), FaultMode::None);
    }

    #[test]
    fn rejects_invalid_and_ambiguous_options() {
        assert!(parse_fault_configuration([FAULT_MODE_OPTION, "unknown"]).is_err());
        assert!(parse_fault_configuration([FAULT_MODE_OPTION]).is_err());
        assert!(
            parse_fault_configuration([FAULT_MODE_OPTION, "none", FAULT_MODE_OPTION, "none"])
                .is_err()
        );
        assert!(parse_fault_configuration(["--other", "1"]).is_err());
        assert!(
            parse_fault_configuration([FAULT_TRIGGER_SEQUENCE_OPTION, "not-a-number"]).is_err()
        );
    }

    #[test]
    fn validates_trigger_delay_rules_and_duration_bounds() {
        assert!(parse_fault_configuration([FAULT_TRIGGER_SEQUENCE_OPTION, "0"]).is_err());
        assert!(parse_fault_configuration([FAULT_MODE_OPTION, "delay-before-claim"]).is_err());
        assert!(
            parse_fault_configuration([
                FAULT_MODE_OPTION,
                "hang-after-claim",
                FAULT_DELAY_MICROS_OPTION,
                "1",
            ])
            .is_err()
        );
        assert!(parse_fault_configuration([WORK_DURATION_MICROS_OPTION, "60000001",]).is_err());
        assert!(
            parse_fault_configuration([
                FAULT_MODE_OPTION,
                "late-completion",
                FAULT_DELAY_MICROS_OPTION,
                "60000001",
            ])
            .is_err()
        );
    }
}
