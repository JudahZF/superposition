//! Connected clients: per-client writer threads, backpressure and input control.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    os::unix::net::UnixStream,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
};

use sp_window_stream::{ClientId, ControlReason, ErrorCode, ServerMessage, WindowId, WireMessage};

/// Queued bytes above which a client skips frames and later catches up with a keyframe.
const MAX_FRAME_BACKLOG_BYTES: usize = 16 * 1024 * 1024;

/// An encoded message shared by every client it is sent to.
pub type Encoded = Arc<Vec<u8>>;

/// The sending half of one client's writer thread.
pub struct Outbox {
    tx: mpsc::Sender<Encoded>,
    queued: Arc<AtomicUsize>,
}

impl Outbox {
    /// Starts a writer thread for `stream`. Dropping the outbox lets the thread drain its queue
    /// and then shut the socket down, which also ends the client's reader thread.
    pub fn spawn(mut stream: UnixStream) -> Self {
        let (tx, rx) = mpsc::channel::<Encoded>();
        let queued = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&queued);
        thread::spawn(move || {
            for bytes in rx {
                let sent = stream.write_all(&bytes);
                counter.fetch_sub(bytes.len(), Ordering::Relaxed);
                if sent.is_err() {
                    break;
                }
            }
            let _ = stream.shutdown(std::net::Shutdown::Both);
        });
        Self { tx, queued }
    }

    fn send(&self, bytes: &Encoded) {
        self.queued.fetch_add(bytes.len(), Ordering::Relaxed);
        // A closed writer means the client is going away; its reader reports that.
        let _ = self.tx.send(Arc::clone(bytes));
    }

    fn backlogged(&self) -> bool {
        self.queued.load(Ordering::Relaxed) > MAX_FRAME_BACKLOG_BYTES
    }
}

pub struct Client {
    pub name: String,
    outbox: Outbox,
    pub welcomed: bool,
    /// Windows this client must get a keyframe for before any more deltas.
    pub stale: BTreeSet<WindowId>,
}

/// Every client and who holds control.
#[derive(Default)]
pub struct Clients {
    pub map: BTreeMap<ClientId, Client>,
    pub controller: Option<ClientId>,
}

pub fn encode(message: &ServerMessage) -> Encoded {
    // Server messages are built within their bounds; a failure is a bug, not peer input.
    Arc::new(message.encode().expect("server message within wire bounds"))
}

impl Clients {
    pub fn add(&mut self, id: ClientId, outbox: Outbox) {
        self.map.insert(
            id,
            Client {
                name: String::new(),
                outbox,
                welcomed: false,
                stale: BTreeSet::new(),
            },
        );
    }

    /// Drops a client, releasing control if it held it.
    pub fn remove(&mut self, id: ClientId) {
        if self.map.remove(&id).is_some() && self.controller == Some(id) {
            self.set_controller(None, ControlReason::Released);
        }
    }

    pub fn send(&self, id: ClientId, message: &ServerMessage) {
        if let Some(client) = self.map.get(&id) {
            client.outbox.send(&encode(message));
        }
    }

    pub fn error(&self, id: ClientId, code: ErrorCode, message: impl Into<String>) {
        let mut message = message.into();
        crate::truncate_at_char(&mut message, sp_window_stream::MAX_ERROR_BYTES);
        self.send(id, &ServerMessage::Error { code, message });
    }

    /// Sends to every welcomed client.
    pub fn broadcast(&self, message: &ServerMessage) {
        let bytes = encode(message);
        for client in self.map.values().filter(|c| c.welcomed) {
            client.outbox.send(&bytes);
        }
    }

    fn set_controller(&mut self, holder: Option<ClientId>, reason: ControlReason) {
        self.controller = holder;
        self.broadcast(&ServerMessage::ControlStatus { holder, reason });
    }

    /// Grants free control, takes it over when asked, or tells the requester who holds it.
    pub fn request_control(&mut self, id: ClientId, takeover: bool) {
        match self.controller {
            None => self.set_controller(Some(id), ControlReason::Granted),
            Some(holder) if holder == id => {}
            Some(holder) if takeover => {
                let from = self.map.get(&holder).map_or("", |c| c.name.as_str());
                let to = self.map.get(&id).map_or("", |c| c.name.as_str());
                eprintln!("sp-window-streamer: control taken over from {from:?} by {to:?}");
                self.set_controller(Some(id), ControlReason::TakenOver);
            }
            Some(holder) => self.send(
                id,
                &ServerMessage::ControlStatus {
                    holder: Some(holder),
                    reason: ControlReason::Denied,
                },
            ),
        }
    }

    pub fn release_control(&mut self, id: ClientId) {
        if self.controller == Some(id) {
            self.set_controller(None, ControlReason::Released);
        }
    }

    /// Sends a window's delta frame to up-to-date clients. A client that is backlogged skips it
    /// and is marked stale; stale clients receive `keyframe` instead once they have room.
    /// `keyframe` is built only if some client needs it.
    pub fn send_frame(
        &mut self,
        window: WindowId,
        delta: Option<&[Encoded]>,
        keyframe: impl FnOnce() -> Vec<Encoded>,
    ) {
        let mut keyframe_messages: Option<Vec<Encoded>> = None;
        let mut keyframe = Some(keyframe);
        for client in self.map.values_mut().filter(|c| c.welcomed) {
            if client.outbox.backlogged() {
                if delta.is_some() {
                    client.stale.insert(window);
                }
                continue;
            }
            let messages = if client.stale.remove(&window) {
                keyframe_messages.get_or_insert_with(|| keyframe.take().expect("built once")())
            } else if let Some(delta) = delta {
                delta
            } else {
                continue;
            };
            for bytes in messages {
                client.outbox.send(bytes);
            }
        }
    }

    /// Whether any client still waits for a keyframe of `window`.
    pub fn wants_keyframe(&self, window: WindowId) -> bool {
        self.map
            .values()
            .any(|c| c.welcomed && c.stale.contains(&window))
    }
}
