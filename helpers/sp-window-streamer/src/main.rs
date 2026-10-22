//! `sp-window-streamer`: streams every top-level window of a private X server to window-stream
//! clients over a Unix socket and injects the controller's input.
//!
//! ```text
//! sp-window-streamer --listen PATH [--display :N] [--max-fps N]
//! ```
//!
//! The socket is created with mode 0600. `listening PATH` is printed on stdout once clients can
//! connect. The streamer exits on SIGTERM, SIGINT, stdin EOF (so a supervisor's pipe ends it) or
//! loss of the X server. Idle, every thread blocks: X events, socket reads and the frame timer
//! all wake one channel.

mod clients;
mod display;
mod input;
mod shm;

use std::{
    fs,
    io::{self, Read, Write},
    ops::ControlFlow,
    os::unix::{
        fs::{FileTypeExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use rustix::fs::Mode;
use sp_window_stream::{
    ClientId, ClientMessage, ControlReason, ErrorCode, FrameUpdate, MAX_TILES_PER_UPDATE,
    PROTOCOL_VERSION, Role, ServerMessage, Tile, TileEncoder, WindowId, WireError, WireMessage,
};
use x11rb::{connection::Connection, protocol::xproto::Mapping, rust_connection::RustConnection};

use crate::{
    clients::{Clients, Encoded, Outbox, encode},
    display::{Change, Display, Tracked},
    input::Keyboard,
};

/// Errors from X, the socket and setup; the streamer reports and exits on them.
pub type XResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Tile bytes per frame message, so large keyframes arrive and render in parts.
const MAX_FRAME_CHUNK_BYTES: usize = 4 * 1024 * 1024;

const USAGE: &str = "usage: sp-window-streamer --listen PATH [--display :N] [--max-fps N]";

/// Everything that wakes the main loop.
enum Event {
    X(x11rb::protocol::Event),
    Connected(ClientId, Outbox),
    Message(ClientId, ClientMessage),
    Malformed(ClientId, WireError),
    Disconnected(ClientId),
    Stop(String),
}

struct Args {
    display: Option<String>,
    listen: PathBuf,
    frame_interval: Duration,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut args = std::env::args().skip(1);
        let (mut display, mut listen, mut fps) = (None, None, 60_u32);
        while let Some(flag) = args.next() {
            let mut value = || args.next().ok_or(format!("{flag} needs a value\n{USAGE}"));
            match flag.as_str() {
                "--display" => display = Some(value()?),
                "--listen" => listen = Some(PathBuf::from(value()?)),
                "--max-fps" => {
                    fps = value()?
                        .parse()
                        .ok()
                        .filter(|f| (1..=240).contains(f))
                        .ok_or("--max-fps takes 1 to 240")?;
                }
                _ => return Err(USAGE.into()),
            }
        }
        Ok(Self {
            display,
            listen: listen.ok_or(USAGE)?,
            frame_interval: Duration::from_secs(1) / fps,
        })
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("sp-window-streamer: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> XResult<()> {
    let args = Args::parse()?;
    let display = Display::open(args.display.as_deref())?;
    let listener = bind(&args.listen)?;
    let (tx, rx) = mpsc::channel();
    spawn_x_reader(Arc::clone(&display.conn), tx.clone());
    spawn_acceptor(listener, tx.clone());
    spawn_stop_watchers(tx)?;
    let mut streamer = Streamer {
        keyboard: Keyboard::load(&display)?,
        display,
        clients: Clients::default(),
        encoder: TileEncoder::new()?,
        frame_interval: args.frame_interval,
    };
    streamer.display.scan()?;
    streamer.display.conn.flush()?;
    let capture = if streamer.display.uses_shm() {
        "MIT-SHM"
    } else {
        "GetImage"
    };
    eprintln!("sp-window-streamer: capturing with {capture}");
    println!("listening {}", args.listen.display());
    io::stdout().flush()?;
    let result = streamer.run(&rx);
    let _ = fs::remove_file(&args.listen);
    result
}

/// Binds the socket with mode 0600, replacing a stale socket file but never a live one.
fn bind(path: &Path) -> XResult<UnixListener> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if !metadata.file_type().is_socket() {
            return Err(format!("{} exists and is not a socket", path.display()).into());
        }
        if UnixStream::connect(path).is_ok() {
            return Err(format!("another streamer serves {}", path.display()).into());
        }
        fs::remove_file(path)?;
    }
    // No other thread runs yet, so the process-wide umask change is contained.
    let previous = rustix::process::umask(Mode::from_raw_mode(0o177));
    let listener = UnixListener::bind(path);
    rustix::process::umask(previous);
    let listener = listener?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

fn spawn_x_reader(conn: Arc<RustConnection>, tx: mpsc::Sender<Event>) {
    thread::spawn(move || {
        let error = loop {
            match conn.wait_for_event() {
                Ok(event) => {
                    if tx.send(Event::X(event)).is_err() {
                        return;
                    }
                }
                Err(error) => break error,
            }
        };
        let _ = tx.send(Event::Stop(format!("lost the X server: {error}")));
    });
}

fn spawn_acceptor(listener: UnixListener, tx: mpsc::Sender<Event>) {
    thread::spawn(move || {
        for (next, stream) in (1..).zip(listener.incoming()) {
            let Ok(mut stream) = stream else { continue };
            let Ok(writer) = stream.try_clone() else {
                continue;
            };
            let id = ClientId(next);
            if tx
                .send(Event::Connected(id, Outbox::spawn(writer)))
                .is_err()
            {
                return;
            }
            let tx = tx.clone();
            thread::spawn(move || {
                let mut buf = Vec::new();
                loop {
                    let event = match ClientMessage::read_from(&mut stream, &mut buf) {
                        Ok(message) => Event::Message(id, message),
                        Err(WireError::Closed | WireError::Io(_)) => Event::Disconnected(id),
                        Err(error) => Event::Malformed(id, error),
                    };
                    let last = !matches!(event, Event::Message(..));
                    if tx.send(event).is_err() || last {
                        return;
                    }
                }
            });
        }
    });
}

/// SIGTERM, SIGINT and SIGHUP, and EOF on stdin, all stop the streamer.
fn spawn_stop_watchers(tx: mpsc::Sender<Event>) -> XResult<()> {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    let mut signals = signal_hook::iterator::Signals::new([SIGTERM, SIGINT, SIGHUP])?;
    let signal_tx = tx.clone();
    thread::spawn(move || {
        if let Some(signal) = signals.forever().next() {
            let _ = signal_tx.send(Event::Stop(format!("signal {signal}")));
        }
    });
    thread::spawn(move || {
        let mut buf = [0; 256];
        let mut stdin = io::stdin();
        while matches!(stdin.read(&mut buf), Ok(1..)) {}
        let _ = tx.send(Event::Stop("stdin closed".into()));
    });
    Ok(())
}

struct Streamer {
    display: Display,
    keyboard: Keyboard,
    clients: Clients,
    encoder: TileEncoder,
    frame_interval: Duration,
}

impl Streamer {
    fn run(&mut self, rx: &mpsc::Receiver<Event>) -> XResult<()> {
        loop {
            let first = match self.next_capture_in() {
                None => rx.recv().ok(),
                Some(wait) => match rx.recv_timeout(wait) {
                    Ok(event) => Some(event),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                },
            };
            // Drain everything already queued so a burst of damage costs one capture.
            for event in first
                .into_iter()
                .chain(std::iter::from_fn(|| rx.try_recv().ok()))
            {
                if let ControlFlow::Break(reason) = self.handle(event)? {
                    eprintln!("sp-window-streamer: stopping: {reason}");
                    return Ok(());
                }
            }
            self.capture_due();
            self.display.conn.flush()?;
        }
    }

    fn pending(&self, window: &Tracked) -> bool {
        window.needs_capture()
            || (window.has_frame() && self.clients.wants_keyframe(window.info.id))
    }

    /// Time until the earliest pending window may be captured, or `None` to sleep until an
    /// event arrives.
    fn next_capture_in(&self) -> Option<Duration> {
        let now = Instant::now();
        self.display
            .windows
            .values()
            .filter(|w| self.pending(w))
            .map(|w| {
                w.last_capture.map_or(Duration::ZERO, |at| {
                    (at + self.frame_interval).saturating_duration_since(now)
                })
            })
            .min()
    }

    fn capture_due(&mut self) {
        let now = Instant::now();
        let due: Vec<WindowId> = self
            .display
            .windows
            .values()
            .filter(|w| {
                self.pending(w)
                    && w.last_capture
                        .is_none_or(|at| at + self.frame_interval <= now)
            })
            .map(|w| w.info.id)
            .collect();
        for id in due {
            let captured = match self.display.capture(id, &mut self.encoder) {
                Ok(captured) => captured,
                // Usually the window vanished; its unmap or destroy event follows.
                Err(error) => {
                    eprintln!("sp-window-streamer: capture of {:#x} failed: {error}", id.0);
                    None
                }
            };
            let Some(window) = self.display.windows.get(&id) else {
                continue;
            };
            let delta = captured.map(|c| frame_messages(window, &c.tiles, c.keyframe));
            self.clients.send_frame(id, delta.as_deref(), || {
                frame_messages(window, &window.tiles, true)
            });
        }
    }

    fn handle(&mut self, event: Event) -> XResult<ControlFlow<String>> {
        match event {
            Event::X(x11rb::protocol::Event::MappingNotify(e))
                if e.request == Mapping::KEYBOARD =>
            {
                self.keyboard.reload(&self.display)?;
            }
            Event::X(event) => {
                if let Some(change) = self.display.handle(&event)? {
                    self.announce(change);
                }
            }
            Event::Connected(id, outbox) => self.clients.add(id, outbox),
            Event::Message(id, message) => self.on_message(id, message),
            Event::Malformed(id, error) => {
                self.clients
                    .error(id, ErrorCode::Malformed, error.to_string());
                self.clients.remove(id);
            }
            Event::Disconnected(id) => self.clients.remove(id),
            Event::Stop(reason) => return Ok(ControlFlow::Break(reason)),
        }
        Ok(ControlFlow::Continue(()))
    }

    fn announce(&mut self, change: Change) {
        let message = match change {
            Change::Opened(info) => ServerMessage::WindowOpened(info),
            Change::Changed(info) => ServerMessage::WindowChanged(info),
            Change::Closed(id) => {
                for client in self.clients.map.values_mut() {
                    client.stale.remove(&id);
                }
                ServerMessage::WindowClosed(id)
            }
        };
        self.clients.broadcast(&message);
    }

    fn on_message(&mut self, id: ClientId, message: ClientMessage) {
        let Some(client) = self.clients.map.get_mut(&id) else {
            return;
        };
        match message {
            ClientMessage::Hello {
                version,
                role,
                name,
            } => {
                if client.welcomed {
                    return self
                        .clients
                        .error(id, ErrorCode::Malformed, "repeated Hello");
                }
                if version != PROTOCOL_VERSION {
                    self.clients.error(
                        id,
                        ErrorCode::UnsupportedVersion,
                        format!("streamer speaks version {PROTOCOL_VERSION}"),
                    );
                    return self.clients.remove(id);
                }
                client.welcomed = true;
                client.name = name;
                client.stale = self.display.windows.keys().copied().collect();
                self.clients.send(
                    id,
                    &ServerMessage::Welcome {
                        version: PROTOCOL_VERSION,
                        client: id,
                    },
                );
                let windows = self
                    .display
                    .windows
                    .values()
                    .map(|w| w.info.clone())
                    .collect();
                self.clients.send(id, &ServerMessage::WindowList(windows));
                match role {
                    Role::Controller => self.clients.request_control(id, false),
                    Role::Viewer => {
                        let holder = self.clients.controller;
                        self.clients.send(
                            id,
                            &ServerMessage::ControlStatus {
                                holder,
                                reason: ControlReason::Current,
                            },
                        );
                    }
                }
            }
            _ if !client.welcomed => {
                self.clients
                    .error(id, ErrorCode::Malformed, "expected Hello");
                self.clients.remove(id);
            }
            ClientMessage::RequestControl { takeover } => {
                self.clients.request_control(id, takeover);
            }
            ClientMessage::ReleaseControl => self.clients.release_control(id),
            ClientMessage::RequestKeyframe { window: None } => {
                client.stale = self.display.windows.keys().copied().collect();
            }
            ClientMessage::RequestKeyframe {
                window: Some(window),
            } => {
                if self.display.windows.contains_key(&window) {
                    client.stale.insert(window);
                } else {
                    self.clients.error(
                        id,
                        ErrorCode::UnknownWindow,
                        format!("window {:#x} is not streamed", window.0),
                    );
                }
            }
            ClientMessage::Input(event) => {
                if self.clients.controller != Some(id) {
                    return self
                        .clients
                        .error(id, ErrorCode::NotController, "input needs control");
                }
                if let Err((code, message)) =
                    input::inject(&mut self.display, &mut self.keyboard, &event)
                {
                    self.clients.error(id, code, message);
                }
            }
        }
    }
}

/// Encodes tiles as one or more frame messages that share the window's current sequence.
fn frame_messages(window: &Tracked, tiles: &[Tile], keyframe: bool) -> Vec<Encoded> {
    let info = &window.info;
    let message = |tiles: Vec<Tile>, complete| {
        encode(&ServerMessage::Frame(FrameUpdate {
            window: info.id,
            sequence: window.sequence,
            width: info.width,
            height: info.height,
            keyframe,
            complete,
            tiles,
        }))
    };
    let mut messages = Vec::new();
    let mut chunk: Vec<Tile> = Vec::new();
    let mut bytes = 0;
    for tile in tiles {
        if !chunk.is_empty()
            && (bytes + tile.data.len() > MAX_FRAME_CHUNK_BYTES
                || chunk.len() == MAX_TILES_PER_UPDATE)
        {
            messages.push(message(std::mem::take(&mut chunk), false));
            bytes = 0;
        }
        bytes += tile.data.len();
        chunk.push(tile.clone());
    }
    messages.push(message(chunk, true));
    messages
}

/// Shortens `text` to at most `max` bytes without splitting a character.
fn truncate_at_char(text: &mut String, max: usize) {
    if text.len() > max {
        let end = (0..=max)
            .rev()
            .find(|&i| text.is_char_boundary(i))
            .unwrap_or(0);
        text.truncate(end);
    }
}
