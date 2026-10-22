//! Maps window-stream input onto `AppKit` events for a macOS editor window.
//!
//! The protocol speaks X: keysyms and X buttons. A Mac editor wants virtual key codes, the
//! characters a key types, modifier flags on every event, drag events while a button is held,
//! and click counts. [`InputMapper`] keeps that state for one worker.

use std::time::{Duration, Instant};

use sp_vst3::editor_input::{EditorInput, Modifiers, MouseButton};
use sp_window_stream::{InputEvent, PointerButton, keysym_for_char};

/// Presses of one button closer than this in time and space make a double or triple click, as
/// with `AppKit`'s default settings.
const MULTI_CLICK_INTERVAL: Duration = Duration::from_millis(500);
const MULTI_CLICK_DISTANCE: i16 = 4;
/// The key code for a character with no key on a US keyboard. No Mac key has it, and it stays
/// below 128 for frameworks that index tables by key code.
const NO_KEY_CODE: u16 = 0x7f;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Modifier {
    Shift,
    Control,
    Option,
    Command,
}

/// What an X keysym is on a Mac keyboard: a modifier, or a key that types one character.
#[derive(Debug, Eq, PartialEq)]
enum MacKey {
    Modifier(u16, Modifier),
    Typing(u16, char),
}

struct Press {
    button: MouseButton,
    at: Instant,
    x: i16,
    y: i16,
    count: u32,
}

/// Input state for one worker's editor windows.
#[derive(Default)]
pub(crate) struct InputMapper {
    modifiers: Modifiers,
    /// Held buttons, first pressed first.
    held: Vec<MouseButton>,
    last_press: Option<Press>,
}

impl InputMapper {
    /// The editor events for one input event, or why a Mac editor cannot take it.
    pub(crate) fn map(
        &mut self,
        event: &InputEvent,
        now: Instant,
    ) -> Result<Vec<EditorInput>, String> {
        let modifiers = self.modifiers;
        Ok(match *event {
            InputEvent::PointerMove { x, y, .. } => vec![EditorInput::MouseMoved {
                x: x.into(),
                y: y.into(),
                dragging: self.held.first().copied(),
                modifiers,
            }],
            InputEvent::PointerButton {
                x,
                y,
                button,
                pressed,
                ..
            } => {
                let button = match button {
                    PointerButton::Left => MouseButton::Left,
                    PointerButton::Right => MouseButton::Right,
                    PointerButton::Middle => MouseButton::Middle,
                    PointerButton::Back | PointerButton::Forward => {
                        return Err("macOS editors take the left, right, and middle buttons".into());
                    }
                };
                let click_count = if pressed {
                    self.press(button, x, y, now)
                } else {
                    self.held.retain(|&held| held != button);
                    self.last_press
                        .as_ref()
                        .filter(|press| press.button == button)
                        .map_or(1, |press| press.count)
                };
                vec![EditorInput::MouseButton {
                    x: x.into(),
                    y: y.into(),
                    button,
                    pressed,
                    click_count,
                    modifiers,
                }]
            }
            InputEvent::Scroll { x, y, dx, dy, .. } => vec![EditorInput::Scroll {
                x: x.into(),
                y: y.into(),
                dx: dx.into(),
                dy: dy.into(),
                modifiers,
            }],
            InputEvent::Key {
                keysym, pressed, ..
            } => {
                match mac_key(keysym).ok_or_else(|| format!("keysym {keysym:#x} has no Mac key"))? {
                    MacKey::Modifier(key_code, modifier) => {
                        let held = match modifier {
                            Modifier::Shift => &mut self.modifiers.shift,
                            Modifier::Control => &mut self.modifiers.control,
                            Modifier::Option => &mut self.modifiers.option,
                            Modifier::Command => &mut self.modifiers.command,
                        };
                        if *held == pressed {
                            Vec::new()
                        } else {
                            *held = pressed;
                            vec![EditorInput::ModifiersChanged {
                                key_code,
                                modifiers: self.modifiers,
                            }]
                        }
                    }
                    MacKey::Typing(key_code, character) => vec![EditorInput::Key {
                        key_code,
                        characters: character.into(),
                        pressed,
                        modifiers,
                    }],
                }
            }
            // Each character is a key press and release, which every editor framework reads.
            InputEvent::Text { ref text, .. } => text
                .chars()
                .flat_map(|character| {
                    let (key_code, character) = match mac_key(keysym_for_char(character)) {
                        Some(MacKey::Typing(key_code, typed)) => (key_code, typed),
                        _ => (NO_KEY_CODE, character),
                    };
                    [true, false].map(|pressed| EditorInput::Key {
                        key_code,
                        characters: character.into(),
                        pressed,
                        modifiers,
                    })
                })
                .collect(),
            InputEvent::Focus { .. } => vec![EditorInput::Focus],
            InputEvent::Close { .. } => vec![EditorInput::Close],
        })
    }

    /// Holds `button` and returns the press's click count.
    fn press(&mut self, button: MouseButton, x: i16, y: i16, now: Instant) -> u32 {
        if !self.held.contains(&button) {
            self.held.push(button);
        }
        let count = match &self.last_press {
            Some(last)
                if last.button == button
                    && now.saturating_duration_since(last.at) <= MULTI_CLICK_INTERVAL
                    && last.x.abs_diff(x) <= MULTI_CLICK_DISTANCE.unsigned_abs()
                    && last.y.abs_diff(y) <= MULTI_CLICK_DISTANCE.unsigned_abs() =>
            {
                last.count + 1
            }
            _ => 1,
        };
        self.last_press = Some(Press {
            button,
            at: now,
            x,
            y,
            count,
        });
        count
    }
}

/// The Mac key for an X keysym: its virtual key code (`kVK_*`) and, for a typing key, the
/// character `AppKit` reports. Named keys type `AppKit` function-key characters from U+F700.
fn mac_key(keysym: u32) -> Option<MacKey> {
    const F_KEY_CODES: [u16; 12] = [122, 120, 99, 118, 96, 97, 98, 100, 101, 109, 103, 111];
    let function = |offset: u32| char::from_u32(0xf700 + offset);
    let (key_code, character) = match keysym {
        0xffe1 => return Some(MacKey::Modifier(56, Modifier::Shift)),
        0xffe2 => return Some(MacKey::Modifier(60, Modifier::Shift)),
        0xffe3 => return Some(MacKey::Modifier(59, Modifier::Control)),
        0xffe4 => return Some(MacKey::Modifier(62, Modifier::Control)),
        0xffe9 => return Some(MacKey::Modifier(58, Modifier::Option)),
        0xffea => return Some(MacKey::Modifier(61, Modifier::Option)),
        0xffeb => return Some(MacKey::Modifier(55, Modifier::Command)),
        0xffec => return Some(MacKey::Modifier(54, Modifier::Command)),
        0xff0d => (36, '\r'),
        0xff09 => (48, '\t'),
        0xff1b => (53, '\u{1b}'),
        0xff08 => (51, '\u{7f}'),
        0xffff => (117, function(0x28)?),
        0xff63 => (114, function(0x27)?),
        0xff50 => (115, function(0x29)?),
        0xff57 => (119, function(0x2b)?),
        0xff55 => (116, function(0x2c)?),
        0xff56 => (121, function(0x2d)?),
        0xff51 => (123, function(0x02)?),
        0xff52 => (126, function(0x00)?),
        0xff53 => (124, function(0x03)?),
        0xff54 => (125, function(0x01)?),
        0xffbe..=0xffc9 => {
            let index = keysym - 0xffbe;
            (F_KEY_CODES[index as usize], function(0x04 + index)?)
        }
        // Latin-1 keysyms are their code points; others carry one in the Unicode range.
        0x20..=0x7e | 0xa0..=0xff => {
            let character = char::from_u32(keysym)?;
            (us_key_code(character), character)
        }
        0x0100_0100..=0x0110_ffff => {
            let character = char::from_u32(keysym - 0x0100_0000)?;
            (us_key_code(character), character)
        }
        _ => return None,
    };
    Some(MacKey::Typing(key_code, character))
}

/// The key that types `character` on a US keyboard, with or without Shift.
fn us_key_code(character: char) -> u16 {
    match character.to_ascii_lowercase() {
        'a' => 0,
        's' => 1,
        'd' => 2,
        'f' => 3,
        'h' => 4,
        'g' => 5,
        'z' => 6,
        'x' => 7,
        'c' => 8,
        'v' => 9,
        'b' => 11,
        'q' => 12,
        'w' => 13,
        'e' => 14,
        'r' => 15,
        'y' => 16,
        't' => 17,
        '1' | '!' => 18,
        '2' | '@' => 19,
        '3' | '#' => 20,
        '4' | '$' => 21,
        '6' | '^' => 22,
        '5' | '%' => 23,
        '=' | '+' => 24,
        '9' | '(' => 25,
        '7' | '&' => 26,
        '-' | '_' => 27,
        '8' | '*' => 28,
        '0' | ')' => 29,
        ']' | '}' => 30,
        'o' => 31,
        'u' => 32,
        '[' | '{' => 33,
        'i' => 34,
        'p' => 35,
        'l' => 37,
        'j' => 38,
        '\'' | '"' => 39,
        'k' => 40,
        ';' | ':' => 41,
        '\\' | '|' => 42,
        ',' | '<' => 43,
        '/' | '?' => 44,
        'n' => 45,
        'm' => 46,
        '.' | '>' => 47,
        ' ' => 49,
        '`' | '~' => 50,
        _ => NO_KEY_CODE,
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use sp_vst3::editor_input::{EditorInput, Modifiers, MouseButton};
    use sp_window_stream::{InputEvent, PointerButton, WindowId};

    use super::InputMapper;

    const W: WindowId = WindowId(7);

    fn key(keysym: u32, pressed: bool) -> InputEvent {
        InputEvent::Key {
            window: W,
            keysym,
            pressed,
        }
    }

    fn typed(key_code: u16, characters: &str, pressed: bool, modifiers: Modifiers) -> EditorInput {
        EditorInput::Key {
            key_code,
            characters: characters.into(),
            pressed,
            modifiers,
        }
    }

    #[test]
    fn keys_carry_appkit_codes_characters_and_held_modifiers() {
        let mut mapper = InputMapper::default();
        let now = Instant::now();
        let none = Modifiers::default();
        let command = Modifiers {
            command: true,
            ..Modifiers::default()
        };
        let mut map = |event| mapper.map(&event, now);

        assert_eq!(
            map(key(0xff52, true)),
            Ok(vec![typed(126, "\u{f700}", true, none)])
        );
        assert_eq!(
            map(key(0xffc2, false)),
            Ok(vec![typed(96, "\u{f708}", false, none)])
        );
        assert_eq!(
            map(key(0xffeb, true)),
            Ok(vec![EditorInput::ModifiersChanged {
                key_code: 55,
                modifiers: command
            }])
        );
        assert_eq!(map(key(0xffeb, true)), Ok(Vec::new()));
        assert_eq!(
            map(key(u32::from('z'), true)),
            Ok(vec![typed(6, "z", true, command)])
        );
        assert_eq!(
            map(InputEvent::Text {
                window: W,
                text: "é\n".into()
            }),
            Ok(vec![
                typed(0x7f, "é", true, command),
                typed(0x7f, "é", false, command),
                typed(36, "\r", true, command),
                typed(36, "\r", false, command),
            ])
        );
        assert!(map(key(0xfe03, true)).is_err());
    }

    /// The click count of the one button event `event` maps to.
    fn clicks(mapper: &mut InputMapper, event: &InputEvent, at: Instant) -> u32 {
        match mapper.map(event, at).as_deref() {
            Ok([EditorInput::MouseButton { click_count, .. }]) => *click_count,
            other => panic!("expected one button event, got {other:?}"),
        }
    }

    #[test]
    fn held_buttons_drag_and_quick_presses_count_clicks() {
        let mut mapper = InputMapper::default();
        let start = Instant::now();
        let ms = |ms| start + Duration::from_millis(ms);
        let left = |x, pressed| InputEvent::PointerButton {
            window: W,
            x,
            y: 10,
            button: PointerButton::Left,
            pressed,
        };
        assert_eq!(clicks(&mut mapper, &left(10, true), ms(0)), 1);
        assert_eq!(clicks(&mut mapper, &left(10, false), ms(50)), 1);
        assert_eq!(clicks(&mut mapper, &left(12, true), ms(150)), 2);
        assert_eq!(
            mapper.map(
                &InputEvent::PointerMove {
                    window: W,
                    x: 40,
                    y: 30
                },
                ms(200)
            ),
            Ok(vec![EditorInput::MouseMoved {
                x: 40.0,
                y: 30.0,
                dragging: Some(MouseButton::Left),
                modifiers: Modifiers::default(),
            }])
        );
        assert_eq!(clicks(&mut mapper, &left(40, false), ms(250)), 2);
        assert_eq!(clicks(&mut mapper, &left(40, true), ms(1000)), 1);
        let back = InputEvent::PointerButton {
            window: W,
            x: 0,
            y: 0,
            button: PointerButton::Back,
            pressed: true,
        };
        assert!(mapper.map(&back, start).is_err());
    }
}
