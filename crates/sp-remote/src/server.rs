//! The engine side: accepts clients, pairs unknown ones, and fans out rendered frames.
//!
//! The app drives its UI against a headless `egui::Context` and calls [`RemoteServer::take_input`]
//! before each frame and [`RemoteServer::publish`] after it. Networking runs on a private tokio
//! runtime; these calls only lock short critical sections and never wait for the network.

use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use egui::{CursorIcon, OutputCommand, TexturesDelta};
use tokio::sync::{Notify, mpsc};

use crate::{
    Fingerprint, Identity, RemoteError, Role, TrustStore, WireMesh,
    protocol::{
        ClientMessage, Frame, PROTOCOL_VERSION, ServerMessage, pairing, read_control,
        write_control, write_frame,
    },
    textures::TextureMirror,
    tls,
};

/// Failed pairing attempts before the code is withdrawn.
const MAX_PAIRING_FAILURES: u8 = 5;

/// How to run the server.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// UDP address to listen on, for example `[::]:4433`.
    pub bind: SocketAddr,
    /// Directory for the identity and the trusted-client list.
    pub state_dir: PathBuf,
    /// The engine name clients see.
    pub name: String,
}

/// A connected client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientInfo {
    /// Connection identifier, unique for the server's lifetime.
    pub id: u64,
    /// The name the client gave.
    pub name: String,
    /// The client's certificate fingerprint.
    pub fingerprint: Fingerprint,
    /// The client's role.
    pub role: Role,
    /// The client's address.
    pub address: SocketAddr,
}

struct FrameBody {
    sequence: u64,
    meshes: Arc<Vec<WireMesh>>,
    cursor: CursorIcon,
}

/// What a client has not been sent yet. Texture deltas accumulate; meshes keep only the latest.
#[derive(Default)]
struct Pending {
    textures: Vec<TexturesDelta>,
    frame: Option<FrameBody>,
    copied_text: String,
}

#[derive(Default)]
struct Outbox {
    pending: Mutex<Pending>,
    ready: Notify,
}

struct Client {
    info: ClientInfo,
    outbox: Arc<Outbox>,
    control: mpsc::UnboundedSender<ServerMessage>,
}

struct Pairing {
    code: String,
    expires: Instant,
    failures: u8,
}

struct State {
    trust: TrustStore,
    pairing: Option<Pairing>,
    clients: HashMap<u64, Client>,
    controller: Option<u64>,
    next_id: u64,
    inputs: Vec<egui::RawInput>,
    textures: TextureMirror,
    sequence: u64,
    latest: Option<(Arc<Vec<WireMesh>>, CursorIcon)>,
}

struct Shared {
    name: String,
    fingerprint: Fingerprint,
    state: Mutex<State>,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A running remote server.
pub struct RemoteServer {
    shared: Arc<Shared>,
    local_addr: SocketAddr,
    _runtime: tokio::runtime::Runtime,
}

impl RemoteServer {
    /// Loads or creates the identity and starts listening.
    ///
    /// # Errors
    /// Returns an error when the identity, trust store, or socket cannot be set up.
    pub fn start(config: ServerConfig) -> Result<Self, RemoteError> {
        let identity = Identity::load_or_create(&config.state_dir)?;
        let trust = TrustStore::load(&config.state_dir)?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("sp-remote")
            .enable_all()
            .build()?;
        let endpoint = {
            let _guard = runtime.enter();
            quinn::Endpoint::server(tls::server_config(&identity)?, config.bind)?
        };
        let local_addr = endpoint.local_addr()?;
        let shared = Arc::new(Shared {
            name: config.name,
            fingerprint: identity.fingerprint(),
            state: Mutex::new(State {
                trust,
                pairing: None,
                clients: HashMap::new(),
                controller: None,
                next_id: 1,
                inputs: Vec::new(),
                textures: TextureMirror::default(),
                sequence: 0,
                latest: None,
            }),
        });
        let accepting = Arc::clone(&shared);
        runtime.spawn(async move {
            while let Some(incoming) = endpoint.accept().await {
                let shared = Arc::clone(&accepting);
                tokio::spawn(async move {
                    // A failed client affects only its own connection.
                    let _ = serve_client(incoming, shared).await;
                });
            }
        });
        Ok(Self {
            shared,
            local_addr,
            _runtime: runtime,
        })
    }

    /// The bound address (useful with port 0).
    #[must_use]
    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// This engine's certificate fingerprint.
    #[must_use]
    pub fn fingerprint(&self) -> Fingerprint {
        self.shared.fingerprint
    }

    /// Opens pairing for `lifetime` and returns the six-digit code to show. One client may pair
    /// with it; five wrong attempts withdraw it.
    pub fn open_pairing(&self, lifetime: Duration) -> String {
        let code = pairing::new_code();
        self.shared.state().pairing = Some(Pairing {
            code: code.clone(),
            expires: Instant::now() + lifetime,
            failures: 0,
        });
        code
    }

    /// The current pairing code, if pairing is open.
    #[must_use]
    pub fn pairing_code(&self) -> Option<String> {
        let state = self.shared.state();
        state
            .pairing
            .as_ref()
            .filter(|pairing| pairing.expires > Instant::now())
            .map(|pairing| pairing.code.clone())
    }

    /// Connected clients.
    #[must_use]
    pub fn clients(&self) -> Vec<ClientInfo> {
        let mut clients: Vec<_> = self
            .shared
            .state()
            .clients
            .values()
            .map(|client| client.info.clone())
            .collect();
        clients.sort_by_key(|client| client.id);
        clients
    }

    /// Forgets a paired client and closes its connection.
    ///
    /// # Errors
    /// Returns an error when the trust store cannot be written.
    pub fn forget_client(&self, fingerprint: Fingerprint) -> Result<(), RemoteError> {
        let mut state = self.shared.state();
        state.trust.forget_client(fingerprint)?;
        for client in state.clients.values() {
            if client.info.fingerprint == fingerprint {
                let _ = client
                    .control
                    .send(ServerMessage::Rejected("this client was unpaired".into()));
            }
        }
        Ok(())
    }

    /// Input the controller sent since the last call, merged into one frame of input:
    /// events in order, window size and modifiers from the newest. `None` without input.
    #[must_use]
    pub fn take_input(&self) -> Option<egui::RawInput> {
        let inputs = std::mem::take(&mut self.shared.state().inputs);
        let mut inputs = inputs.into_iter();
        let mut merged = inputs.next()?;
        for input in inputs {
            let mut events = std::mem::take(&mut merged.events);
            events.extend(input.events.iter().cloned());
            merged = egui::RawInput { events, ..input };
        }
        Some(merged)
    }

    /// Tessellates one frame and queues it for every client.
    pub fn publish(&self, ctx: &egui::Context, output: &egui::FullOutput) {
        let meshes: Vec<WireMesh> = ctx
            .tessellate(output.shapes.clone(), output.pixels_per_point)
            .into_iter()
            .filter_map(|primitive| match primitive.primitive {
                egui::epaint::Primitive::Mesh(mesh) => Some(WireMesh {
                    clip_rect: primitive.clip_rect,
                    mesh,
                }),
                // Paint callbacks run GPU code on the engine; a client cannot replay them.
                egui::epaint::Primitive::Callback(_) => None,
            })
            .collect();
        let meshes = Arc::new(meshes);
        let cursor = output.platform_output.cursor_icon;
        let copied: String = output
            .platform_output
            .commands
            .iter()
            .filter_map(|command| match command {
                OutputCommand::CopyText(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        let has_textures = !output.textures_delta.is_empty();
        let mut state = self.shared.state();
        state.textures.apply(&output.textures_delta);
        state.sequence += 1;
        let sequence = state.sequence;
        state.latest = Some((Arc::clone(&meshes), cursor));
        for client in state.clients.values() {
            let mut pending = client
                .outbox
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if has_textures {
                pending.textures.push(output.textures_delta.clone());
            }
            pending.copied_text.push_str(&copied);
            pending.frame = Some(FrameBody {
                sequence,
                meshes: Arc::clone(&meshes),
                cursor,
            });
            drop(pending);
            client.outbox.ready.notify_one();
        }
    }
}

async fn serve_client(incoming: quinn::Incoming, shared: Arc<Shared>) -> Result<(), RemoteError> {
    let connection = incoming.await?;
    let fingerprint = tls::peer_fingerprint(&connection).ok_or(RemoteError::NoPeerIdentity)?;
    let (mut send, mut receive) = connection.accept_bi().await?;
    let Some(ClientMessage::Hello {
        version,
        name,
        role,
    }) = read_control(&mut receive).await?
    else {
        return Err(RemoteError::Protocol("expected Hello"));
    };
    if version != PROTOCOL_VERSION {
        let reason =
            format!("protocol {version} is not supported; the engine speaks {PROTOCOL_VERSION}");
        write_control(&mut send, &ServerMessage::Rejected(reason)).await?;
        return Ok(());
    }
    let known = shared.state().trust.client(fingerprint).is_some();
    if !known && !pair(&shared, &mut send, &mut receive, fingerprint, &name).await? {
        return Ok(());
    }

    let (control, outgoing) = mpsc::unbounded_channel();
    let outbox = Arc::new(Outbox::default());
    let (id, role) = {
        let mut state = shared.state();
        let id = state.next_id;
        state.next_id += 1;
        // A device that reconnects replaces its previous session, which may not have closed.
        let previous: Vec<u64> = state
            .clients
            .values()
            .filter(|client| client.info.fingerprint == fingerprint)
            .map(|client| client.info.id)
            .collect();
        for previous in previous {
            if let Some(client) = state.clients.remove(&previous) {
                let _ = client.control.send(ServerMessage::Rejected(
                    "replaced by a new connection".into(),
                ));
            }
            if state.controller == Some(previous) {
                state.controller = None;
            }
        }
        let role = if role == Role::Controller && state.controller.is_none() {
            state.controller = Some(id);
            Role::Controller
        } else {
            Role::Viewer
        };
        {
            let mut pending = outbox
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            pending.textures.push(state.textures.snapshot());
            pending.frame = state.latest.as_ref().map(|(meshes, cursor)| FrameBody {
                sequence: state.sequence,
                meshes: Arc::clone(meshes),
                cursor: *cursor,
            });
        }
        state.clients.insert(
            id,
            Client {
                info: ClientInfo {
                    id,
                    name,
                    fingerprint,
                    role,
                    address: connection.remote_address(),
                },
                outbox: Arc::clone(&outbox),
                control,
            },
        );
        (id, role)
    };
    let result = run_client(
        &shared,
        &connection,
        id,
        role,
        outbox,
        send,
        receive,
        outgoing,
    )
    .await;
    let mut state = shared.state();
    state.clients.remove(&id);
    if state.controller == Some(id) {
        state.controller = None;
    }
    result
}

/// Pairs an unknown client. Returns `false` when the client was refused.
async fn pair(
    shared: &Shared,
    send: &mut quinn::SendStream,
    receive: &mut quinn::RecvStream,
    client: Fingerprint,
    name: &str,
) -> Result<bool, RemoteError> {
    write_control(send, &ServerMessage::PairingRequired).await?;
    let Some(ClientMessage::PairStart { spake }) = read_control(receive).await? else {
        return Ok(false);
    };
    let code = {
        let state = shared.state();
        state
            .pairing
            .as_ref()
            .filter(|pairing| pairing.expires > Instant::now())
            .map(|pairing| pairing.code.clone())
    };
    let Some(code) = code else {
        let reason = "pairing is not open on the engine; open it to get a code".to_owned();
        write_control(send, &ServerMessage::Rejected(reason)).await?;
        return Ok(false);
    };
    let (exchange, reply) = pairing::start_server(&code);
    let key = exchange
        .finish(&spake)
        .map_err(|_| RemoteError::Protocol("malformed pairing message"))?;
    let mac = pairing::confirmation(&key, true, client, shared.fingerprint);
    write_control(send, &ServerMessage::PairReply { spake: reply, mac }).await?;
    let Some(ClientMessage::PairConfirm { mac }) = read_control(receive).await? else {
        return Ok(false);
    };
    let verified = pairing::verify(&key, false, client, shared.fingerprint, &mac);
    {
        let mut state = shared.state();
        if verified {
            state.pairing = None;
            state.trust.pin_client(name.to_owned(), client)?;
        } else if let Some(pairing) = state.pairing.as_mut() {
            pairing.failures += 1;
            if pairing.failures >= MAX_PAIRING_FAILURES {
                state.pairing = None;
            }
        }
    }
    if !verified {
        write_control(send, &ServerMessage::Rejected("wrong pairing code".into())).await?;
    }
    Ok(verified)
}

/// Reads control messages on their own task: `read_exact` is not cancel-safe, so a stream must
/// never be read inside `select!`.
pub(crate) fn spawn_reader<T: serde::de::DeserializeOwned + Send + 'static>(
    mut receive: quinn::RecvStream,
) -> (
    mpsc::UnboundedReceiver<Result<T, RemoteError>>,
    tokio::task::JoinHandle<()>,
) {
    let (sender, messages) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        loop {
            match read_control::<T>(&mut receive).await {
                Ok(Some(message)) => {
                    if sender.send(Ok(message)).is_err() {
                        return;
                    }
                }
                Ok(None) => return,
                Err(error) => {
                    let _ = sender.send(Err(error));
                    return;
                }
            }
        }
    });
    (messages, task)
}

/// Sends each newest frame, with every texture change since the last one sent.
async fn write_frames(mut frames: quinn::SendStream, outbox: Arc<Outbox>) {
    loop {
        outbox.ready.notified().await;
        let frame = {
            let mut pending = outbox
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            // Textures wait for a frame to carry them, so none is lost before the first.
            pending.frame.take().map(|body| Frame {
                sequence: body.sequence,
                textures: std::mem::take(&mut pending.textures),
                meshes: body.meshes.as_ref().clone(),
                cursor: body.cursor,
                copied_text: std::mem::take(&mut pending.copied_text),
            })
        };
        if let Some(frame) = frame
            && write_frame(&mut frames, &frame).await.is_err()
        {
            return;
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "one connection's streams and state"
)]
async fn run_client(
    shared: &Shared,
    connection: &quinn::Connection,
    id: u64,
    role: Role,
    outbox: Arc<Outbox>,
    mut send: quinn::SendStream,
    receive: quinn::RecvStream,
    mut outgoing: mpsc::UnboundedReceiver<ServerMessage>,
) -> Result<(), RemoteError> {
    let welcome = ServerMessage::Welcome {
        role,
        server_name: shared.name.clone(),
    };
    write_control(&mut send, &welcome).await?;
    let frames = connection.open_uni().await?;
    let writer = tokio::spawn(write_frames(frames, outbox));
    let (mut incoming, reader) = spawn_reader::<ClientMessage>(receive);
    let result = loop {
        tokio::select! {
            message = incoming.recv() => match message {
                Some(Ok(ClientMessage::Input(input))) => {
                    let mut state = shared.state();
                    if state.controller == Some(id) {
                        state.inputs.push(*input);
                    }
                }
                Some(Ok(ClientMessage::TakeControl)) => take_control(shared, id),
                Some(Ok(_)) => break Err(RemoteError::Protocol("unexpected client message")),
                Some(Err(error)) => break Err(error),
                None => break Ok(()),
            },
            message = outgoing.recv() => match message {
                Some(message @ ServerMessage::Rejected(_)) => {
                    break write_control(&mut send, &message).await;
                }
                Some(message) => {
                    if let Err(error) = write_control(&mut send, &message).await {
                        break Err(error);
                    }
                }
                None => break Ok(()),
            },
        }
    };
    writer.abort();
    reader.abort();
    result
}

fn take_control(shared: &Shared, id: u64) {
    let mut state = shared.state();
    if state.controller == Some(id) {
        return;
    }
    if let Some(previous) = state.controller.replace(id)
        && let Some(client) = state.clients.get_mut(&previous)
    {
        client.info.role = Role::Viewer;
        let _ = client
            .control
            .send(ServerMessage::RoleChanged(Role::Viewer));
    }
    if let Some(client) = state.clients.get_mut(&id) {
        client.info.role = Role::Controller;
        let _ = client
            .control
            .send(ServerMessage::RoleChanged(Role::Controller));
    }
    state.inputs.clear();
}
