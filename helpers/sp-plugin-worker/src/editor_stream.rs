//! macOS: streams this worker's editor windows to the app, which shows them to remote clients.
//!
//! The app names its socket in [`sp_window_stream::EDITOR_STREAM_ENV`]. A thread connects to it
//! and serves the window-stream session for this worker's own windows ([`WindowSource`]); the
//! app speaks first, as it does to the Linux streamer. While the app listens, the loading thread
//! keeps the window server's captures of the open editors going and hands over each new picture
//! as soon as this thread has written the previous ones: up to 50 a second while the pictures
//! change or input comes, 10 a second after [`ACTIVE_HOLD`] without either. It injects the input
//! that comes back. The app says hello only while a remote client watches, so an idle worker
//! pictures nothing.

use std::{
    io,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use sp_vst3::editor_input::EditorInput;
use sp_window_stream::{
    ClientMessage, ErrorCode, ServerMessage, SourceAction, WindowId, WindowInfo, WindowSource,
    WireError, WireMessage,
};

use crate::remote_input::InputMapper;

/// The shortest time between the starts of two captures of a window while its pictures change
/// or input comes: up to 50 a second.
const CAPTURE_INTERVAL: Duration = Duration::from_millis(20);
/// The shortest time between the starts of two captures of a still window: 10 a second.
const STILL_CAPTURE_INTERVAL: Duration = Duration::from_millis(100);
/// How long the pictures stay at [`CAPTURE_INTERVAL`] after the last change or input.
const ACTIVE_HOLD: Duration = Duration::from_secs(1);
/// The shortest time between two copies of the editors' views, when the window server gives no
/// picture: 30 per second.
const COPY_INTERVAL: Duration = Duration::from_millis(33);
/// The pause before connecting to the app's socket again, after an attempt that failed or a
/// connection on which the app never said hello.
const RECONNECT_INTERVAL: Duration = Duration::from_secs(1);

/// One open editor's picture: its window, and BGRX pixels of `info.width` x `info.height`.
pub(crate) struct Capture {
    pub(crate) info: WindowInfo,
    pub(crate) bgrx: Vec<u8>,
}

enum Event {
    Windows(Vec<WindowInfo>),
    Frame(Capture),
    /// A message from connection `generation`; an error ends that connection.
    Client(u64, Result<ClientMessage, WireError>),
}

#[derive(Default)]
struct Shared {
    /// The app said hello on the current connection.
    streaming: AtomicBool,
    /// Pictures handed to the thread and not yet written to the socket.
    frames_in_flight: AtomicUsize,
    /// A picture differed from the one before it since the loading thread last looked.
    changed: AtomicBool,
}

/// The loading thread's end of the stream.
pub(crate) struct EditorStream {
    events: Sender<Event>,
    input: Receiver<(WindowId, EditorInput)>,
    shared: Arc<Shared>,
    published: Vec<WindowInfo>,
    next_copy: Instant,
    /// Until when the captures run at [`CAPTURE_INTERVAL`].
    active_until: Instant,
}

impl EditorStream {
    /// Starts the thread that serves `socket`. It lives as long as the worker.
    pub(crate) fn spawn(socket: PathBuf) -> io::Result<Self> {
        let (events, events_rx) = mpsc::channel();
        let (input_tx, input) = mpsc::channel();
        let shared = Arc::new(Shared::default());
        let thread_events = events.clone();
        let thread_shared = Arc::clone(&shared);
        thread::Builder::new()
            .name("sp-editor-stream".into())
            .spawn(move || {
                let mut server = match Server::new(input_tx, thread_shared) {
                    Ok(server) => server,
                    Err(error) => {
                        eprintln!("sp-plugin-worker: editor stream unavailable: {error}");
                        return;
                    }
                };
                server.run(&socket, &events_rx, &thread_events);
            })?;
        Ok(Self {
            events,
            input,
            shared,
            published: Vec::new(),
            next_copy: Instant::now(),
            active_until: Instant::now(),
        })
    }

    /// Input from the app for this worker's windows, in arrival order. Input makes the captures
    /// fast at once, so the picture that shows its effect comes soon.
    pub(crate) fn take_input(&mut self, now: Instant) -> Vec<(WindowId, EditorInput)> {
        let input: Vec<_> = self.input.try_iter().collect();
        if !input.is_empty() {
            self.active_until = now + ACTIVE_HOLD;
        }
        input
    }

    /// The shortest time between the starts of two captures of a window now:
    /// [`CAPTURE_INTERVAL`] within [`ACTIVE_HOLD`] of a changed picture or of input, else
    /// [`STILL_CAPTURE_INTERVAL`].
    pub(crate) fn capture_interval(&mut self, now: Instant) -> Duration {
        if self.shared.changed.swap(false, Ordering::AcqRel) {
            self.active_until = now + ACTIVE_HOLD;
        }
        if now < self.active_until {
            CAPTURE_INTERVAL
        } else {
            STILL_CAPTURE_INTERVAL
        }
    }

    /// Whether the app listens: it said hello on the current connection.
    pub(crate) fn streaming(&self) -> bool {
        self.shared.streaming.load(Ordering::Acquire)
    }

    /// Whether new pictures may go now: the app listens and the last pictures are written.
    pub(crate) fn ready(&self) -> bool {
        self.streaming() && self.shared.frames_in_flight.load(Ordering::Acquire) == 0
    }

    /// Whether a copy of the views is due, for a window that the window server does not
    /// picture: at most one each [`COPY_INTERVAL`].
    pub(crate) fn copy_due(&mut self, now: Instant) -> bool {
        let due = now >= self.next_copy;
        if due {
            self.next_copy = now + COPY_INTERVAL;
        }
        due
    }

    /// Sends the window list if it changed, then the new pictures. The list holds each window of
    /// `open` that has had a picture, with the size of its last one.
    pub(crate) fn publish(&mut self, open: &[WindowId], captures: Vec<Capture>) {
        let mut windows: Vec<WindowInfo> = (self.published.iter())
            .filter(|window| {
                open.contains(&window.id) && !captures.iter().any(|c| c.info.id == window.id)
            })
            .cloned()
            .collect();
        windows.extend(captures.iter().map(|c| c.info.clone()));
        windows.sort_by_key(|window| window.id);
        if windows != self.published {
            self.published.clone_from(&windows);
            // A closed channel means the thread failed to start; it logged why.
            let _ = self.events.send(Event::Windows(windows));
        }
        for capture in captures {
            self.shared.frames_in_flight.fetch_add(1, Ordering::AcqRel);
            if self.events.send(Event::Frame(capture)).is_err() {
                self.shared.frames_in_flight.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }
}

/// The stream thread's state across connections.
struct Server {
    source: WindowSource,
    mapper: InputMapper,
    input: Sender<(WindowId, EditorInput)>,
    shared: Arc<Shared>,
    out: Vec<ServerMessage>,
    /// Pictures handled since `out` was last written.
    frames_handled: usize,
    generation: u64,
}

impl Server {
    fn new(input: Sender<(WindowId, EditorInput)>, shared: Arc<Shared>) -> io::Result<Self> {
        Ok(Self {
            source: WindowSource::new()?,
            mapper: InputMapper::default(),
            input,
            shared,
            out: Vec::new(),
            frames_handled: 0,
            generation: 0,
        })
    }

    /// Lets the loading thread picture again once handled pictures are written (or dropped).
    fn wrote_frames(&mut self) {
        self.shared
            .frames_in_flight
            .fetch_sub(std::mem::take(&mut self.frames_handled), Ordering::AcqRel);
    }

    fn run(&mut self, socket: &Path, events: &Receiver<Event>, events_tx: &Sender<Event>) {
        loop {
            if let Ok(stream) = UnixStream::connect(socket) {
                if let Err(error) = self.serve(&stream, events, events_tx) {
                    eprintln!("sp-plugin-worker: editor stream closed: {error}");
                }
                let _ = stream.shutdown(std::net::Shutdown::Both);
                let greeted = self.source.streaming();
                self.source.disconnect();
                self.mapper = InputMapper::default();
                self.shared.streaming.store(false, Ordering::Release);
                // The app hangs up when no client watches any more: be ready at once for the
                // next client, which may come within a second.
                if greeted {
                    continue;
                }
            }
            // Wait before the next attempt, keeping the window list current meanwhile.
            let until = Instant::now() + RECONNECT_INTERVAL;
            while let Some(wait) = until.checked_duration_since(Instant::now()) {
                match events.recv_timeout(wait) {
                    Ok(event) => {
                        self.handle(event);
                        self.out.clear();
                        self.wrote_frames();
                    }
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        }
    }

    /// Serves one connection until it closes or fails.
    fn serve(
        &mut self,
        stream: &UnixStream,
        events: &Receiver<Event>,
        events_tx: &Sender<Event>,
    ) -> Result<(), WireError> {
        self.generation += 1;
        let generation = self.generation;
        let mut reader = stream.try_clone()?;
        let reader_events = events_tx.clone();
        thread::Builder::new()
            .name("sp-editor-stream-reader".into())
            .spawn(move || {
                let mut buffer = Vec::new();
                loop {
                    let message = ClientMessage::read_from(&mut reader, &mut buffer);
                    let last = message.is_err();
                    if reader_events
                        .send(Event::Client(generation, message))
                        .is_err()
                        || last
                    {
                        return;
                    }
                }
            })?;
        let mut writer = stream;
        // `events_tx` keeps the channel open, so `recv` fails only if the worker is exiting.
        while let Ok(event) = events.recv() {
            let open = self.handle(event);
            let written = self
                .out
                .drain(..)
                .try_for_each(|message| message.write_to(&mut writer));
            self.wrote_frames();
            self.shared
                .streaming
                .store(self.source.streaming(), Ordering::Release);
            written?;
            if !open {
                break;
            }
        }
        Ok(())
    }

    /// Applies one event, queueing messages in `self.out`. Returns false when the connection
    /// must close after they are sent.
    fn handle(&mut self, event: Event) -> bool {
        match event {
            Event::Windows(windows) => self.source.set_windows(windows, &mut self.out),
            Event::Frame(Capture { info, bgrx }) => {
                let queued = self.out.len();
                if let Err(error) =
                    self.source
                        .update_frame(info.id, info.width, info.height, bgrx, &mut self.out)
                {
                    eprintln!("sp-plugin-worker: editor picture dropped: {error}");
                }
                if self.out.len() > queued {
                    self.shared.changed.store(true, Ordering::Release);
                }
                self.frames_handled += 1;
            }
            Event::Client(generation, _) if generation != self.generation => {}
            Event::Client(_, Ok(message)) => match self.source.receive(message, &mut self.out) {
                SourceAction::None => {}
                SourceAction::Input(event) => match self.mapper.map(&event, Instant::now()) {
                    Ok(inputs) => {
                        for input in inputs {
                            let _ = self.input.send((event.window(), input));
                        }
                    }
                    Err(message) => self.out.push(ServerMessage::Error {
                        code: ErrorCode::Unsupported,
                        message,
                    }),
                },
                SourceAction::Close => return false,
            },
            Event::Client(_, Err(WireError::Closed | WireError::Io(_))) => return false,
            Event::Client(_, Err(error)) => {
                self.out.push(ServerMessage::Error {
                    code: ErrorCode::Malformed,
                    message: error.to_string(),
                });
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use std::{
        os::unix::net::{UnixListener, UnixStream},
        sync::atomic::Ordering,
        time::{Duration, Instant},
    };

    use sp_vst3::editor_input::{EditorInput, Modifiers, MouseButton};
    use sp_window_stream::{
        ClientMessage, InputEvent, PROTOCOL_VERSION, PointerButton, Role, ServerMessage, WindowId,
        WindowInfo, WindowKind, WindowStreamClient, WireMessage,
    };

    use super::{
        ACTIVE_HOLD, CAPTURE_INTERVAL, COPY_INTERVAL, Capture, EditorStream, STILL_CAPTURE_INTERVAL,
    };

    /// Polls `condition` for up to five seconds.
    fn eventually<T>(mut condition: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(value) = condition() {
                return value;
            }
            assert!(Instant::now() < deadline, "timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn read(app: &mut UnixStream, client: &mut WindowStreamClient) -> ServerMessage {
        let message = ServerMessage::read_from(app, &mut Vec::new()).unwrap();
        client.apply(&message).unwrap();
        message
    }

    #[test]
    #[allow(clippy::too_many_lines, reason = "one session, step by step")]
    fn serves_pictures_while_greeted_and_returns_input_as_appkit_events() {
        let directory =
            std::env::temp_dir().join(format!("sp-editor-stream-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let socket = directory.join("editors.sock");
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();
        let mut stream = EditorStream::spawn(socket).unwrap();
        let (mut app, _) = listener.accept().unwrap();
        app.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        // Connected but not greeted: nothing is pictured.
        assert!(!stream.streaming() && !stream.ready());

        ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            role: Role::Controller,
            name: "test".into(),
        }
        .write_to(&mut app)
        .unwrap();
        let mut client = WindowStreamClient::new().unwrap();
        for _ in 0..3 {
            read(&mut app, &mut client);
        }
        assert!(client.has_control());
        eventually(|| stream.ready().then_some(()));

        let window = WindowId(4711);
        let info = WindowInfo {
            id: window,
            kind: WindowKind::Normal,
            title: "Editor".into(),
            x: 0,
            y: 0,
            width: 2,
            height: 1,
            rack: Some(0),
            slot: Some(1),
            pid: None,
            transient_for: None,
        };
        stream.publish(
            &[window],
            vec![Capture {
                info: info.clone(),
                bgrx: vec![10, 20, 30, 0, 40, 50, 60, 0],
            }],
        );
        assert!(matches!(
            read(&mut app, &mut client),
            ServerMessage::WindowOpened(_)
        ));
        assert!(matches!(
            read(&mut app, &mut client),
            ServerMessage::Frame(_)
        ));
        assert_eq!(
            client.window(window).unwrap().rgba(),
            [30, 20, 10, 255, 60, 50, 40, 255]
        );
        // The worker's own copy of the views, for a window that the window server does not
        // picture, is due once each interval.
        let now = Instant::now();
        assert!(stream.copy_due(now) && !stream.copy_due(now));
        assert!(stream.copy_due(now + COPY_INTERVAL));
        // A changed picture makes the captures fast; they slow down when nothing changes.
        assert_eq!(stream.capture_interval(Instant::now()), CAPTURE_INTERVAL);
        assert_eq!(
            stream.capture_interval(Instant::now() + ACTIVE_HOLD),
            STILL_CAPTURE_INTERVAL
        );
        // An open window without a new picture stays.
        stream.publish(&[window], Vec::new());
        stream.publish(
            &[window],
            vec![Capture {
                info,
                bgrx: vec![10, 20, 30, 0, 0, 0, 0, 0],
            }],
        );
        assert!(matches!(
            read(&mut app, &mut client),
            ServerMessage::Frame(_)
        ));

        ClientMessage::Input(InputEvent::PointerButton {
            window,
            x: 1,
            y: 0,
            button: PointerButton::Left,
            pressed: true,
        })
        .write_to(&mut app)
        .unwrap();
        assert_eq!(
            eventually(|| stream.take_input(Instant::now()).into_iter().next()),
            (
                window,
                EditorInput::MouseButton {
                    x: 1.0,
                    y: 0.0,
                    button: MouseButton::Left,
                    pressed: true,
                    click_count: 1,
                    modifiers: Modifiers::default(),
                }
            )
        );

        // A window that is not open any more goes.
        stream.publish(&[], Vec::new());
        assert_eq!(
            read(&mut app, &mut client),
            ServerMessage::WindowClosed(window)
        );

        // When the app hangs up, picturing stops and the worker waits on a new connection.
        drop(app);
        eventually(|| (!stream.shared.streaming.load(Ordering::Acquire)).then_some(()));
        let (_app, _) = listener.accept().unwrap();
        assert!(!stream.shared.streaming.load(Ordering::Acquire));
        let _ = std::fs::remove_dir_all(&directory);
    }
}
