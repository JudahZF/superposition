//! Three-letter state tokens, always shown with their colour and never colour alone.

use crate::design::ColorToken;

/// The state of a rack or plug-in slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StateToken {
    /// Ready and processing.
    Ready,
    /// Loading, or its worker is recovering.
    Loading,
    /// Bypassed (slot or rack).
    Bypassed,
    /// The plug-in is missing on this machine.
    Missing,
    /// Worker recovery failed; the rack passes dry.
    Faulted,
    /// The rack's worker is recovering.
    Recovering,
    /// The engine is offline or the worker is not loaded.
    Unloaded,
}

impl StateToken {
    /// The uppercase token shown in the UI.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Ready => "RDY",
            Self::Loading => "LDG",
            Self::Bypassed => "BYP",
            Self::Missing => "MIS",
            Self::Faulted => "FLT",
            Self::Recovering => "REC",
            Self::Unloaded => "UNL",
        }
    }

    /// Plain-language meaning for hover text and assistive technology.
    #[must_use]
    pub const fn meaning(self) -> &'static str {
        match self {
            Self::Ready => "Ready and processing",
            Self::Loading => "Loading",
            Self::Bypassed => "Bypassed",
            Self::Missing => "Plug-in missing on this machine",
            Self::Faulted => "Worker recovery failed, rack passes dry",
            Self::Recovering => "Rack worker recovering",
            Self::Unloaded => "Not loaded",
        }
    }

    /// The token's colour.
    #[must_use]
    pub const fn color(self) -> ColorToken {
        match self {
            Self::Ready => ColorToken::Text,
            Self::Loading | Self::Recovering => ColorToken::Warn,
            Self::Bypassed | Self::Unloaded => ColorToken::Dim,
            Self::Missing | Self::Faulted => ColorToken::Fault,
        }
    }
}
