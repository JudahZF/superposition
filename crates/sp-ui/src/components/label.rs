//! Deterministic long-name presentation that preserves full accessible names.

/// Text prepared for a bounded visual label and an untruncated accessibility name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TruncatedLabel {
    /// Full string provided to accessibility APIs and tooltips.
    pub accessible_name: String,
    /// Text the renderer should draw in the constrained label area.
    pub visible_text: String,
    /// Whether visual text was shortened.
    pub truncated: bool,
}

impl TruncatedLabel {
    /// Creates a label that fits within `max_characters` Unicode scalar values.
    ///
    /// This is a deterministic fixture and model limit; renderers should additionally apply
    /// pixel-width ellipsis according to their active font metrics.
    #[must_use]
    pub fn new(name: impl Into<String>, max_characters: usize) -> Self {
        let accessible_name = name.into();
        let character_count = accessible_name.chars().count();
        if character_count <= max_characters {
            return Self {
                visible_text: accessible_name.clone(),
                accessible_name,
                truncated: false,
            };
        }

        let retained = max_characters.saturating_sub(1);
        let visible_text = accessible_name
            .chars()
            .take(retained)
            .chain(std::iter::once('…'))
            .collect();
        Self {
            accessible_name,
            visible_text,
            truncated: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::TruncatedLabel;

    #[test]
    fn long_names_keep_their_full_accessible_value() {
        let label = TruncatedLabel::new("A very long rack name", 8);
        assert_eq!(label.visible_text, "A very …");
        assert_eq!(label.accessible_name, "A very long rack name");
        assert!(label.truncated);
    }
}
