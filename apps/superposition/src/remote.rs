//! Remote operation: `--serve` runs the engine and its UI without a window for remote clients;
//! `--connect` opens a window that shows and drives a served engine.
//!
//! ```text
//! superposition --serve [--bind ADDR] [--name NAME] [--pair]
//! superposition --connect HOST[:PORT] [--pair CODE] [--viewer]
//! ```
//!
//! The served UI is the normal app UI, so streamed plug-in editors appear on the client too. An
//! engine opens pairing on its first start, or with `--pair`, and prints the six-digit code.

use std::{
    net::{SocketAddr, ToSocketAddrs},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use eframe::egui;
use sp_remote::{
    ClientConfig, ClientStatus, DEFAULT_PORT, RemoteClient, RemoteServer, RemoteView, Role,
    ServerConfig,
};
use sp_session::SessionController;

use crate::{ProductRuntime, default_application_support, default_session_root, ui};

const PAIRING_LIFETIME: Duration = Duration::from_mins(10);
/// The shortest frame interval, and the longest (the engine keeps polling workers and audio).
const ACTIVE_FRAME: Duration = Duration::from_millis(16);
const IDLE_FRAME: Duration = Duration::from_millis(250);
/// How often an idle server checks for client input.
const INPUT_POLL: Duration = Duration::from_millis(4);
/// The served window size before a controller connects.
const DEFAULT_SCREEN: egui::Vec2 = egui::vec2(1180.0, 720.0);

/// Whether these arguments select a remote mode.
pub(crate) fn is_remote(arguments: &[String]) -> bool {
    matches!(
        arguments.first().map(String::as_str),
        Some("--serve" | "--connect")
    )
}

/// Runs `--serve` or `--connect`.
pub(crate) fn run(arguments: &[String]) -> Result<(), String> {
    match arguments.first().map(String::as_str) {
        Some("--serve") => serve(&arguments[1..]),
        Some("--connect") => connect(&arguments[1..]),
        _ => Err("expected --serve or --connect".to_owned()),
    }
}

fn host_name() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "Superposition".to_owned())
}

fn remote_state(directory: &str) -> PathBuf {
    default_application_support().join(directory)
}

fn serve(arguments: &[String]) -> Result<(), String> {
    let mut bind: Option<SocketAddr> = None;
    let mut name = host_name();
    let mut pair = false;
    let mut arguments = arguments.iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--bind" => {
                bind = Some(
                    arguments
                        .next()
                        .ok_or("--bind needs an address")?
                        .parse()
                        .map_err(|error| format!("invalid --bind address: {error}"))?,
                );
            }
            "--name" => name.clone_from(arguments.next().ok_or("--name needs a value")?),
            "--pair" => pair = true,
            other => return Err(format!("unknown --serve option `{other}`")),
        }
    }

    let controller = SessionController::open(default_session_root())
        .map_err(|error| format!("cannot open the session: {error}"))?;
    let recovery_offered = controller.recovery_offered();
    let runtime = ProductRuntime::open(default_application_support());
    let ctx = egui::Context::default();
    ui::install_style(&ctx);
    let mut app = ui::LiveRackApp::new(controller, recovery_offered, runtime);

    let start = |bind| {
        RemoteServer::start(ServerConfig {
            bind,
            state_dir: remote_state("remote"),
            name: name.clone(),
        })
    };
    // By default listen dual-stack, or on IPv4 alone where IPv6 is disabled.
    let server = match bind {
        Some(bind) => start(bind),
        None => start(([0_u16; 8], DEFAULT_PORT).into())
            .or_else(|_| start(([0, 0, 0, 0], DEFAULT_PORT).into())),
    }
    .map_err(|error| format!("cannot start the remote server: {error}"))?;
    println!(
        "serving on {} (fingerprint {})",
        server.local_addr(),
        server.fingerprint()
    );
    if pair || server.paired_clients() == 0 {
        let code = server.open_pairing(PAIRING_LIFETIME);
        println!("pairing code: {code} (valid for 10 minutes)");
    }

    let stop = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&stop))
            .map_err(|error| format!("cannot handle signals: {error}"))?;
    }
    let started = Instant::now();
    let mut screen = egui::Rect::from_min_size(egui::Pos2::ZERO, DEFAULT_SCREEN);
    let mut viewports = egui::ViewportIdMap::default();
    let mut next_frame = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let input = server.take_input();
        // Render only for input or when the UI asked to repaint (meters, timers, polling).
        if input.is_none() && Instant::now() < next_frame {
            std::thread::sleep(INPUT_POLL);
            continue;
        }
        let mut input = input.unwrap_or_default();
        // Between controller inputs, keep laying out for the controller's last window.
        match input.screen_rect {
            Some(rect) => screen = rect,
            None => input.screen_rect = Some(screen),
        }
        if input.viewports.is_empty() {
            input.viewports.clone_from(&viewports);
        } else {
            viewports.clone_from(&input.viewports);
        }
        input.time = Some(started.elapsed().as_secs_f64());
        let output = ctx.run(input, |ctx| app.frame(ctx));
        server.publish(&ctx, &output);
        let repaint = output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map_or(IDLE_FRAME, |viewport| viewport.repaint_delay);
        next_frame = Instant::now() + repaint.clamp(ACTIVE_FRAME, IDLE_FRAME);
    }
    eframe::App::on_exit(&mut app, None);
    Ok(())
}

fn connect(arguments: &[String]) -> Result<(), String> {
    let mut arguments = arguments.iter();
    let target = arguments
        .next()
        .ok_or("--connect needs HOST or HOST:PORT")?;
    let server = resolve(target)?;
    let mut pairing_code = None;
    let mut role = Role::Controller;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--pair" => {
                pairing_code = Some(arguments.next().ok_or("--pair needs the code")?.clone());
            }
            "--viewer" => role = Role::Viewer,
            other => return Err(format!("unknown --connect option `{other}`")),
        }
    }
    let client = RemoteClient::connect(ClientConfig {
        server,
        state_dir: remote_state("remote-client"),
        name: host_name(),
        role,
        pairing_code,
    })
    .map_err(|error| format!("cannot start the remote client: {error}"))?;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(DEFAULT_SCREEN)
            .with_min_inner_size([960.0, 640.0])
            .with_title(format!("Superposition — {target}")),
        ..Default::default()
    };
    eframe::run_native(
        "Superposition remote",
        options,
        Box::new(move |cc| {
            ui::install_style(&cc.egui_ctx);
            client.repaint_on_receive(&cc.egui_ctx);
            Ok(Box::new(RemoteApp {
                client,
                view: RemoteView::new(),
            }))
        }),
    )
    .map_err(|error| error.to_string())
}

fn resolve(target: &str) -> Result<SocketAddr, String> {
    let with_port = if target.contains(':') && !target.ends_with(']') {
        target.to_owned()
    } else {
        format!("{target}:{DEFAULT_PORT}")
    };
    with_port
        .to_socket_addrs()
        .map_err(|error| format!("cannot resolve {target}: {error}"))?
        .next()
        .ok_or_else(|| format!("{target} has no address"))
}

/// The client window: the engine's UI, plus connection state and a takeover button.
struct RemoteApp {
    client: RemoteClient,
    view: RemoteView,
}

impl eframe::App for RemoteApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        match self.client.status() {
            ClientStatus::Connected { role, .. } => {
                self.view.show(ctx, &self.client);
                if role == Role::Viewer {
                    egui::Area::new(egui::Id::new("remote-viewer"))
                        .anchor(egui::Align2::RIGHT_TOP, [-12.0, 12.0])
                        .order(egui::Order::Foreground)
                        .show(ctx, |ui| {
                            ui.horizontal(|ui| {
                                ui.label("Watching");
                                if ui.button("Take control").clicked() {
                                    self.client.take_control();
                                }
                            });
                        });
                }
            }
            ClientStatus::Connecting => {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.centered_and_justified(|ui| ui.label("Connecting to the engine…"));
                });
            }
            ClientStatus::Closed(reason) => {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.centered_and_justified(|ui| ui.label(format!("Disconnected: {reason}")));
                });
            }
        }
    }
}
