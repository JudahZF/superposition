//! Streamed plug-in editor windows shown inside the app.
//!
//! Linux: plug-in editors, Windows installers, and licence managers draw on the engine-owned
//! Xvfb, and `sp-window-streamer` sends their pixels over one connection.
//!
//! macOS: editors are native windows of their workers. While the engine serves remote clients,
//! each worker connects to the engine's editor-stream socket and streams its own windows. The UI
//! greets the workers only while a remote client watches and hangs up when the last one leaves,
//! so an unwatched Mac pictures nothing; local users keep using the native windows.
//!
//! Either way the pixels arrive as lossless tiles. This module keeps one texture per window, draws
//! each top-level window as an egui window (popups float over their parent), and sends pointer,
//! wheel, key, and text input back to the connection that owns the window. Because the windows are
//! egui content, a remote client sees and drives them like the rest of the UI.

#[cfg(target_os = "linux")]
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};
use std::{
    collections::HashMap,
    io,
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
};
#[cfg(target_os = "macos")]
use std::{os::unix::net::UnixListener, sync::mpsc};

use egui::{Context, Event, Key, Modifiers, PointerButton, Sense, TextureHandle, TextureOptions};
use sp_window_stream::{
    ClientMessage, InputEvent, PROTOCOL_VERSION, PointerButton as StreamButton, Role,
    ScrollAccumulator, ServerMessage, WindowId, WindowInfo, WindowKind, WindowStreamClient,
    WireMessage, keysym_for_char,
};

/// Wheel input per notch, in egui points.
const POINTS_PER_NOTCH: f32 = 40.0;
#[cfg(target_os = "linux")]
const RECONNECT_INTERVAL: Duration = Duration::from_secs(1);

/// A request the app must handle itself.
pub(crate) enum StreamedWindowEvent {
    /// The user closed a plug-in editor; close it through its worker.
    CloseEditor { rack: usize, slot: usize },
}

struct Connection {
    writer: UnixStream,
    state: Arc<Mutex<WindowStreamClient>>,
    alive: Arc<AtomicBool>,
}

impl Connection {
    /// Greets the window source at the other end of `stream` and starts reading its windows.
    fn start(mut stream: UnixStream, ctx: &Context) -> io::Result<Self> {
        ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            role: Role::Controller,
            name: "Superposition".into(),
        }
        .write_to(&mut stream)
        .map_err(io::Error::other)?;
        let state = Arc::new(Mutex::new(WindowStreamClient::new()?));
        let alive = Arc::new(AtomicBool::new(true));
        let mut reader = stream.try_clone()?;
        let reader_state = Arc::clone(&state);
        let reader_alive = Arc::clone(&alive);
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("sp-window-stream".into())
            .spawn(move || {
                let mut buffer = Vec::new();
                while let Ok(message) = ServerMessage::read_from(&mut reader, &mut buffer) {
                    let applied = reader_state
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .apply(&message);
                    if applied.is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
                reader_alive.store(false, Ordering::Release);
                ctx.request_repaint();
            })?;
        Ok(Self {
            writer: stream,
            state,
            alive,
        })
    }

    fn send(&mut self, event: InputEvent) {
        if ClientMessage::Input(event)
            .write_to(&mut self.writer)
            .is_err()
        {
            self.alive.store(false, Ordering::Release);
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // Shutting the socket down also ends the reader thread's clone, so the peer sees EOF.
        let _ = self.writer.shutdown(Shutdown::Both);
    }
}

/// Where window-stream connections come from.
enum Source {
    /// Linux: the streamer's socket, reconnected after the streamer restarts.
    #[cfg(target_os = "linux")]
    Streamer { socket: PathBuf, retry_at: Instant },
    /// macOS: workers that connected to the engine's editor-stream socket and wait to be greeted.
    #[cfg(target_os = "macos")]
    Workers {
        incoming: mpsc::Receiver<UnixStream>,
        waiting: Vec<UnixStream>,
        /// A remote client watches, so workers stream.
        watched: bool,
    },
}

struct Shown {
    info: WindowInfo,
    size: [usize; 2],
    /// Index of the connection that streams the window.
    connection: usize,
}

/// The streamed windows of the engine's private display or of its workers.
pub(crate) struct StreamedWindows {
    source: Source,
    connections: Vec<Connection>,
    textures: HashMap<WindowId, TextureHandle>,
    scroll: HashMap<WindowId, ScrollAccumulator>,
    /// The window under a held pointer button, which keeps receiving the drag.
    pointer_capture: Option<WindowId>,
    /// The window that receives keys: the last one clicked.
    keyboard: Option<WindowId>,
    /// Modifiers held in the keyboard window.
    modifiers: Modifiers,
}

impl StreamedWindows {
    fn with_source(source: Source) -> Self {
        Self {
            source,
            connections: Vec::new(),
            textures: HashMap::new(),
            scroll: HashMap::new(),
            pointer_capture: None,
            keyboard: None,
            modifiers: Modifiers::NONE,
        }
    }

    /// Linux: streams from the streamer listening at `socket`.
    #[cfg(target_os = "linux")]
    pub(crate) fn from_streamer(socket: PathBuf) -> Self {
        Self::with_source(Source::Streamer {
            socket,
            retry_at: Instant::now(),
        })
    }

    /// macOS: streams from the workers that connect to `listener`, once a remote client watches.
    #[cfg(target_os = "macos")]
    pub(crate) fn from_workers(listener: UnixListener) -> Self {
        let (accepted, incoming) = mpsc::channel();
        let accept = std::thread::Builder::new()
            .name("sp-editor-stream-accept".into())
            .spawn(move || {
                for stream in listener.incoming().flatten() {
                    if accepted.send(stream).is_err() {
                        return;
                    }
                }
            });
        if let Err(error) = accept {
            eprintln!("superposition: plug-in editors will not stream: {error}");
        }
        Self::with_source(Source::Workers {
            incoming,
            waiting: Vec::new(),
            watched: false,
        })
    }

    /// macOS: whether a remote client watches. Workers stream only then.
    #[cfg(target_os = "macos")]
    pub(crate) fn set_watched(&mut self, watched: bool) {
        let Source::Workers { watched: now, .. } = &mut self.source;
        *now = watched;
    }

    /// Whether a streamed window has keyboard focus (the user last clicked inside one).
    pub(crate) fn has_keyboard_focus(&self) -> bool {
        self.keyboard.is_some()
    }

    /// Opens, greets, and closes connections as the source requires.
    fn connect(&mut self, ctx: &Context) {
        // A dead connection's window IDs are gone with it; its textures go below.
        self.connections
            .retain(|connection| connection.alive.load(Ordering::Acquire));
        match &mut self.source {
            #[cfg(target_os = "linux")]
            Source::Streamer { socket, retry_at } => {
                if self.connections.is_empty() && Instant::now() >= *retry_at {
                    *retry_at = Instant::now() + RECONNECT_INTERVAL;
                    if let Ok(connection) = UnixStream::connect(&*socket)
                        .and_then(|stream| Connection::start(stream, ctx))
                    {
                        self.connections.push(connection);
                    }
                }
            }
            #[cfg(target_os = "macos")]
            Source::Workers {
                incoming,
                waiting,
                watched,
            } => {
                waiting.extend(incoming.try_iter());
                if *watched {
                    self.connections.extend(
                        waiting
                            .drain(..)
                            .filter_map(|stream| Connection::start(stream, ctx).ok()),
                    );
                } else {
                    // Hanging up stops each worker's pictures; it reconnects and waits.
                    self.connections.clear();
                }
            }
        }
    }

    /// Draws every streamed window and forwards input. Returns editor close requests.
    #[allow(
        clippy::too_many_lines,
        reason = "one pass: sync textures, draw windows, then popups over their parents"
    )]
    pub(crate) fn show(&mut self, ctx: &Context) -> Vec<StreamedWindowEvent> {
        self.connect(ctx);
        let mut windows = Vec::new();
        for (index, connection) in self.connections.iter().enumerate() {
            let mut state = connection
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let ids: Vec<_> = state.windows().map(|window| window.info().id).collect();
            for id in ids {
                let Some(window) = state.window_mut(id) else {
                    continue;
                };
                let size = window.size();
                if size[0] == 0 || size[1] == 0 {
                    continue;
                }
                upload(ctx, &mut self.textures, id, window);
                windows.push(Shown {
                    info: window.info().clone(),
                    size,
                    connection: index,
                });
            }
        }
        self.textures
            .retain(|id, _| windows.iter().any(|window| window.info.id == *id));
        if self
            .keyboard
            .is_some_and(|id| windows.iter().all(|window| window.info.id != id))
        {
            // The window is gone, and with it the modifiers held there.
            self.keyboard = None;
            self.modifiers = Modifiers::NONE;
        }
        self.scroll
            .retain(|id, _| windows.iter().any(|window| window.info.id == *id));
        if windows.is_empty() {
            return Vec::new();
        }

        let pixels_per_point = ctx.pixels_per_point();
        let mut events = Vec::new();
        let mut image_rects: HashMap<WindowId, egui::Rect> = HashMap::new();
        for window in windows
            .iter()
            .filter(|window| window.info.kind == WindowKind::Normal && !is_decoration(&window.info))
        {
            let Some(texture) = self.textures.get(&window.info.id) else {
                continue;
            };
            let mut open = true;
            let title = if window.info.title.is_empty() {
                "Window".to_owned()
            } else {
                window.info.title.clone()
            };
            let shown = egui::Window::new(title)
                .id(egui::Id::new(("streamed-window", window.info.id)))
                .open(&mut open)
                .resizable(false)
                .collapsible(false)
                .show(ctx, |ui| {
                    let size = points(window.size, pixels_per_point);
                    ui.add(egui::Image::new((texture.id(), size)).sense(Sense::click_and_drag()))
                });
            if let Some(inner) = shown.and_then(|shown| shown.inner) {
                image_rects.insert(window.info.id, inner.rect);
            }
            if !open {
                match (window.info.rack, window.info.slot) {
                    (Some(rack), Some(slot)) => events.push(StreamedWindowEvent::CloseEditor {
                        rack: rack as usize,
                        slot: slot as usize,
                    }),
                    _ => self.connections[window.connection].send(InputEvent::Close {
                        window: window.info.id,
                    }),
                }
            }
        }
        // Popups (menus, drop-downs) keep their position relative to their parent window.
        for window in windows
            .iter()
            .filter(|window| window.info.kind == WindowKind::Popup)
        {
            let parent = window
                .info
                .transient_for
                .and_then(|parent| windows.iter().find(|shown| shown.info.id == parent));
            let (Some(parent), Some(texture)) = (parent, self.textures.get(&window.info.id)) else {
                continue;
            };
            let Some(parent_rect) = image_rects.get(&parent.info.id) else {
                continue;
            };
            let offset = egui::vec2(
                f32::from(window.info.x - parent.info.x),
                f32::from(window.info.y - parent.info.y),
            ) / pixels_per_point;
            let size = points(window.size, pixels_per_point);
            let shown = egui::Area::new(egui::Id::new(("streamed-popup", window.info.id)))
                .order(egui::Order::Foreground)
                .fixed_pos(parent_rect.min + offset)
                .show(ctx, |ui| {
                    ui.add(egui::Image::new((texture.id(), size)).sense(Sense::click_and_drag()))
                });
            image_rects.insert(window.info.id, shown.inner.rect);
        }
        let owners = windows
            .iter()
            .map(|window| (window.info.id, window.connection))
            .collect();
        self.forward_input(ctx, &image_rects, &owners, pixels_per_point);
        events
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one dispatch over egui's input events"
    )]
    fn forward_input(
        &mut self,
        ctx: &Context,
        image_rects: &HashMap<WindowId, egui::Rect>,
        owners: &HashMap<WindowId, usize>,
        pixels_per_point: f32,
    ) {
        let connections = &mut self.connections;
        let mut send = |event: InputEvent| {
            if let Some(connection) = owners
                .get(&event.window())
                .and_then(|&index| connections.get_mut(index))
            {
                connection.send(event);
            }
        };
        // Popups draw last, so test them first.
        let target_at = |position: egui::Pos2| {
            let layer = ctx.layer_id_at(position);
            image_rects
                .iter()
                .filter(|(_, rect)| rect.contains(position))
                .map(|(id, rect)| (*id, *rect))
                .max_by_key(|(id, _)| {
                    layer.is_some_and(|layer| {
                        layer.id == egui::Id::new(("streamed-popup", *id))
                            || layer.id == egui::Id::new(("streamed-window", *id))
                    })
                })
        };
        let local = |rect: egui::Rect, position: egui::Pos2| {
            let offset = (position - rect.min) * pixels_per_point;
            (clamp_i16(offset.x), clamp_i16(offset.y))
        };
        let events = ctx.input(|input| input.events.clone());
        for event in events {
            match event {
                Event::PointerMoved(position) => {
                    let target = self
                        .pointer_capture
                        .and_then(|id| image_rects.get(&id).map(|rect| (id, *rect)))
                        .or_else(|| target_at(position));
                    if let Some((window, rect)) = target {
                        let (x, y) = local(rect, position);
                        send(InputEvent::PointerMove { window, x, y });
                    }
                }
                Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    ..
                } => {
                    let target = if pressed {
                        target_at(pos)
                    } else {
                        self.pointer_capture
                            .and_then(|id| image_rects.get(&id).map(|rect| (id, *rect)))
                    };
                    let Some((window, rect)) = target else {
                        // A click in the app's own UI takes the keyboard back.
                        if pressed && let Some(previous) = self.keyboard.take() {
                            sync_modifiers(
                                &mut send,
                                previous,
                                &mut self.modifiers,
                                Modifiers::NONE,
                            );
                        }
                        continue;
                    };
                    if pressed {
                        self.pointer_capture = Some(window);
                        if self.keyboard != Some(window) {
                            // Modifiers held in another window are released there first.
                            if let Some(previous) = self.keyboard.replace(window) {
                                sync_modifiers(
                                    &mut send,
                                    previous,
                                    &mut self.modifiers,
                                    Modifiers::NONE,
                                );
                            }
                            send(InputEvent::Focus { window });
                        }
                    } else {
                        self.pointer_capture = None;
                    }
                    let (x, y) = local(rect, pos);
                    send(InputEvent::PointerButton {
                        window,
                        x,
                        y,
                        button: stream_button(button),
                        pressed,
                    });
                }
                Event::MouseWheel { delta, unit, .. } => {
                    let Some(position) = ctx.input(|input| input.pointer.hover_pos()) else {
                        continue;
                    };
                    let Some((window, rect)) = target_at(position) else {
                        continue;
                    };
                    let scale = match unit {
                        egui::MouseWheelUnit::Line => POINTS_PER_NOTCH,
                        egui::MouseWheelUnit::Page => POINTS_PER_NOTCH * 10.0,
                        egui::MouseWheelUnit::Point => 1.0,
                    };
                    // egui reports wheel-up as positive y; X scrolls down for positive notches.
                    let notches = self
                        .scroll
                        .entry(window)
                        .or_insert_with(|| ScrollAccumulator::new(POINTS_PER_NOTCH))
                        .push(-delta.x * scale, -delta.y * scale);
                    if let Some((dx, dy)) = notches {
                        let (x, y) = local(rect, position);
                        send(InputEvent::Scroll {
                            window,
                            x,
                            y,
                            dx,
                            dy,
                        });
                    }
                }
                Event::Key {
                    key,
                    pressed,
                    modifiers,
                    ..
                } => {
                    let Some(window) = self.keyboard.filter(|id| image_rects.contains_key(id))
                    else {
                        continue;
                    };
                    sync_modifiers(&mut send, window, &mut self.modifiers, modifiers);
                    // Printable keys arrive as text unless a shortcut modifier is held.
                    let shortcut = modifiers.ctrl || modifiers.alt || modifiers.command;
                    if let Some(keysym) = keysym(key, shortcut) {
                        send(InputEvent::Key {
                            window,
                            keysym,
                            pressed,
                        });
                    }
                }
                Event::Text(text) => {
                    let Some(window) = self.keyboard.filter(|id| image_rects.contains_key(id))
                    else {
                        continue;
                    };
                    if !text.is_empty() {
                        send(InputEvent::Text { window, text });
                    }
                }
                _ => {}
            }
        }
    }
}

fn upload(
    ctx: &Context,
    textures: &mut HashMap<WindowId, TextureHandle>,
    id: WindowId,
    window: &mut sp_window_stream::StreamedWindow,
) {
    let [width, height] = window.size();
    let dirty = window.take_dirty();
    let resized = textures
        .get(&id)
        .is_some_and(|texture| texture.size() != [width, height]);
    if resized || !textures.contains_key(&id) {
        let image = egui::ColorImage::from_rgba_unmultiplied([width, height], window.rgba());
        textures.insert(
            id,
            ctx.load_texture(format!("streamed-{id:?}"), image, TextureOptions::NEAREST),
        );
        return;
    }
    let texture = textures.get_mut(&id).expect("checked above");
    for rect in dirty {
        let image = egui::ColorImage::from_rgba_unmultiplied(
            [usize::from(rect.width), usize::from(rect.height)],
            &window.copy_rect(rect),
        );
        texture.set_partial(
            [usize::from(rect.x), usize::from(rect.y)],
            image,
            TextureOptions::NEAREST,
        );
    }
}

/// Untitled, very thin top-level windows are window decorations a toolkit draws itself, such
/// as JUCE's drop shadows, which Wine maps as separate windows and fills black. Editors and
/// dialogs are never this thin.
fn is_decoration(info: &WindowInfo) -> bool {
    const THIN: u16 = 32;
    info.title.trim().is_empty() && info.rack.is_none() && info.width.min(info.height) <= THIN
}

/// A window's pixel size in egui points. X window sizes fit `u16`.
fn points([width, height]: [usize; 2], pixels_per_point: f32) -> egui::Vec2 {
    let pixels = |value: usize| f32::from(u16::try_from(value).unwrap_or(u16::MAX));
    egui::vec2(pixels(width), pixels(height)) / pixels_per_point
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "clamped to the i16 range before the cast"
)]
fn clamp_i16(value: f32) -> i16 {
    // Saturating: drags may leave the window by more than i16 allows only in theory.
    value
        .round()
        .clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
}

fn stream_button(button: PointerButton) -> StreamButton {
    match button {
        PointerButton::Primary => StreamButton::Left,
        PointerButton::Secondary => StreamButton::Right,
        PointerButton::Middle => StreamButton::Middle,
        PointerButton::Extra1 => StreamButton::Back,
        PointerButton::Extra2 => StreamButton::Forward,
    }
}

/// Presses or releases modifier keys so `window` sees the same state as egui. A Mac's Command key
/// travels as `Super_L`, which macOS workers turn back into Command.
fn sync_modifiers(
    send: &mut impl FnMut(InputEvent),
    window: WindowId,
    current: &mut Modifiers,
    wanted: Modifiers,
) {
    for (was, is, keysym) in [
        (current.shift, wanted.shift, 0xffe1),
        (current.ctrl, wanted.ctrl, 0xffe3),
        (current.alt, wanted.alt, 0xffe9),
        (current.mac_cmd, wanted.mac_cmd, 0xffeb),
    ] {
        if was != is {
            send(InputEvent::Key {
                window,
                keysym,
                pressed: is,
            });
        }
    }
    *current = wanted;
}

/// The X keysym for a key egui reports. Printable keys map only for shortcuts; otherwise their
/// text arrives as `Event::Text`.
fn keysym(key: Key, shortcut: bool) -> Option<u32> {
    let named = match key {
        Key::Enter => 0xff0d,
        Key::Escape => 0xff1b,
        Key::Tab => 0xff09,
        Key::Backspace => 0xff08,
        Key::Delete => 0xffff,
        Key::Insert => 0xff63,
        Key::Home => 0xff50,
        Key::End => 0xff57,
        Key::PageUp => 0xff55,
        Key::PageDown => 0xff56,
        Key::ArrowLeft => 0xff51,
        Key::ArrowUp => 0xff52,
        Key::ArrowRight => 0xff53,
        Key::ArrowDown => 0xff54,
        Key::F1 => 0xffbe,
        Key::F2 => 0xffbf,
        Key::F3 => 0xffc0,
        Key::F4 => 0xffc1,
        Key::F5 => 0xffc2,
        Key::F6 => 0xffc3,
        Key::F7 => 0xffc4,
        Key::F8 => 0xffc5,
        Key::F9 => 0xffc6,
        Key::F10 => 0xffc7,
        Key::F11 => 0xffc8,
        Key::F12 => 0xffc9,
        _ if shortcut => {
            let mut name = key.name().chars();
            return match (name.next(), name.next()) {
                (Some(character), None) => Some(keysym_for_char(character.to_ascii_lowercase())),
                _ => None,
            };
        }
        _ => return None,
    };
    Some(named)
}

#[cfg(test)]
mod tests {
    use egui::Key;

    use sp_window_stream::{WindowId, WindowInfo, WindowKind};

    use super::{is_decoration, keysym};

    #[test]
    fn only_untitled_thin_windows_are_decorations() {
        let window = |title: &str, width, height| WindowInfo {
            id: WindowId(1),
            kind: WindowKind::Normal,
            title: title.into(),
            x: 0,
            y: 0,
            width,
            height,
            rack: None,
            slot: None,
            pid: None,
            transient_for: None,
        };
        assert!(is_decoration(&window("", 615, 20)));
        assert!(!is_decoration(&window("", 400, 300)));
        assert!(!is_decoration(&window("Toolbar", 615, 20)));
    }

    #[test]
    fn printable_keys_map_only_as_shortcuts() {
        assert_eq!(keysym(Key::A, false), None);
        assert_eq!(keysym(Key::A, true), Some(u32::from('a')));
        assert_eq!(keysym(Key::Enter, false), Some(0xff0d));
        assert_eq!(keysym(Key::ArrowLeft, true), Some(0xff51));
    }
}
