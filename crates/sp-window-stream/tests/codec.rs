//! Wire round trips, malformed-input rejection and client framebuffer decoding.
#![allow(clippy::cast_possible_truncation)]

use std::sync::Arc;

use sp_window_stream::{
    ApplyError, ClientId, ClientMessage, ControlReason, ErrorCode, FrameUpdate, InputEvent,
    MAX_MESSAGE_BYTES, PROTOCOL_VERSION, PointerButton, Rect, Role, ServerMessage, Tile,
    TileEncoder, TileEncoding, WindowId, WindowInfo, WindowKind, WindowStreamClient, WireError,
    WireMessage, tile_grid,
};

const W: WindowId = WindowId(0x0040_0001);

fn info(width: u16, height: u16) -> WindowInfo {
    WindowInfo {
        id: W,
        kind: WindowKind::Normal,
        title: "Pro-Q 4 — rack 2".into(),
        x: -20,
        y: 40,
        width,
        height,
        rack: Some(2),
        slot: Some(5),
        pid: Some(4242),
        transient_for: None,
    }
}

/// A BGRX test image whose bytes depend on position.
fn bgrx(width: u16, height: u16) -> Vec<u8> {
    let mut image = Vec::new();
    for y in 0..height {
        for x in 0..width {
            image.extend_from_slice(&[(x % 251) as u8, (y % 241) as u8, ((x ^ y) % 256) as u8, 0]);
        }
    }
    image
}

fn keyframe(width: u16, height: u16, image: &[u8]) -> FrameUpdate {
    let mut encoder = TileEncoder::new().unwrap();
    let tiles = tile_grid(width, height)
        .map(|rect| encoder.encode(image, usize::from(width) * 4, rect).unwrap())
        .collect();
    FrameUpdate {
        window: W,
        sequence: 1,
        width,
        height,
        keyframe: true,
        complete: true,
        tiles,
    }
}

fn client_samples() -> Vec<ClientMessage> {
    vec![
        ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            role: Role::Controller,
            name: "egui local".into(),
        },
        ClientMessage::RequestControl { takeover: true },
        ClientMessage::ReleaseControl,
        ClientMessage::RequestKeyframe { window: None },
        ClientMessage::RequestKeyframe { window: Some(W) },
        ClientMessage::Input(InputEvent::PointerMove {
            window: W,
            x: -3,
            y: 700,
        }),
        ClientMessage::Input(InputEvent::PointerButton {
            window: W,
            x: 1,
            y: 2,
            button: PointerButton::Forward,
            pressed: true,
        }),
        ClientMessage::Input(InputEvent::Scroll {
            window: W,
            x: 5,
            y: 6,
            dx: -1,
            dy: 3,
        }),
        ClientMessage::Input(InputEvent::Key {
            window: W,
            keysym: 0xffe1,
            pressed: false,
        }),
        ClientMessage::Input(InputEvent::Text {
            window: W,
            text: "héllo".into(),
        }),
        ClientMessage::Input(InputEvent::Focus { window: W }),
        ClientMessage::Input(InputEvent::Close { window: W }),
    ]
}

fn server_samples() -> Vec<ServerMessage> {
    let popup = WindowInfo {
        kind: WindowKind::Popup,
        transient_for: Some(W),
        rack: None,
        ..info(30, 20)
    };
    vec![
        ServerMessage::Welcome {
            version: PROTOCOL_VERSION,
            client: ClientId(7),
        },
        ServerMessage::WindowList(vec![info(800, 600), popup.clone()]),
        ServerMessage::WindowOpened(popup),
        ServerMessage::WindowChanged(info(640, 480)),
        ServerMessage::WindowClosed(W),
        ServerMessage::Frame(keyframe(130, 70, &bgrx(130, 70))),
        ServerMessage::ControlStatus {
            holder: Some(ClientId(3)),
            reason: ControlReason::TakenOver,
        },
        ServerMessage::ControlStatus {
            holder: None,
            reason: ControlReason::Released,
        },
        ServerMessage::Error {
            code: ErrorCode::NotController,
            message: "viewer input".into(),
        },
    ]
}

fn body(encoded: &[u8]) -> &[u8] {
    &encoded[4..]
}

#[test]
fn every_message_round_trips() {
    for message in client_samples() {
        let encoded = message.encode().unwrap();
        assert_eq!(
            u32::from_le_bytes(encoded[..4].try_into().unwrap()) as usize,
            encoded.len() - 4
        );
        assert_eq!(ClientMessage::decode(body(&encoded)).unwrap(), message);
        assert_eq!(
            ClientMessage::read_from(&mut &encoded[..], &mut Vec::new()).unwrap(),
            message
        );
    }
    for message in server_samples() {
        let encoded = message.encode().unwrap();
        assert_eq!(ServerMessage::decode(body(&encoded)).unwrap(), message);
    }
}

#[test]
fn truncated_and_padded_bodies_are_rejected() {
    let encoded: Vec<Vec<u8>> = server_samples()
        .iter()
        .map(|m| m.encode().unwrap())
        .collect();
    for bytes in &encoded {
        let body = body(bytes);
        for len in 0..body.len() {
            assert!(
                ServerMessage::decode(&body[..len]).is_err(),
                "prefix of {len} bytes decoded"
            );
        }
        let mut padded = body.to_vec();
        padded.push(0);
        assert!(matches!(
            ServerMessage::decode(&padded),
            Err(WireError::TrailingBytes(1))
        ));
    }
    for message in client_samples() {
        let bytes = message.encode().unwrap();
        for len in 0..bytes.len() - 4 {
            assert!(ClientMessage::decode(&body(&bytes)[..len]).is_err());
        }
    }
}

/// Random byte mutations must produce errors or valid messages, never panics or huge allocations.
#[test]
fn mutated_bodies_never_panic() {
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let server: Vec<Vec<u8>> = server_samples()
        .iter()
        .map(|m| body(&m.encode().unwrap()).to_vec())
        .collect();
    let client: Vec<Vec<u8>> = client_samples()
        .iter()
        .map(|m| body(&m.encode().unwrap()).to_vec())
        .collect();
    for round in 0..20_000 {
        let pool = if round % 2 == 0 { &server } else { &client };
        let mut bytes = pool[next() as usize % pool.len()].clone();
        for _ in 0..=next() % 4 {
            let at = next() as usize % bytes.len();
            bytes[at] = next() as u8;
        }
        if let Ok(ServerMessage::Frame(frame)) = ServerMessage::decode(&bytes) {
            // A frame that still decodes must still apply or fail cleanly.
            let mut client = WindowStreamClient::new().unwrap();
            client
                .apply(&ServerMessage::WindowOpened(WindowInfo {
                    id: frame.window,
                    ..info(1, 1)
                }))
                .unwrap();
            let _ = client.apply(&ServerMessage::Frame(frame));
        }
        let _ = ClientMessage::decode(&bytes);
    }
}

#[test]
fn hostile_fields_are_rejected() {
    let mut hello = ClientMessage::Hello {
        version: 1,
        role: Role::Viewer,
        name: String::new(),
    }
    .encode()
    .unwrap();
    hello[5] ^= 1;
    assert!(matches!(
        ClientMessage::decode(body(&hello)),
        Err(WireError::BadMagic)
    ));

    let oversized = (MAX_MESSAGE_BYTES as u32 + 1).to_le_bytes();
    assert!(matches!(
        ServerMessage::read_from(&mut &oversized[..], &mut Vec::new()),
        Err(WireError::TooLarge { .. })
    ));
    assert!(matches!(
        ServerMessage::read_from(&mut &[][..], &mut Vec::new()),
        Err(WireError::Closed)
    ));

    // A window list that claims a million entries in a few bytes fails before allocating.
    let mut list = vec![2];
    list.extend_from_slice(&1_000_000_u32.to_le_bytes());
    assert!(matches!(
        ServerMessage::decode(&list),
        Err(WireError::TooLarge { .. })
    ));
    let mut list = vec![2];
    list.extend_from_slice(&1000_u32.to_le_bytes());
    assert!(matches!(
        ServerMessage::decode(&list),
        Err(WireError::Truncated)
    ));

    let tile = |rect| Tile {
        rect,
        encoding: TileEncoding::ZstdBgrx,
        data: Arc::from(&[0_u8][..]),
    };
    let outside = FrameUpdate {
        window: W,
        sequence: 0,
        width: 64,
        height: 64,
        keyframe: false,
        complete: true,
        tiles: vec![tile(Rect {
            x: 32,
            y: 0,
            width: 64,
            height: 64,
        })],
    };
    assert!(matches!(
        ServerMessage::Frame(outside).encode(),
        Err(WireError::Invalid("tile rectangle"))
    ));
    let key = ClientMessage::Input(InputEvent::Key {
        window: W,
        keysym: 0,
        pressed: true,
    });
    assert!(key.encode().is_err());
    let long = ClientMessage::Input(InputEvent::Text {
        window: W,
        text: "x".repeat(257),
    });
    assert!(matches!(long.encode(), Err(WireError::TooLarge { .. })));
}

#[test]
fn client_decodes_keyframes_and_partial_updates() {
    let (width, height) = (130, 70);
    let mut image = bgrx(width, height);
    let mut client = WindowStreamClient::new().unwrap();
    client
        .apply(&ServerMessage::WindowOpened(info(width, height)))
        .unwrap();
    client
        .apply(&ServerMessage::Frame(keyframe(width, height, &image)))
        .unwrap();

    let expected = |image: &[u8]| {
        image
            .chunks_exact(4)
            .flat_map(|p| [p[2], p[1], p[0], 255])
            .collect::<Vec<_>>()
    };
    let window = client.window_mut(W).unwrap();
    assert_eq!(window.rgba(), expected(&image));
    assert_eq!(
        window.take_dirty(),
        vec![Rect {
            x: 0,
            y: 0,
            width,
            height
        }]
    );

    // One interior tile changes; only it is sent and only it is dirty.
    let rect = Rect {
        x: 64,
        y: 0,
        width: 64,
        height: 64,
    };
    for y in 0..64 {
        let at = (y * usize::from(width) + 64) * 4;
        image[at..at + 64 * 4].fill(0xab);
    }
    let tile = TileEncoder::new()
        .unwrap()
        .encode(&image, usize::from(width) * 4, rect)
        .unwrap();
    let update = FrameUpdate {
        sequence: 2,
        keyframe: false,
        tiles: vec![tile],
        ..keyframe(width, height, &image)
    };
    client.apply(&ServerMessage::Frame(update)).unwrap();
    let window = client.window_mut(W).unwrap();
    assert_eq!(window.rgba(), expected(&image));
    assert_eq!(window.take_dirty(), vec![rect]);
    assert_eq!(
        window.copy_rect(rect),
        [0xab, 0xab, 0xab, 255].repeat(64 * 64)
    );

    let corrupt = Tile {
        rect,
        encoding: TileEncoding::ZstdBgrx,
        data: Arc::from(&b"not zstd"[..]),
    };
    let bad = FrameUpdate {
        tiles: vec![corrupt],
        ..keyframe(width, height, &image)
    };
    assert!(matches!(
        client.apply(&ServerMessage::Frame(bad)),
        Err(ApplyError::CorruptTile(_))
    ));
}
