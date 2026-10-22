//! Input for a worker-owned editor window, in `AppKit` terms.
//!
//! The types are plain data on every platform, so the mapping from remote input can be tested
//! anywhere. Only the macOS editor window injects them.

/// Modifier keys held during an event.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "one flag per modifier key, as AppKit's modifier mask"
)]
pub struct Modifiers {
    /// Shift.
    pub shift: bool,
    /// Control.
    pub control: bool,
    /// Option.
    pub option: bool,
    /// Command.
    pub command: bool,
}

/// A mouse button that `AppKit` mouse events carry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MouseButton {
    /// The primary button.
    Left,
    /// The secondary button.
    Right,
    /// The middle button, an `AppKit` "other" button.
    Middle,
}

/// One input event for an editor window. Positions are points from the top-left corner of the
/// window's content view.
#[derive(Clone, Debug, PartialEq)]
pub enum EditorInput {
    /// The pointer moved. With a held button this is a drag of that button.
    MouseMoved {
        /// Horizontal position.
        x: f64,
        /// Vertical position, downwards.
        y: f64,
        /// The held button, if any.
        dragging: Option<MouseButton>,
        /// Held modifiers.
        modifiers: Modifiers,
    },
    /// A button was pressed or released.
    MouseButton {
        /// Horizontal position.
        x: f64,
        /// Vertical position, downwards.
        y: f64,
        /// The button.
        button: MouseButton,
        /// Pressed or released.
        pressed: bool,
        /// 1 for a single click, 2 for a double click, and so on.
        click_count: u32,
        /// Held modifiers.
        modifiers: Modifiers,
    },
    /// The wheel turned by whole lines. Positive is down and right.
    Scroll {
        /// Horizontal position.
        x: f64,
        /// Vertical position, downwards.
        y: f64,
        /// Horizontal lines.
        dx: i32,
        /// Vertical lines.
        dy: i32,
        /// Held modifiers.
        modifiers: Modifiers,
    },
    /// A key that types `characters`: printable text, or an `AppKit` function-key character
    /// such as `NSUpArrowFunctionKey`.
    Key {
        /// The virtual key code (`kVK_*`).
        key_code: u16,
        /// The characters the key types.
        characters: String,
        /// Pressed or released.
        pressed: bool,
        /// Held modifiers.
        modifiers: Modifiers,
    },
    /// A modifier key changed.
    ModifiersChanged {
        /// The modifier key's virtual key code (`kVK_*`).
        key_code: u16,
        /// The modifiers held after the change.
        modifiers: Modifiers,
    },
    /// Raise the window and make it the key window.
    Focus,
    /// Ask the window to close, as its close button does.
    Close,
}
