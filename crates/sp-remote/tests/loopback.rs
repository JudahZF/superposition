//! A real QUIC connection on loopback: pairing, frames, input, pinning, and control takeover.

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use sp_remote::{
    ClientConfig, ClientStatus, RemoteClient, RemoteServer, RemoteView, Role, ServerConfig,
};

fn directory(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("sp-remote-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    path
}

/// The engine's UI: one button that counts presses.
struct Engine {
    server: RemoteServer,
    ctx: egui::Context,
    presses: u32,
    button: egui::Rect,
}

impl Engine {
    fn frame(&mut self) {
        let input = self.server.take_input().unwrap_or_else(|| egui::RawInput {
            screen_rect: Some(screen()),
            ..Default::default()
        });
        let presses = &mut self.presses;
        let button = &mut self.button;
        let output = self.ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let response = ui.button(format!("Pressed {presses}"));
                *button = response.rect;
                if response.clicked() {
                    *presses += 1;
                }
            });
        });
        self.server.publish(&self.ctx, &output);
    }
}

fn client(state: &Path, engine: &Engine, role: Role, code: Option<String>) -> RemoteClient {
    RemoteClient::connect(ClientConfig {
        server: engine.server.local_addr(),
        state_dir: state.to_path_buf(),
        name: "Test client".into(),
        role,
        pairing_code: code,
    })
    .unwrap()
}

/// Runs engine frames until `done` holds.
fn until(engine: &mut Engine, step: &str, mut done: impl FnMut(&mut Engine) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done(engine) {
        assert!(Instant::now() < deadline, "timed out: {step}");
        engine.frame();
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn connected(client: &RemoteClient) -> Option<Role> {
    match client.status() {
        ClientStatus::Connected { role, .. } => Some(role),
        ClientStatus::Connecting => None,
        ClientStatus::Closed(reason) => panic!("connection closed: {reason}"),
    }
}

fn closed(client: &RemoteClient) -> Option<String> {
    match client.status() {
        ClientStatus::Closed(reason) => Some(reason),
        _ => None,
    }
}

fn screen() -> egui::Rect {
    egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 300.0))
}

fn click(at: egui::Pos2) -> egui::RawInput {
    let press = |pressed| egui::Event::PointerButton {
        pos: at,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    };
    egui::RawInput {
        screen_rect: Some(screen()),
        events: vec![egui::Event::PointerMoved(at), press(true), press(false)],
        ..Default::default()
    }
}

#[test]
fn pairing_frames_input_pins_and_takeover() {
    let engine_state = directory("engine");
    let laptop_state = directory("laptop");
    let tablet_state = directory("tablet");
    let mut engine = Engine {
        server: RemoteServer::start(ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            state_dir: engine_state.clone(),
            name: "Test engine".into(),
        })
        .unwrap(),
        ctx: egui::Context::default(),
        presses: 0,
        button: egui::Rect::NOTHING,
    };
    engine.frame();

    // An unpaired client without a code is refused before any connection.
    let unpaired = client(&tablet_state, &engine, Role::Viewer, None);
    until(&mut engine, "unpaired refused", |_| {
        closed(&unpaired).is_some()
    });

    // A wrong code fails pairing.
    let code = engine.server.open_pairing(Duration::from_mins(1));
    let wrong = if code == "000000" { "111111" } else { "000000" };
    let rejected = client(&laptop_state, &engine, Role::Controller, Some(wrong.into()));
    until(&mut engine, "wrong code refused", |_| {
        closed(&rejected).is_some()
    });
    assert!(engine.server.clients().is_empty());

    // The right code pairs; the controller receives frames and its click reaches the engine.
    let laptop = client(&laptop_state, &engine, Role::Controller, Some(code));
    until(&mut engine, "paired", |_| connected(&laptop).is_some());
    assert_eq!(connected(&laptop), Some(Role::Controller));
    assert_eq!(
        engine.server.pairing_code(),
        None,
        "a code pairs one client"
    );
    // The viewer paints the engine's meshes with the engine's textures in a local context.
    let mut view = RemoteView::new();
    let local = egui::Context::default();
    until(&mut engine, "viewer paints", |engine| {
        let input = egui::RawInput {
            screen_rect: Some(screen()),
            ..Default::default()
        };
        let output = local.run(input, |ctx| view.show(ctx, &laptop));
        !output.shapes.is_empty() && engine.button.is_positive()
    });
    laptop.send_input(click(engine.button.center()));
    until(&mut engine, "click reaches engine", |engine| {
        engine.presses == 1
    });
    drop(laptop);

    // The pinned client reconnects without a code; a second client pairs and takes control.
    let laptop = client(&laptop_state, &engine, Role::Controller, None);
    until(&mut engine, "pinned reconnect", |_| {
        connected(&laptop) == Some(Role::Controller)
    });
    let code = engine.server.open_pairing(Duration::from_mins(1));
    let tablet = client(&tablet_state, &engine, Role::Controller, Some(code));
    until(&mut engine, "second client pairs", |_| {
        connected(&tablet).is_some()
    });
    assert_eq!(
        connected(&tablet),
        Some(Role::Viewer),
        "one controller at a time"
    );
    tablet.send_input(click(engine.button.center()));
    tablet.take_control();
    until(&mut engine, "takeover", |_| {
        connected(&tablet) == Some(Role::Controller) && connected(&laptop) == Some(Role::Viewer)
    });
    assert_eq!(engine.presses, 1, "a viewer's input is ignored");
    assert_eq!(engine.server.clients().len(), 2);

    // A client that lost its pins must see the engine prove the code again, even though the
    // engine still trusts it.
    drop(tablet);
    std::fs::remove_file(tablet_state.join("trusted.json")).unwrap();
    let no_code = client(&tablet_state, &engine, Role::Viewer, None);
    until(&mut engine, "lost pin needs a code", |_| {
        closed(&no_code).is_some()
    });
    let code = engine.server.open_pairing(Duration::from_mins(1));
    let tablet = client(&tablet_state, &engine, Role::Viewer, Some(code));
    until(&mut engine, "re-paired", |_| connected(&tablet).is_some());
    assert_eq!(
        engine.server.pairing_code(),
        None,
        "the re-pairing used the code"
    );

    for path in [engine_state, laptop_state, tablet_state] {
        std::fs::remove_dir_all(path).unwrap();
    }
}
