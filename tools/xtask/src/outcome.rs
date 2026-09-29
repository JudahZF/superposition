//! Shared result types for xtask commands.

/// Successful command completion with a process exit status.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CommandOutcome {
    pub(crate) exit_code: u8,
}

impl CommandOutcome {
    pub(crate) const fn passed() -> Self {
        Self { exit_code: 0 }
    }

    pub(crate) const fn acceptance_failure() -> Self {
        Self { exit_code: 1 }
    }
}

/// Distinguishes invalid input and unavailable infrastructure from an acceptance failure.
///
/// Both variants exit with status 2 so scripts can tell them apart from a failed check.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CommandError {
    InvalidConfiguration(String),
    Infrastructure(String),
}

impl CommandError {
    pub(crate) const EXIT_CODE: u8 = 2;
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidConfiguration(message) | Self::Infrastructure(message) => {
                formatter.write_str(message)
            }
        }
    }
}
