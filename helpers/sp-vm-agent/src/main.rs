//! `sp-vm-agent [--worker EXE] [--scanner EXE] [--image-id FILE]`: the guest agent of
//! Lambda's Windows VM.
//!
//! Start it at logon in the auto-logon user's session. It opens the virtio-serial ports
//! `\\.\Global\lambda.agent` and `\\.\Global\lambda.worker.<port>`. The worker defaults to
//! `sp-plugin-worker.exe` next to the agent, the scanner to `sp-plugin-scanner.exe`, and the
//! image ID to `image-id.txt`. It scans `C:\Program Files\Common Files\VST3`.

use sp_vm_agent::{Agent, AgentConfig, PortIo};
use std::{io, path::PathBuf, process::ExitCode, thread, time::Duration};

fn main() -> ExitCode {
    let here = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(PathBuf::from))
        .unwrap_or_default();
    let mut worker = here.join("sp-plugin-worker.exe");
    let mut scanner = here.join("sp-plugin-scanner.exe");
    let mut image_id_file = here.join("image-id.txt");
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        match (flag.as_str(), args.next()) {
            ("--worker", Some(path)) => worker = path.into(),
            ("--scanner", Some(path)) => scanner = path.into(),
            ("--image-id", Some(path)) => image_id_file = path.into(),
            _ => {
                eprintln!("usage: sp-vm-agent [--worker EXE] [--scanner EXE] [--image-id FILE]");
                return ExitCode::from(2);
            }
        }
    }
    let image_id = std::fs::read_to_string(&image_id_file)
        .map_or_else(|_| "unknown".to_owned(), |id| id.trim().to_owned());
    // The host may not listen yet: open the agent port until it works.
    let agent = loop {
        let config = AgentConfig {
            worker: worker.clone(),
            scanner: scanner.clone(),
            vst3: PathBuf::from(r"C:\Program Files\Common Files\VST3"),
            reports: std::env::temp_dir().join("lambda-scan"),
            image_id: image_id.clone(),
            open_port: Box::new(open_port),
        };
        match Agent::new(config) {
            Ok(agent) => break agent,
            Err(error) => eprintln!("sp-vm-agent: agent port: {error}"),
        }
        thread::sleep(Duration::from_secs(1));
    };
    agent.run()
}

fn open_port(port: Option<u32>) -> io::Result<Box<dyn PortIo>> {
    let path = port.map_or_else(
        || r"\\.\Global\lambda.agent".to_owned(),
        |port| format!(r"\\.\Global\lambda.worker.{port}"),
    );
    open_path(&path)
}

/// A virtio-serial port must not serialize its reads and writes (`OverlappedPort`).
#[cfg(windows)]
fn open_path(path: &str) -> io::Result<Box<dyn PortIo>> {
    Ok(Box::new(sp_vm_agent::OverlappedPort::open(path)?))
}

/// Other systems build the agent only for its tests.
#[cfg(not(windows))]
fn open_path(path: &str) -> io::Result<Box<dyn PortIo>> {
    Ok(Box::new(
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?,
    ))
}
