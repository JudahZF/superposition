//! End-to-end: a private Xvfb, a test window with a known pattern, the streamer binary and two
//! protocol clients. Checks pixels, damage-only updates, input injection and control takeover,
//! and prints idle CPU, keyframe size and change-to-pixel latency.
#![cfg(target_os = "linux")]
// Test coordinates and colours are small constants.
#![allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]

use std::{
    collections::BTreeSet,
    io::{BufRead, BufReader},
    os::unix::net::UnixStream,
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use sp_window_stream::{
    ClientMessage, ControlReason, ErrorCode, InputEvent, PROTOCOL_VERSION, PointerButton, Rect,
    Role, ServerMessage, WindowId, WindowKind, WindowStreamClient, WireMessage,
};
use x11rb::{
    connection::Connection,
    protocol::{
        Event,
        xproto::{
            ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, ImageFormat, WindowClass,
        },
    },
    rust_connection::RustConnection,
};

const WIDTH: u16 = 800;
const HEIGHT: u16 = 600;
const TIMEOUT: Duration = Duration::from_secs(10);

/// Stops the child on drop so a failed assertion leaves no X server behind. SIGTERM lets Xvfb
/// remove its socket and the streamer remove its own.
struct Process(Child);

impl Drop for Process {
    fn drop(&mut self) {
        let pid = rustix::process::Pid::from_child(&self.0);
        if rustix::process::kill_process(pid, rustix::process::Signal::TERM).is_err() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

fn start_xvfb() -> (Process, String) {
    let mut child = Command::new("Xvfb")
        .args([
            "-displayfd",
            "1",
            "-nolisten",
            "tcp",
            "-screen",
            "0",
            "1280x1024x24",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("Xvfb from the dev shell");
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    (Process(child), format!(":{}", line.trim()))
}

/// Hold the returned stdin: closing it stops the streamer.
fn start_streamer(display: &str, socket: &PathBuf) -> (Process, ChildStdin) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sp-window-streamer"))
        .args(["--display", display, "--listen"])
        .arg(socket)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert!(line.starts_with("listening "), "streamer said {line:?}");
    let stdin = child.stdin.take().unwrap();
    (Process(child), stdin)
}

const fn rect(x: u16, y: u16, width: u16, height: u16) -> Rect {
    Rect {
        x,
        y,
        width,
        height,
    }
}

/// The test window's BGRX pixels.
struct Pattern(Vec<u8>);

impl Pattern {
    /// A UI-like picture: soft gradients with flat panels and fine detail.
    fn new() -> Self {
        let mut pixels = Vec::with_capacity(usize::from(WIDTH) * usize::from(HEIGHT) * 4);
        for y in 0..u32::from(HEIGHT) {
            for x in 0..u32::from(WIDTH) {
                let panel = (40..360).contains(&x) && (60..300).contains(&y);
                let detail = (x / 3 + y / 5) % 7 == 0 && y > 400;
                let px = match (panel, detail) {
                    (true, _) => [0x30, 0x28, 0x24, 0],
                    (_, true) => [0xf0, 0xf0, 0xf0, 0],
                    _ => [(x / 4) as u8, (y / 3) as u8, 0x80, 0],
                };
                pixels.extend_from_slice(&px);
            }
        }
        Self(pixels)
    }

    fn fill(&mut self, rect: Rect, bgrx: [u8; 4]) {
        for y in rect.y..rect.y + rect.height {
            for x in rect.x..rect.x + rect.width {
                let at = (usize::from(y) * usize::from(WIDTH) + usize::from(x)) * 4;
                self.0[at..at + 4].copy_from_slice(&bgrx);
            }
        }
    }

    fn rgba(&self) -> Vec<u8> {
        self.0
            .chunks_exact(4)
            .flat_map(|p| [p[2], p[1], p[0], 255])
            .collect()
    }
}

struct TestWindow {
    conn: RustConnection,
    id: u32,
    gc: u32,
    depth: u8,
}

impl TestWindow {
    fn open(display: &str) -> Self {
        let (conn, screen) = x11rb::connect(Some(display)).unwrap();
        let screen = &conn.setup().roots[screen];
        let (root, depth, visual) = (screen.root, screen.root_depth, screen.root_visual);
        let id = conn.generate_id().unwrap();
        let mask = EventMask::BUTTON_PRESS
            | EventMask::BUTTON_RELEASE
            | EventMask::KEY_PRESS
            | EventMask::KEY_RELEASE;
        let aux = CreateWindowAux::new().event_mask(mask);
        conn.create_window(
            depth,
            id,
            root,
            100,
            100,
            WIDTH,
            HEIGHT,
            0,
            WindowClass::INPUT_OUTPUT,
            visual,
            &aux,
        )
        .unwrap();
        let gc = conn.generate_id().unwrap();
        conn.create_gc(gc, id, &CreateGCAux::new()).unwrap();
        conn.map_window(id).unwrap();
        Self {
            conn,
            id,
            gc,
            depth,
        }
    }

    /// Maps a blank override-redirect window, as menus and tooltips are.
    fn popup(&self, area: Rect) {
        let screen = &self.conn.setup().roots[0];
        let popup = self.conn.generate_id().unwrap();
        let aux = CreateWindowAux::new()
            .override_redirect(1)
            .background_pixel(screen.white_pixel);
        let (x, y) = (area.x as i16, area.y as i16);
        self.conn
            .create_window(
                self.depth,
                popup,
                screen.root,
                x,
                y,
                area.width,
                area.height,
                0,
                WindowClass::INPUT_OUTPUT,
                screen.root_visual,
                &aux,
            )
            .unwrap();
        self.conn.map_window(popup).unwrap();
        self.conn.flush().unwrap();
    }

    /// Uploads `rect` of the pattern and waits until the server has drawn it.
    fn draw(&self, pattern: &Pattern, rect: Rect) {
        let stride = usize::from(WIDTH) * 4;
        for y in (rect.y..rect.y + rect.height).step_by(32) {
            let rows = 32.min(rect.y + rect.height - y);
            let mut band = Vec::new();
            for row in y..y + rows {
                let at = usize::from(row) * stride + usize::from(rect.x) * 4;
                band.extend_from_slice(&pattern.0[at..at + usize::from(rect.width) * 4]);
            }
            let (x, y) = (rect.x as i16, y as i16);
            self.conn
                .put_image(
                    ImageFormat::Z_PIXMAP,
                    self.id,
                    self.gc,
                    rect.width,
                    rows,
                    x,
                    y,
                    0,
                    self.depth,
                    &band,
                )
                .unwrap();
        }
        self.conn.get_input_focus().unwrap().reply().unwrap();
    }

    /// Waits for an event the predicate accepts.
    fn wait_event(&self, what: &str, mut accept: impl FnMut(&Event) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            match self.conn.poll_for_event().unwrap() {
                Some(event) if accept(&event) => return,
                Some(_) => {}
                None => thread::sleep(Duration::from_millis(2)),
            }
        }
        panic!("test window never received {what}");
    }

    fn keysym(&self, keycode: u8) -> u32 {
        let reply = self
            .conn
            .get_keyboard_mapping(keycode, 1)
            .unwrap()
            .reply()
            .unwrap();
        reply.keysyms[0]
    }
}

/// A protocol client whose reader thread decodes messages into a channel.
struct TestClient {
    stream: UnixStream,
    rx: mpsc::Receiver<(Instant, usize, ServerMessage)>,
    state: WindowStreamClient,
}

impl TestClient {
    fn connect(socket: &PathBuf, role: Role, name: &str) -> Self {
        let stream = UnixStream::connect(socket).unwrap();
        let mut reader = stream.try_clone().unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut buf = Vec::new();
            while let Ok(message) = ServerMessage::read_from(&mut reader, &mut buf) {
                if tx.send((Instant::now(), buf.len() + 4, message)).is_err() {
                    return;
                }
            }
        });
        let mut client = Self {
            stream,
            rx,
            state: WindowStreamClient::new().unwrap(),
        };
        client.send(&ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            role,
            name: name.into(),
        });
        client
    }

    fn send(&mut self, message: &ClientMessage) {
        message.write_to(&mut self.stream).unwrap();
    }

    /// Applies messages until `accept` returns true; returns the accepted one's arrival time.
    fn wait(
        &mut self,
        what: &str,
        mut accept: impl FnMut(&ServerMessage, &mut WindowStreamClient, usize) -> bool,
    ) -> Instant {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let (at, bytes, message) = self
                .rx
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
            self.state.apply(&message).unwrap();
            if accept(&message, &mut self.state, bytes) {
                return at;
            }
        }
    }

    /// Every message that arrives within `quiet`.
    fn drain(&mut self, quiet: Duration) -> Vec<ServerMessage> {
        let mut messages = Vec::new();
        while let Ok((_, _, message)) = self.rx.recv_timeout(quiet) {
            self.state.apply(&message).unwrap();
            messages.push(message);
        }
        messages
    }
}

/// The client's framebuffer for `window` equals `pattern`.
fn shows(state: &WindowStreamClient, window: WindowId, pattern: &Pattern) -> bool {
    state
        .window(window)
        .is_some_and(|w| w.rgba() == pattern.rgba())
}

fn cpu_ticks(pid: u32) -> u64 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let fields: Vec<&str> = stat
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .collect();
    // utime and stime are fields 14 and 15; the split starts at field 3.
    fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap()
}

/// One scenario on one X server: later steps depend on the state earlier steps set up.
#[test]
#[allow(clippy::too_many_lines)]
fn streams_pixels_injects_input_and_hands_over_control() {
    let (_xvfb, display) = start_xvfb();
    let mut pattern = Pattern::new();
    let window = TestWindow::open(&display);
    window.draw(&pattern, rect(0, 0, WIDTH, HEIGHT));
    let id = WindowId(window.id);

    let dir = std::env::temp_dir().join(format!("sp-window-streamer-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("stream.sock");
    let (streamer, _stdin) = start_streamer(&display, &socket);

    // Controller A joins, gets control and a keyframe that matches the pattern.
    let mut a = TestClient::connect(&socket, Role::Controller, "a");
    let mut a_id = None;
    a.wait("welcome", |m, _, _| {
        if let ServerMessage::Welcome { client, .. } = m {
            a_id = Some(*client);
        }
        matches!(m, ServerMessage::Welcome { .. })
    });
    a.wait("granted", |m, _, _| {
        matches!(
            m,
            ServerMessage::ControlStatus {
                reason: ControlReason::Granted,
                ..
            }
        )
    });
    assert!(a.state.has_control());
    assert!(
        a.state.window(id).is_some(),
        "window list lacks the test window"
    );
    let mut keyframe_bytes = 0;
    a.wait("keyframe", |m, state, bytes| {
        if matches!(m, ServerMessage::Frame(f) if f.window == id && f.keyframe) {
            keyframe_bytes += bytes;
        }
        shows(state, id, &pattern)
    });

    // Idle: nothing changes, so the streamer must not use CPU.
    a.drain(Duration::from_millis(300));
    let ticks = cpu_ticks(streamer.0.id());
    thread::sleep(Duration::from_secs(3));
    let idle_ticks = cpu_ticks(streamer.0.id()) - ticks;
    assert!(
        idle_ticks <= 3,
        "streamer used {idle_ticks} CPU ticks in 3 s of idle"
    );

    // A change spanning two tiles sends exactly those two tiles.
    let change = rect(200, 130, 60, 20);
    pattern.fill(change, [0x00, 0x00, 0xff, 0]);
    let changed_at = Instant::now();
    window.draw(&pattern, change);
    let mut tiles = BTreeSet::new();
    let decoded_at = a.wait("partial update", |m, state, _| {
        if let ServerMessage::Frame(f) = m {
            tiles.extend(
                f.tiles
                    .iter()
                    .map(|t| (t.rect.x, t.rect.y, t.rect.width, t.rect.height)),
            );
        }
        shows(state, id, &pattern)
    });
    for message in a.drain(Duration::from_millis(200)) {
        if let ServerMessage::Frame(f) = message {
            tiles.extend(
                f.tiles
                    .iter()
                    .map(|t| (t.rect.x, t.rect.y, t.rect.width, t.rect.height)),
            );
        }
    }
    assert_eq!(
        tiles,
        BTreeSet::from([(192, 128, 64, 64), (256, 128, 64, 64)])
    );

    eprintln!(
        "measured: idle {idle_ticks} ticks / 3 s, keyframe {WIDTH}x{HEIGHT} {keyframe_bytes} bytes, change-to-decode {:.1} ms",
        (decoded_at - changed_at).as_secs_f64() * 1000.0
    );

    // Isolated changes: each starts after the previous frame interval has passed.
    let mut latencies = Vec::new();
    for i in 0..30_u16 {
        let square = rect(400 + (i % 5) * 64, 300, 32, 32);
        pattern.fill(square, [(i * 8) as u8, 0x40, 0, 0]);
        let started = Instant::now();
        window.draw(&pattern, square);
        latencies.push(a.wait("update", |_, state, _| shows(state, id, &pattern)) - started);
        thread::sleep(Duration::from_millis(20));
    }
    latencies.sort();

    // A burst of 40 changes in about 160 ms is coalesced by the 60 fps cap.
    let burst = Instant::now();
    for i in 0..40_u16 {
        let square = rect(64, 448, 64, 64);
        pattern.fill(square, [(i * 6) as u8, 0, 0x80, 0]);
        window.draw(&pattern, square);
        thread::sleep(Duration::from_millis(4));
    }
    let burst_ms = burst.elapsed().as_millis();
    let mut frames = 0;
    a.wait("burst end", |m, state, _| {
        frames += usize::from(matches!(m, ServerMessage::Frame(f) if f.window == id));
        shows(state, id, &pattern)
    });
    assert!(
        (2..=burst_ms as usize / 16 + 3).contains(&frames),
        "{frames} frames for a {burst_ms} ms burst"
    );
    eprintln!(
        "measured: isolated change-to-decode median {:.1} ms, max {:.1} ms; 40 changes in {burst_ms} ms sent as {frames} frames",
        latencies[latencies.len() / 2].as_secs_f64() * 1000.0,
        latencies[latencies.len() - 1].as_secs_f64() * 1000.0
    );

    // Input from the controller reaches the window at window-local coordinates.
    let click = |pressed| InputEvent::PointerButton {
        window: id,
        x: 10,
        y: 20,
        button: PointerButton::Left,
        pressed,
    };
    a.send(&ClientMessage::Input(click(true)));
    a.send(&ClientMessage::Input(click(false)));
    window.wait_event("a click at (10, 20)", |e| {
        matches!(e, Event::ButtonPress(b) if b.event == window.id && (b.event_x, b.event_y) == (10, 20) && b.detail == 1)
    });
    a.send(&ClientMessage::Input(InputEvent::Key {
        window: id,
        keysym: 'a'.into(),
        pressed: true,
    }));
    a.send(&ClientMessage::Input(InputEvent::Key {
        window: id,
        keysym: 'a'.into(),
        pressed: false,
    }));
    let mut pressed = Vec::new();
    window.wait_event("key a", |e| {
        if let Event::KeyPress(k) = e {
            pressed.push(k.detail);
        }
        matches!(e, Event::KeyRelease(k) if k.event == window.id)
    });
    assert_eq!(
        pressed
            .iter()
            .map(|&k| window.keysym(k))
            .collect::<Vec<_>>(),
        [0x61]
    );
    // A character missing from the keymap is typed through a remapped scratch keycode.
    a.send(&ClientMessage::Input(InputEvent::Text {
        window: id,
        text: "é".into(),
    }));
    let mut typed = 0;
    window.wait_event("text é", |e| {
        if let Event::KeyPress(k) = e {
            typed = k.detail;
        }
        matches!(e, Event::KeyRelease(_))
    });
    assert_eq!(window.keysym(typed), 0xe9);

    // B asks for control without takeover and is denied; as a viewer it still gets pixels.
    let a_id = a_id.unwrap();
    let mut b = TestClient::connect(&socket, Role::Controller, "b");
    b.wait("denied", |m, _, _| {
        matches!(m, ServerMessage::ControlStatus { holder: Some(h), reason: ControlReason::Denied } if *h == a_id)
    });
    b.wait("viewer keyframe", |_, state, _| shows(state, id, &pattern));
    b.send(&ClientMessage::Input(InputEvent::Focus { window: id }));
    b.wait("not controller", |m, _, _| {
        matches!(
            m,
            ServerMessage::Error {
                code: ErrorCode::NotController,
                ..
            }
        )
    });

    // B takes over; both are told, and A's input is now refused.
    b.send(&ClientMessage::RequestControl { takeover: true });
    let b_id = b.state.client_id().unwrap();
    let took_over = |m: &ServerMessage| matches!(m, ServerMessage::ControlStatus { holder: Some(h), reason: ControlReason::TakenOver } if *h == b_id);
    b.wait("takeover", |m, _, _| took_over(m));
    a.wait("taken over", |m, _, _| took_over(m));
    assert!(b.state.has_control() && !a.state.has_control());
    assert_eq!(a.state.controller(), Some(b_id));
    a.send(&ClientMessage::Input(InputEvent::Focus { window: id }));
    a.wait("not controller", |m, _, _| {
        matches!(
            m,
            ServerMessage::Error {
                code: ErrorCode::NotController,
                ..
            }
        )
    });

    // An override-redirect popup is its own window, attached to the window that last got input.
    window.popup(rect(150, 150, 120, 40));
    let mut popup = None;
    b.wait("popup", |m, _, _| {
        if let ServerMessage::WindowOpened(info) = m {
            popup = Some(info.clone());
        }
        popup.is_some()
    });
    let popup = popup.unwrap();
    assert_eq!(
        (popup.kind, popup.transient_for, popup.x, popup.y),
        (WindowKind::Popup, Some(id), 150, 150)
    );

    // Both clients see a later change.
    pattern.fill(rect(0, 0, 64, 64), [0xff, 0, 0, 0]);
    window.draw(&pattern, rect(0, 0, 64, 64));
    a.wait("change on a", |_, state, _| shows(state, id, &pattern));
    b.wait("change on b", |_, state, _| shows(state, id, &pattern));
    let _ = std::fs::remove_dir_all(dir);
}
