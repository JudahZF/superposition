//! Remote operation: a Superposition engine serves its UI to egui clients over QUIC and TLS 1.3.
//!
//! The engine runs its normal UI against a headless `egui::Context`. A client sends each frame's
//! `egui::RawInput` and paints the tessellated meshes and texture changes it receives
//! ([`RemoteView`]), so the client shows exactly the engine's UI, including streamed plug-in
//! editor windows, which are textures in that UI. One client controls; others watch, and a
//! viewer may take control.
//!
//! Each installation has a self-signed identity ([`Identity`]). An unknown client must pair once
//! with the six-digit code the engine shows: SPAKE2 over the connection, confirmed with HMACs
//! over both certificate fingerprints, after which each side pins the other ([`TrustStore`]).

mod client;
mod identity;
mod protocol;
mod server;
mod textures;
mod tls;
mod viewer;

pub use client::{ClientConfig, ClientStatus, RemoteClient};
pub use identity::{Fingerprint, Identity, PeerRecord, TrustStore};
pub use protocol::{Frame, PROTOCOL_VERSION, Role, WireMesh};
pub use server::{ClientInfo, RemoteServer, ServerConfig};
pub use viewer::RemoteView;

/// The default UDP port.
pub const DEFAULT_PORT: u16 = 4433;

/// Why a remote operation failed.
#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    /// Local I/O failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// TLS configuration failed.
    #[error("TLS: {0}")]
    Tls(String),
    /// The QUIC connection failed.
    #[error("connection: {0}")]
    Connection(#[from] quinn::ConnectionError),
    /// Starting a connection failed.
    #[error("connect: {0}")]
    Connect(#[from] quinn::ConnectError),
    /// Writing to a stream failed.
    #[error("send: {0}")]
    Write(#[from] quinn::WriteError),
    /// Reading from a stream failed.
    #[error("receive: {0}")]
    Read(#[from] quinn::ReadExactError),
    /// A message could not be encoded or decoded.
    #[error("message encoding: {0}")]
    Encoding(#[from] postcard::Error),
    /// A message exceeded its size limit.
    #[error("a message exceeded its size limit")]
    TooLarge,
    /// The peer broke the protocol.
    #[error("protocol: {0}")]
    Protocol(&'static str),
    /// The peer presented no certificate.
    #[error("the peer presented no certificate")]
    NoPeerIdentity,
    /// The client has not paired with this engine.
    #[error("not paired with this engine; enter the code the engine shows")]
    NotPaired,
    /// The engine did not prove knowledge of the pairing code.
    #[error("pairing failed: the engine did not confirm the code")]
    PairingFailed,
    /// An engine this client has no pin for accepted it without pairing.
    #[error("the engine accepted this client without proving the pairing code; refused")]
    PairingSkipped,
}

impl From<rustls::Error> for RemoteError {
    fn from(error: rustls::Error) -> Self {
        Self::Tls(error.to_string())
    }
}
