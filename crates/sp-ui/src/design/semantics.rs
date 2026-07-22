//! Renderer-independent accessibility semantics and keyboard interaction contracts.

/// The semantic role exposed to AccessKit, `VoiceOver`, and other accessibility bridges.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessibilityRole {
    /// A command invokes an action.
    Button,
    /// A binary setting can be enabled or disabled.
    Switch,
    /// A continuous or discrete numeric value can be adjusted.
    Slider,
    /// A mutually exclusive set of choices.
    RadioGroup,
    /// A single choice within a radio group.
    Radio,
    /// Informational live status that does not take focus.
    Status,
    /// An urgent message announced when it appears.
    Alert,
    /// A bounded value that conveys progress or level.
    ProgressBar,
}

/// A numeric range announced for an adjustable or bounded value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AccessibilityRange {
    /// Inclusive minimum value.
    pub minimum: f64,
    /// Inclusive maximum value.
    pub maximum: f64,
    /// Keyboard adjustment increment.
    pub step: f64,
}

/// State flags announced in addition to a component's value.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct AccessibilityState {
    /// Whether interaction is unavailable.
    pub disabled: bool,
    /// Whether this item is the selected item in its group.
    pub selected: bool,
    /// Whether an operation is underway.
    pub busy: bool,
    /// Whether the component is pressed or engaged.
    pub pressed: bool,
    /// Whether an error or fault requires attention.
    pub fault: bool,
}

/// A deterministic place in keyboard traversal order.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct FocusOrder(pub u16);

/// Keyboard action understood by a component model.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyboardAction {
    /// Move to the previous item in an ordered collection.
    Previous,
    /// Move to the next item in an ordered collection.
    Next,
    /// Activate or toggle the focused item.
    Toggle,
    /// Open or activate the focused item.
    Open,
    /// Close, dismiss, or cancel the current item.
    Close,
    /// Decrease the value by one normal step.
    Decrease,
    /// Increase the value by one normal step.
    Increase,
    /// Decrease the value by a fine step while Shift is held.
    FineDecrease,
    /// Increase the value by a fine step while Shift is held.
    FineIncrease,
    /// Move an adjustable value to its minimum.
    Minimum,
    /// Move an adjustable value to its maximum.
    Maximum,
    /// Reset to the defined default by double click or an equivalent command.
    Reset,
}

/// Complete semantics for one focusable or announced UI element.
#[derive(Clone, Debug, PartialEq)]
pub struct AccessibilityNode {
    /// Semantic control role.
    pub role: AccessibilityRole,
    /// Stable user-facing name. Renderers must retain this full value when text truncates.
    pub name: String,
    /// Optional formatted current value.
    pub value: Option<String>,
    /// Optional numeric range.
    pub range: Option<AccessibilityRange>,
    /// Announced interaction state.
    pub state: AccessibilityState,
    /// Deterministic focus traversal order. `None` means the element is not focusable.
    pub focus_order: Option<FocusOrder>,
    /// Supported keyboard equivalents.
    pub keyboard_actions: &'static [KeyboardAction],
}

impl AccessibilityNode {
    /// Creates a node with no value, range, state flags, or focus target.
    #[must_use]
    pub fn named(role: AccessibilityRole, name: impl Into<String>) -> Self {
        Self {
            role,
            name: name.into(),
            value: None,
            range: None,
            state: AccessibilityState::default(),
            focus_order: None,
            keyboard_actions: &[],
        }
    }
}

/// Produces explicit accessibility semantics for a renderer-independent component model.
pub trait AccessibleComponent {
    /// Returns role, name, value, range, state, focus order, and keyboard actions.
    fn accessibility(&self, focus_order: FocusOrder) -> AccessibilityNode;
}
