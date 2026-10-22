//! The single-client source that macOS workers serve: session, keyframes, deltas, and control.
#![allow(clippy::cast_possible_truncation)]

use sp_window_stream::{
    ClientMessage, ControlReason, ErrorCode, InputEvent, PROTOCOL_VERSION, Rect, Role,
    SOURCE_CLIENT, ServerMessage, SourceAction, WindowId, WindowInfo, WindowKind, WindowSource,
    WindowStreamClient, WireMessage,
};

const W: WindowId = WindowId(731);

fn info(width: u16, height: u16) -> WindowInfo {
    WindowInfo {
        id: W,
        kind: WindowKind::Normal,
        title: "Pro-Q 4 – Rack 1, Slot 2".into(),
        x: 0,
        y: 0,
        width,
        height,
        rack: Some(0),
        slot: Some(1),
        pid: Some(4242),
        transient_for: None,
    }
}

fn hello(role: Role) -> ClientMessage {
    ClientMessage::Hello {
        version: PROTOCOL_VERSION,
        role,
        name: "Superposition".into(),
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

/// Sends `out` over the wire format to `client`, returning the frames' tile rectangles.
fn deliver(client: &mut WindowStreamClient, out: &mut Vec<ServerMessage>) -> Vec<Rect> {
    let mut tiles = Vec::new();
    for message in out.drain(..) {
        let bytes = message.encode().unwrap();
        let decoded = ServerMessage::decode(&bytes[4..]).unwrap();
        if let ServerMessage::Frame(frame) = &decoded {
            tiles.extend(frame.tiles.iter().map(|tile| tile.rect));
        }
        client.apply(&decoded).unwrap();
    }
    tiles
}

fn expected_rgba(image: &[u8]) -> Vec<u8> {
    image
        .chunks_exact(4)
        .flat_map(|bgrx| [bgrx[2], bgrx[1], bgrx[0], 255])
        .collect()
}

#[test]
fn a_new_client_gets_a_keyframe_then_only_changed_tiles() {
    let (width, height) = (130, 70);
    let mut source = WindowSource::new().unwrap();
    let mut client = WindowStreamClient::new().unwrap();
    let mut out = Vec::new();

    // Without a client nothing is queued and frames are dropped.
    source.set_windows([info(width, height)], &mut out);
    source
        .update_frame(W, width, height, bgrx(width, height), &mut out)
        .unwrap();
    assert!(out.is_empty());

    assert_eq!(
        source.receive(hello(Role::Controller), &mut out),
        SourceAction::None
    );
    deliver(&mut client, &mut out);
    assert_eq!(client.client_id(), Some(SOURCE_CLIENT));
    assert!(client.has_control());
    assert!(client.window(W).is_some());

    let mut image = bgrx(width, height);
    source
        .update_frame(W, width, height, image.clone(), &mut out)
        .unwrap();
    assert_eq!(deliver(&mut client, &mut out).len(), 3 * 2);
    assert_eq!(client.window(W).unwrap().rgba(), expected_rgba(&image));

    // One pixel in the bottom-middle tile changes; only that tile travels.
    let offset = (66 * usize::from(width) + 70) * 4;
    image[offset] ^= 0xff;
    source
        .update_frame(W, width, height, image.clone(), &mut out)
        .unwrap();
    assert_eq!(
        deliver(&mut client, &mut out),
        [Rect {
            x: 64,
            y: 64,
            width: 64,
            height: 6
        }]
    );
    assert_eq!(client.window(W).unwrap().rgba(), expected_rgba(&image));

    // An unchanged frame sends nothing; a keyframe request sends every tile again.
    source
        .update_frame(W, width, height, image.clone(), &mut out)
        .unwrap();
    assert!(out.is_empty());
    source.receive(ClientMessage::RequestKeyframe { window: Some(W) }, &mut out);
    source
        .update_frame(W, width, height, image, &mut out)
        .unwrap();
    assert_eq!(deliver(&mut client, &mut out).len(), 3 * 2);

    source.set_windows([], &mut out);
    deliver(&mut client, &mut out);
    assert!(client.window(W).is_none());
}

#[test]
fn input_passes_only_from_the_controller_to_streamed_windows() {
    let mut source = WindowSource::new().unwrap();
    let mut out = Vec::new();
    let focus = |window| ClientMessage::Input(InputEvent::Focus { window });
    let error_code = |out: &mut Vec<ServerMessage>| match out.drain(..).next_back() {
        Some(ServerMessage::Error { code, .. }) => Some(code),
        _ => None,
    };

    assert_eq!(source.receive(focus(W), &mut out), SourceAction::Close);
    assert_eq!(error_code(&mut out), Some(ErrorCode::Malformed));

    source.set_windows([info(40, 30)], &mut out);
    assert_eq!(
        source.receive(hello(Role::Viewer), &mut out),
        SourceAction::None
    );
    assert!(matches!(
        out.last(),
        Some(ServerMessage::ControlStatus {
            holder: None,
            reason: ControlReason::Current
        })
    ));
    out.clear();
    assert_eq!(source.receive(focus(W), &mut out), SourceAction::None);
    assert_eq!(error_code(&mut out), Some(ErrorCode::NotController));

    source.receive(ClientMessage::RequestControl { takeover: false }, &mut out);
    out.clear();
    assert_eq!(
        source.receive(focus(WindowId(9)), &mut out),
        SourceAction::None
    );
    assert_eq!(error_code(&mut out), Some(ErrorCode::UnknownWindow));
    assert_eq!(
        source.receive(focus(W), &mut out),
        SourceAction::Input(InputEvent::Focus { window: W })
    );
    assert!(out.is_empty());

    source.disconnect();
    let stale = ClientMessage::Hello {
        version: PROTOCOL_VERSION + 1,
        role: Role::Controller,
        name: String::new(),
    };
    assert_eq!(source.receive(stale, &mut out), SourceAction::Close);
    assert_eq!(error_code(&mut out), Some(ErrorCode::UnsupportedVersion));
}
