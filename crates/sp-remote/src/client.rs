//! The client side: connects (pairing first if needed), sends input, and receives frames.

use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use egui::TexturesDelta;
use tokio::sync::mpsc;

use crate::{
    Identity, RemoteError, Role, TrustStore,
    protocol::{
        ClientMessage, Frame, PROTOCOL_VERSION, ServerMessage, pairing, read_control, read_frame,
        write_control,
    },
    server::spawn_reader,
    tls,
};

/// How to connect.
#[derive(Clone, Debug)]
pub struct ClientConfig {
    /// The engine's address.
    pub server: SocketAddr,
    /// Directory for this client's identity and pinned engines.
    pub state_dir: PathBuf,
    /// The name the engine shows for this client.
    pub name: String,
    /// The role to ask for. A second controller joins as a viewer.
    pub role: Role,
    /// The code the engine shows, for the first connection to it.
    pub pairing_code: Option<String>,
}

/// The client's connection state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientStatus {
    /// Connecting or pairing.
    Connecting,
    /// Accepted.
    Connected {
        /// The engine's name.
        server_name: String,
        /// The current role.
        role: Role,
    },
    /// The connection ended or was refused.
    Closed(String),
}

#[derive(Default)]
struct Received {
    textures: Vec<TexturesDelta>,
    frame: Option<Frame>,
}

struct Shared {
    status: Mutex<ClientStatus>,
    /// The live connection and its endpoint, closed gracefully on drop.
    connection: Mutex<Option<(quinn::Endpoint, quinn::Connection)>>,
    received: Mutex<Received>,
    repaint: Mutex<Option<egui::Context>>,
}

impl Shared {
    fn set_status(&self, status: ClientStatus) {
        *self.status.lock().unwrap_or_else(PoisonError::into_inner) = status;
        self.repaint();
    }

    fn repaint(&self) {
        if let Some(ctx) = self
            .repaint
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            ctx.request_repaint();
        }
    }

    fn received(&self) -> MutexGuard<'_, Received> {
        self.received.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A connection to an engine.
pub struct RemoteClient {
    shared: Arc<Shared>,
    outgoing: mpsc::UnboundedSender<ClientMessage>,
    runtime: tokio::runtime::Runtime,
}

impl RemoteClient {
    /// Starts connecting in the background. Watch [`Self::status`].
    ///
    /// # Errors
    /// Returns an error when the local identity or runtime cannot be set up.
    pub fn connect(config: ClientConfig) -> Result<Self, RemoteError> {
        let identity = Identity::load_or_create(&config.state_dir)?;
        let trust = TrustStore::load(&config.state_dir)?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("sp-remote-client")
            .enable_all()
            .build()?;
        let shared = Arc::new(Shared {
            status: Mutex::new(ClientStatus::Connecting),
            connection: Mutex::new(None),
            received: Mutex::new(Received::default()),
            repaint: Mutex::new(None),
        });
        let (outgoing, messages) = mpsc::unbounded_channel();
        let task_shared = Arc::clone(&shared);
        runtime.spawn(async move {
            let result = run(config, identity, trust, &task_shared, messages).await;
            let reason = match result {
                Ok(reason) => reason,
                Err(error) => error.to_string(),
            };
            task_shared.set_status(ClientStatus::Closed(reason));
        });
        Ok(Self {
            shared,
            outgoing,
            runtime,
        })
    }

    /// The connection state.
    #[must_use]
    pub fn status(&self) -> ClientStatus {
        self.shared
            .status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Repaints `ctx` whenever a frame or status arrives.
    pub fn repaint_on_receive(&self, ctx: &egui::Context) {
        *self
            .shared
            .repaint
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(ctx.clone());
    }

    /// Sends one frame of input. The engine ignores it unless this client controls it.
    pub fn send_input(&self, input: egui::RawInput) {
        let _ = self.outgoing.send(ClientMessage::Input(Box::new(input)));
    }

    /// Asks to take control from the current controller.
    pub fn take_control(&self) {
        let _ = self.outgoing.send(ClientMessage::TakeControl);
    }

    /// Texture changes and the newest frame received since the last call.
    pub(crate) fn take_received(&self) -> (Vec<TexturesDelta>, Option<Frame>) {
        let mut received = self.shared.received();
        (
            std::mem::take(&mut received.textures),
            received.frame.take(),
        )
    }
}

impl Drop for RemoteClient {
    fn drop(&mut self) {
        let connection = self
            .shared
            .connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some((endpoint, connection)) = connection {
            // Tell the engine now, rather than leaving it to time the connection out.
            connection.close(0_u32.into(), b"bye");
            self.runtime.block_on(async {
                let _ =
                    tokio::time::timeout(Duration::from_millis(250), endpoint.wait_idle()).await;
            });
        }
    }
}

async fn run(
    config: ClientConfig,
    identity: Identity,
    mut trust: TrustStore,
    shared: &Arc<Shared>,
    mut messages: mpsc::UnboundedReceiver<ClientMessage>,
) -> Result<String, RemoteError> {
    let address = config.server.to_string();
    let pinned = trust.server(&address).map(|peer| peer.fingerprint);
    let expected = if config.pairing_code.is_some() {
        None
    } else {
        Some(pinned.ok_or(RemoteError::NotPaired)?)
    };
    let bind: SocketAddr = if config.server.is_ipv4() {
        ([0, 0, 0, 0], 0).into()
    } else {
        ([0_u16; 8], 0).into()
    };
    let mut endpoint = quinn::Endpoint::client(bind)?;
    endpoint.set_default_client_config(tls::client_config(&identity, expected)?);
    let connection = endpoint.connect(config.server, "superposition")?.await?;
    let server = tls::peer_fingerprint(&connection).ok_or(RemoteError::NoPeerIdentity)?;
    *shared
        .connection
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some((endpoint.clone(), connection.clone()));
    let (mut send, mut receive) = connection.open_bi().await?;
    let hello = ClientMessage::Hello {
        version: PROTOCOL_VERSION,
        name: config.name,
        role: config.role,
    };
    write_control(&mut send, &hello).await?;

    let mut reply: Option<ServerMessage> = read_control(&mut receive).await?;
    if matches!(reply, Some(ServerMessage::PairingRequired)) {
        let Some(code) = config.pairing_code.as_deref() else {
            return Err(RemoteError::NotPaired);
        };
        let (exchange, spake) = pairing::start_client(code);
        write_control(&mut send, &ClientMessage::PairStart { spake }).await?;
        let Some(ServerMessage::PairReply { spake, mac }) = read_control(&mut receive).await?
        else {
            return Ok("the engine refused pairing".into());
        };
        let key = exchange
            .finish(&spake)
            .map_err(|_| RemoteError::Protocol("malformed pairing reply"))?;
        if !pairing::verify(&key, true, identity.fingerprint(), server, &mac) {
            return Err(RemoteError::PairingFailed);
        }
        let mac = pairing::confirmation(&key, false, identity.fingerprint(), server);
        write_control(&mut send, &ClientMessage::PairConfirm { mac }).await?;
        reply = read_control(&mut receive).await?;
        if let Some(ServerMessage::Welcome { server_name, .. }) = &reply {
            trust.pin_server(address, server_name.clone(), server)?;
        }
    }
    let Some(ServerMessage::Welcome { role, server_name }) = reply else {
        return Ok(match reply {
            Some(ServerMessage::Rejected(reason)) => reason,
            _ => "the engine ended the connection".into(),
        });
    };
    shared.set_status(ClientStatus::Connected {
        server_name: server_name.clone(),
        role,
    });

    let frames = connection.accept_uni().await?;
    let frame_reader = tokio::spawn(read_frames(frames, Arc::clone(shared)));
    let (mut incoming, control_reader) = spawn_reader::<ServerMessage>(receive);
    let result = loop {
        tokio::select! {
            message = incoming.recv() => match message {
                Some(Ok(ServerMessage::RoleChanged(role))) => shared.set_status(ClientStatus::Connected {
                    server_name: server_name.clone(),
                    role,
                }),
                Some(Ok(ServerMessage::Rejected(reason))) => break Ok(reason),
                Some(Ok(_)) => break Err(RemoteError::Protocol("unexpected engine message")),
                Some(Err(error)) => break Err(error),
                None => break Ok("the engine closed the connection".into()),
            },
            message = messages.recv() => match message {
                Some(message) => {
                    if let Err(error) = write_control(&mut send, &message).await {
                        break Err(error);
                    }
                }
                None => break Ok("disconnected".into()),
            },
        }
    };
    frame_reader.abort();
    control_reader.abort();
    result
}

/// Receives frames on their own task (stream reads are not cancel-safe).
async fn read_frames(mut frames: quinn::RecvStream, shared: Arc<Shared>) {
    while let Ok(Some(mut frame)) = read_frame(&mut frames).await {
        {
            let mut received = shared.received();
            received.textures.append(&mut frame.textures);
            received.frame = Some(frame);
        }
        shared.repaint();
    }
}
