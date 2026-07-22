//! Shared interaction-state wrapper for renderer component models.

use crate::design::{
    AccessibilityNode, AccessibleComponent, FocusOrder, Interaction, SemanticPairing, Theme,
};

/// A component model together with its explicit visual interaction state and focus position.
///
/// Application renderers own pointer events and update `interaction`; this wrapper guarantees
/// that every component consumes the same centralized semantic pairing and accessibility model.
#[derive(Clone, Debug, PartialEq)]
pub struct ComponentPresentation<T> {
    /// Component-specific data.
    pub component: T,
    /// Default, hover, pressed, focused, disabled, selected, loading, success, or fault state.
    pub interaction: Interaction,
    /// Deterministic keyboard traversal position.
    pub focus_order: FocusOrder,
}

impl<T> ComponentPresentation<T> {
    /// Creates an actionable component in its default interaction state.
    #[must_use]
    pub fn new(component: T, focus_order: FocusOrder) -> Self {
        Self {
            component,
            interaction: Interaction::Default,
            focus_order,
        }
    }

    /// Returns the centralized semantic pairing for this interaction state.
    #[must_use]
    pub const fn pairing(&self, theme: Theme) -> SemanticPairing {
        theme.interaction_pairing(self.interaction)
    }

    /// Replaces the interaction state after renderer event handling.
    pub fn set_interaction(&mut self, interaction: Interaction) {
        self.interaction = interaction;
    }
}

impl<T: AccessibleComponent> ComponentPresentation<T> {
    /// Returns complete role/name/value/state/range/focus/keyboard semantics for this component.
    #[must_use]
    pub fn accessibility(&self) -> AccessibilityNode {
        let mut node = self.component.accessibility(self.focus_order);
        match self.interaction {
            Interaction::Disabled => node.state.disabled = true,
            Interaction::Selected => node.state.selected = true,
            Interaction::Pressed | Interaction::Active => node.state.pressed = true,
            Interaction::Loading => node.state.busy = true,
            Interaction::Fault => node.state.fault = true,
            Interaction::Default
            | Interaction::Hover
            | Interaction::Hovered
            | Interaction::Focused
            | Interaction::Success
            | Interaction::Inactive => {}
        }
        node
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        components::{ComponentPresentation, RackCardState, RackCardStatus},
        design::{DARK, FocusOrder, Interaction, SemanticPairing, Theme},
    };

    #[test]
    fn wrapper_carries_visual_and_accessibility_states_together() {
        let mut presentation = ComponentPresentation::new(
            RackCardState {
                name: "Main".to_owned(),
                status: RackCardStatus::Active,
            },
            FocusOrder(3),
        );
        presentation.set_interaction(Interaction::Focused);
        assert_eq!(presentation.pairing(DARK), SemanticPairing::Focused);
        assert_eq!(
            presentation.accessibility().focus_order,
            Some(FocusOrder(3))
        );
        assert_eq!(Theme, DARK);
    }
}
