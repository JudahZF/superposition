//! Controller input, injected with XTEST at root coordinates.
//!
//! XTEST input lands on whatever window is on screen under the pointer, and no window manager
//! stacks or focuses windows, so the target is raised (and, if partly off screen, moved fully on
//! screen) and focused before input reaches it.

use std::collections::HashMap;

use sp_window_stream::{ErrorCode, InputEvent, WindowId, WindowKind, keysym_for_char};
use x11rb::{
    CURRENT_TIME,
    connection::Connection,
    protocol::xproto::{
        AtomEnum, BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, ClientMessageEvent, ConfigureWindowAux,
        ConnectionExt as _, EventMask, InputFocus, KEY_PRESS_EVENT, KEY_RELEASE_EVENT,
        MOTION_NOTIFY_EVENT, StackMode,
    },
    protocol::xtest::ConnectionExt as _,
};

use crate::{XResult, display::Display};

const SHIFT_L: u32 = 0xffe1;
const SHIFT_R: u32 = 0xffe2;
/// Scratch keycodes rotated for keysyms the keymap lacks. Rotation keeps a remapped keycode
/// from being reused while a client may still be translating the previous key with it.
const MAX_SCRATCH_KEYCODES: usize = 16;

/// A rejected input event, reported to the controller.
pub type InputError = (ErrorCode, String);

/// The server keymap, plus keys the controller holds down.
pub struct Keyboard {
    first_keycode: u8,
    keysyms_per_keycode: u8,
    /// Keysym to keycode and whether Shift selects it (column 1).
    lookup: HashMap<u32, (u8, bool)>,
    scratch: Vec<u8>,
    next_scratch: usize,
    held: HashMap<u32, u8>,
}

impl Keyboard {
    pub fn load(display: &Display) -> XResult<Self> {
        let mut keyboard = Self {
            first_keycode: 0,
            keysyms_per_keycode: 0,
            lookup: HashMap::new(),
            scratch: Vec::new(),
            next_scratch: 0,
            held: HashMap::new(),
        };
        let empty = keyboard.reload(display)?;
        keyboard.scratch = empty.into_iter().rev().take(MAX_SCRATCH_KEYCODES).collect();
        Ok(keyboard)
    }

    /// Re-reads the keymap after a `MappingNotify`; returns keycodes with no keysyms.
    pub fn reload(&mut self, display: &Display) -> XResult<Vec<u8>> {
        let setup = display.conn.setup();
        let (first, last) = (setup.min_keycode, setup.max_keycode);
        let reply = display
            .conn
            .get_keyboard_mapping(first, last - first + 1)?
            .reply()?;
        let per = usize::from(reply.keysyms_per_keycode).max(1);
        self.first_keycode = first;
        self.keysyms_per_keycode = reply.keysyms_per_keycode;
        self.lookup.clear();
        let mut empty = Vec::new();
        for (keycode, syms) in (first..=last).zip(reply.keysyms.chunks(per)) {
            if syms.iter().all(|&s| s == 0) {
                empty.push(keycode);
            }
            for (column, &sym) in syms.iter().take(2).enumerate() {
                // Prefer the unshifted column and the lowest keycode.
                if sym != 0
                    && self
                        .lookup
                        .get(&sym)
                        .is_none_or(|&(_, shifted)| shifted && column == 0)
                {
                    self.lookup.insert(sym, (keycode, column == 1));
                }
            }
        }
        Ok(empty)
    }

    /// Finds or maps a keycode for `keysym`.
    fn keycode(&mut self, display: &Display, keysym: u32) -> Result<(u8, bool), InputError> {
        if let Some(&found) = self.lookup.get(&keysym) {
            return Ok(found);
        }
        let Some(&keycode) = self
            .scratch
            .get(self.next_scratch % self.scratch.len().max(1))
        else {
            return Err((
                ErrorCode::Unsupported,
                format!("no keycode for keysym {keysym:#x}"),
            ));
        };
        self.next_scratch += 1;
        let syms = vec![keysym; usize::from(self.keysyms_per_keycode)];
        let sent =
            display
                .conn
                .change_keyboard_mapping(1, keycode, self.keysyms_per_keycode, &syms);
        sent.map_err(internal)?;
        self.lookup.retain(|_, &mut (code, _)| code != keycode);
        self.lookup.insert(keysym, (keycode, false));
        Ok((keycode, false))
    }

    fn shift_held(&self) -> bool {
        self.held.contains_key(&SHIFT_L) || self.held.contains_key(&SHIFT_R)
    }

    fn key(&mut self, display: &Display, keysym: u32, pressed: bool) -> Result<(), InputError> {
        if !pressed {
            let keycode = match self.held.remove(&keysym) {
                Some(keycode) => keycode,
                None => self.keycode(display, keysym)?.0,
            };
            return fake(display, KEY_RELEASE_EVENT, keycode);
        }
        let (keycode, shifted) = self.keycode(display, keysym)?;
        let wrap = shifted && !self.shift_held();
        let shift = if wrap {
            Some(self.keycode(display, SHIFT_L)?.0)
        } else {
            None
        };
        if let Some(shift) = shift {
            fake(display, KEY_PRESS_EVENT, shift)?;
        }
        fake(display, KEY_PRESS_EVENT, keycode)?;
        if let Some(shift) = shift {
            fake(display, KEY_RELEASE_EVENT, shift)?;
        }
        self.held.insert(keysym, keycode);
        Ok(())
    }

    fn text(&mut self, display: &Display, text: &str) -> Result<(), InputError> {
        for c in text.chars() {
            let keysym = keysym_for_char(c);
            self.key(display, keysym, true)?;
            self.key(display, keysym, false)?;
        }
        Ok(())
    }
}

fn internal(error: impl std::fmt::Display) -> InputError {
    (ErrorCode::Internal, error.to_string())
}

fn fake(display: &Display, kind: u8, detail: u8) -> Result<(), InputError> {
    display
        .conn
        .xtest_fake_input(kind, detail, CURRENT_TIME, display.root, 0, 0, 0)
        .map_err(internal)?;
    Ok(())
}

/// Injects one controller event.
pub fn inject(
    display: &mut Display,
    keyboard: &mut Keyboard,
    event: &InputEvent,
) -> Result<(), InputError> {
    let id = event.window();
    if !display.windows.contains_key(&id) {
        return Err((
            ErrorCode::UnknownWindow,
            format!("window {:#x} is not streamed", id.0),
        ));
    }
    match *event {
        InputEvent::PointerMove { x, y, .. } => pointer(display, id, x, y),
        InputEvent::PointerButton {
            x,
            y,
            button,
            pressed,
            ..
        } => {
            pointer(display, id, x, y)?;
            let kind = if pressed {
                BUTTON_PRESS_EVENT
            } else {
                BUTTON_RELEASE_EVENT
            };
            fake(display, kind, button.x_button())
        }
        InputEvent::Scroll { x, y, dx, dy, .. } => {
            pointer(display, id, x, y)?;
            // Core wheel buttons: 4 up, 5 down, 6 left, 7 right.
            for (notches, negative, positive) in [(dy, 4, 5), (dx, 6, 7)] {
                let button = if notches < 0 { negative } else { positive };
                for _ in 0..notches.unsigned_abs() {
                    fake(display, BUTTON_PRESS_EVENT, button)?;
                    fake(display, BUTTON_RELEASE_EVENT, button)?;
                }
            }
            Ok(())
        }
        InputEvent::Key {
            keysym, pressed, ..
        } => {
            bring_to_front(display, id, false)?;
            keyboard.key(display, keysym, pressed)
        }
        InputEvent::Text { ref text, .. } => {
            bring_to_front(display, id, false)?;
            keyboard.text(display, text)
        }
        InputEvent::Focus { .. } => bring_to_front(display, id, true),
        InputEvent::Close { .. } => close(display, id),
    }?;
    display.conn.flush().map_err(internal)
}

fn pointer(display: &mut Display, id: WindowId, x: i16, y: i16) -> Result<(), InputError> {
    bring_to_front(display, id, false)?;
    let info = &display.windows[&id].info;
    let (root_x, root_y) = (info.x.saturating_add(x), info.y.saturating_add(y));
    let sent = display.conn.xtest_fake_input(
        MOTION_NOTIFY_EVENT,
        0,
        CURRENT_TIME,
        display.root,
        root_x,
        root_y,
        0,
    );
    sent.map_err(internal)?;
    Ok(())
}

/// Raises the target. A normal window is also moved fully on screen when it can be, focused,
/// and kept below every popup. Skipped when the target already is in front, unless `force`.
fn bring_to_front(display: &mut Display, id: WindowId, force: bool) -> Result<(), InputError> {
    let conn = display.conn.clone();
    let kind = display.windows[&id].info.kind;
    let raise = ConfigureWindowAux::new().stack_mode(StackMode::ABOVE);
    if kind == WindowKind::Popup {
        conn.configure_window(id.0, &raise).map_err(internal)?;
        return Ok(());
    }
    if display.front == Some(id) && !force {
        return Ok(());
    }
    let (screen_w, screen_h) = (
        i32::from(display.screen_width),
        i32::from(display.screen_height),
    );
    let info = &mut display
        .windows
        .get_mut(&id)
        .expect("checked by inject")
        .info;
    let clamp = |pos: i16, size: u16, screen: i32| {
        i16::try_from(i32::from(pos).min(screen - i32::from(size)).max(0)).unwrap_or(0)
    };
    let (x, y) = (
        clamp(info.x, info.width, screen_w),
        clamp(info.y, info.height, screen_h),
    );
    let mut aux = raise;
    if (x, y) != (info.x, info.y) {
        // Positions are of the content origin; the window border is ignored here because
        // editor windows on the private server have none.
        aux = aux.x(i32::from(x)).y(i32::from(y));
        info.x = x;
        info.y = y;
    }
    conn.configure_window(id.0, &aux).map_err(internal)?;
    for popup in display
        .windows
        .values()
        .filter(|w| w.info.kind == WindowKind::Popup)
    {
        conn.configure_window(popup.info.id.0, &raise)
            .map_err(internal)?;
    }
    conn.set_input_focus(InputFocus::PARENT, id.0, CURRENT_TIME)
        .map_err(internal)?;
    display.front = Some(id);
    Ok(())
}

fn close(display: &Display, id: WindowId) -> Result<(), InputError> {
    let conn = &display.conn;
    let atoms = &display.atoms;
    let protocols = conn
        .get_property(false, id.0, atoms.WM_PROTOCOLS, AtomEnum::ATOM, 0, 64)
        .map_err(internal)?
        .reply()
        .map_err(internal)?;
    if !protocols
        .value32()
        .into_iter()
        .flatten()
        .any(|atom| atom == atoms.WM_DELETE_WINDOW)
    {
        return Err((
            ErrorCode::Unsupported,
            "window does not accept WM_DELETE_WINDOW".into(),
        ));
    }
    let message = ClientMessageEvent::new(
        32,
        id.0,
        atoms.WM_PROTOCOLS,
        [atoms.WM_DELETE_WINDOW, CURRENT_TIME, 0, 0, 0],
    );
    conn.send_event(false, id.0, EventMask::NO_EVENT, message)
        .map_err(internal)?;
    Ok(())
}
