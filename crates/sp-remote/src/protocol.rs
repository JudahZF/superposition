//! Messages on a connection and their framing.
//!
//! A connection has one bidirectional control stream that the client opens, and one
//! unidirectional frame stream that the server opens after the client is accepted. Each message
//! is a little-endian `u32` length followed by a postcard body; frame bodies are also
//! zstd-compressed. Every read is bounded.

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::RemoteError;

/// The protocol version in `Hello`.
pub const PROTOCOL_VERSION: u32 = 1;

const MAX_CONTROL_BYTES: u32 = 4 << 20;
const MAX_FRAME_BYTES: u32 = 64 << 20;

/// What a client may do.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Role {
    /// Sends input. One client at a time.
    Controller,
    /// Watches only.
    Viewer,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) enum ClientMessage {
    Hello {
        version: u32,
        name: String,
        role: Role,
        /// The client has no pin for this engine and requires it to prove the pairing code,
        /// even if the engine already trusts the client.
        pair: bool,
    },
    /// SPAKE2 side A, when the client has a pairing code.
    PairStart { spake: Vec<u8> },
    /// The client's key confirmation.
    PairConfirm { mac: Vec<u8> },
    /// One egui frame of input from the controller.
    Input(Box<egui::RawInput>),
    /// A viewer asks to take control.
    TakeControl,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) enum ServerMessage {
    /// The client is unknown: it must pair with the code the engine shows.
    PairingRequired,
    /// SPAKE2 side B and the server's key confirmation.
    PairReply { spake: Vec<u8>, mac: Vec<u8> },
    /// The client is accepted with this role.
    Welcome { role: Role, server_name: String },
    /// The client's role changed (for example after another client took control).
    RoleChanged(Role),
    /// The connection is refused.
    Rejected(String),
}

/// A mesh and its clip rectangle, in points.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WireMesh {
    /// The clip rectangle.
    pub clip_rect: egui::Rect,
    /// The tessellated mesh. Texture IDs are the engine's.
    pub mesh: egui::epaint::Mesh,
}

/// One rendered engine frame.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Frame {
    /// Increases with every published frame.
    pub sequence: u64,
    /// Texture changes since the client's previous frame, oldest first.
    pub textures: Vec<egui::TexturesDelta>,
    /// The frame's meshes, in paint order.
    pub meshes: Vec<WireMesh>,
    /// The pointer cursor the engine's UI asks for.
    pub cursor: egui::CursorIcon,
    /// Text the engine's UI copied to the clipboard in this frame.
    pub copied_text: String,
}

async fn write_body(send: &mut quinn::SendStream, body: &[u8]) -> Result<(), RemoteError> {
    let length = u32::try_from(body.len()).map_err(|_| RemoteError::TooLarge)?;
    send.write_all(&length.to_le_bytes()).await?;
    send.write_all(body).await?;
    Ok(())
}

async fn read_body(
    receive: &mut quinn::RecvStream,
    limit: u32,
) -> Result<Option<Vec<u8>>, RemoteError> {
    let mut length = [0; 4];
    match receive.read_exact(&mut length).await {
        Ok(()) => {}
        Err(quinn::ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let length = u32::from_le_bytes(length);
    if length > limit {
        return Err(RemoteError::TooLarge);
    }
    let mut body = vec![0; length as usize];
    receive.read_exact(&mut body).await?;
    Ok(Some(body))
}

pub(crate) async fn write_control<T: Serialize>(
    send: &mut quinn::SendStream,
    message: &T,
) -> Result<(), RemoteError> {
    write_body(send, &postcard::to_stdvec(message)?).await
}

/// Reads one control message; `None` at a clean end of stream.
pub(crate) async fn read_control<T: DeserializeOwned>(
    receive: &mut quinn::RecvStream,
) -> Result<Option<T>, RemoteError> {
    match read_body(receive, MAX_CONTROL_BYTES).await? {
        Some(body) => Ok(Some(postcard::from_bytes(&body)?)),
        None => Ok(None),
    }
}

pub(crate) async fn write_frame(
    send: &mut quinn::SendStream,
    frame: &Frame,
) -> Result<(), RemoteError> {
    let body = postcard::to_stdvec(frame)?;
    // Level 1: meshes and glyph atlases compress well even at the fastest level.
    write_body(send, &zstd::bulk::compress(&body, 1)?).await
}

pub(crate) async fn read_frame(
    receive: &mut quinn::RecvStream,
) -> Result<Option<Frame>, RemoteError> {
    let Some(body) = read_body(receive, MAX_FRAME_BYTES).await? else {
        return Ok(None);
    };
    let body = zstd::bulk::decompress(&body, MAX_FRAME_BYTES as usize * 4)?;
    Ok(Some(postcard::from_bytes(&body)?))
}

/// SPAKE2 pairing bound to both certificate fingerprints.
pub(crate) mod pairing {
    use spake2::{Ed25519Group, Identity, Password, Spake2};

    use crate::Fingerprint;

    const CLIENT: &[u8] = b"superposition-remote-client";
    const SERVER: &[u8] = b"superposition-remote-server";

    pub(crate) fn start_client(code: &str) -> (Spake2<Ed25519Group>, Vec<u8>) {
        Spake2::<Ed25519Group>::start_a(
            &Password::new(code.as_bytes()),
            &Identity::new(CLIENT),
            &Identity::new(SERVER),
        )
    }

    pub(crate) fn start_server(code: &str) -> (Spake2<Ed25519Group>, Vec<u8>) {
        Spake2::<Ed25519Group>::start_b(
            &Password::new(code.as_bytes()),
            &Identity::new(CLIENT),
            &Identity::new(SERVER),
        )
    }

    fn tagged_message(server_side: bool, client: Fingerprint, server: Fingerprint) -> Vec<u8> {
        let label = if server_side { SERVER } else { CLIENT };
        [label, client.as_bytes(), server.as_bytes()].concat()
    }

    /// The confirmation tag one side sends: HMAC over its label and both fingerprints.
    pub(crate) fn confirmation(
        key: &[u8],
        server_side: bool,
        client: Fingerprint,
        server: Fingerprint,
    ) -> Vec<u8> {
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key);
        ring::hmac::sign(&key, &tagged_message(server_side, client, server))
            .as_ref()
            .to_vec()
    }

    /// Checks a peer's confirmation tag in constant time.
    pub(crate) fn verify(
        key: &[u8],
        server_side: bool,
        client: Fingerprint,
        server: Fingerprint,
        tag: &[u8],
    ) -> bool {
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key);
        ring::hmac::verify(&key, &tagged_message(server_side, client, server), tag).is_ok()
    }

    /// A random six-digit code.
    pub(crate) fn new_code() -> String {
        let mut bytes = [0; 4];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes)
            .expect("the system random source works");
        format!("{:06}", u32::from_le_bytes(bytes) % 1_000_000)
    }

    #[cfg(test)]
    mod tests {
        use super::{confirmation, start_client, start_server, verify};
        use crate::Fingerprint;

        #[test]
        fn matching_codes_agree_and_a_wrong_code_fails_confirmation() {
            let client_fp = Fingerprint::of(b"client");
            let server_fp = Fingerprint::of(b"server");
            for (client_code, server_code, agree) in
                [("123456", "123456", true), ("123456", "654321", false)]
            {
                let (client, client_message) = start_client(client_code);
                let (server, server_message) = start_server(server_code);
                let client_key = client.finish(&server_message).unwrap();
                let server_key = server.finish(&client_message).unwrap();
                let tag = confirmation(&server_key, true, client_fp, server_fp);
                assert_eq!(verify(&client_key, true, client_fp, server_fp, &tag), agree);
                // The tag is bound to the fingerprints the client saw.
                assert!(!verify(&server_key, true, server_fp, client_fp, &tag));
            }
        }
    }
}
