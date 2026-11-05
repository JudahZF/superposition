//! A plug-in-free rack worker process for the lane soak: serves one bank with
//! [`sp_lane_adapter::echo::EchoWorker`]. Builds for Linux and, to run under Wine, for
//! `x86_64-pc-windows-gnu`.
//!
//! ```text
//! sp-lane-worker --bank <name> --worker-id <id> --gain <g> --load-ns <ns> --sample-rate <hz>
//! ```
//!
//! Standard input takes `retarget <bank name>` (or `retarget -` for none); the worker answers
//! `ok` or an error on standard output. It exits when standard input closes.

use std::{
    io::{self, BufRead, Write},
    sync::mpsc,
    time::Duration,
};

use sp_lane_adapter::echo::{EchoConfig, EchoWorker};

fn main() {
    if let Err(error) = run() {
        eprintln!("sp-lane-worker: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let (bank, config) = parse(std::env::args().skip(1))?;
    let (commands, received) = mpsc::channel();
    // Control arrives off the processing thread, as in the product worker.
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if commands.send(Some(line)).is_err() {
                return;
            }
        }
        let _ = commands.send(None);
    });
    let mut worker = EchoWorker::open(&bank, config).map_err(|error| error.to_string())?;
    loop {
        while let Ok(command) = received.try_recv() {
            let Some(command) = command else {
                return Ok(());
            };
            let reply = match command.strip_prefix("retarget ") {
                Some("-") => worker.retarget(None).map_err(|error| error.to_string()),
                Some(next) => worker
                    .retarget(Some(next))
                    .map_err(|error| error.to_string()),
                None => Err(format!("unknown command `{command}`")),
            };
            let mut stdout = io::stdout().lock();
            let _ = writeln!(stdout, "{}", reply.err().unwrap_or_else(|| "ok".to_owned()));
            let _ = stdout.flush();
        }
        worker.serve_once().map_err(|error| error.to_string())?;
    }
}

fn parse(mut arguments: impl Iterator<Item = String>) -> Result<(String, EchoConfig), String> {
    let mut bank = None;
    let mut config = EchoConfig {
        worker_id: 1,
        gain: 1.0,
        load: Duration::ZERO,
        sample_rate: 96_000,
        realtime: true,
    };
    while let Some(option) = arguments.next() {
        let value = arguments
            .next()
            .ok_or_else(|| format!("`{option}` needs a value"))?;
        let invalid = || format!("invalid value for `{option}`: {value}");
        match option.as_str() {
            "--bank" => bank = Some(value.clone()),
            "--worker-id" => config.worker_id = value.parse().map_err(|_| invalid())?,
            "--gain" => config.gain = value.parse().map_err(|_| invalid())?,
            "--load-ns" => {
                config.load = Duration::from_nanos(value.parse().map_err(|_| invalid())?);
            }
            "--sample-rate" => config.sample_rate = value.parse().map_err(|_| invalid())?,
            _ => return Err(format!("unknown option `{option}`")),
        }
    }
    Ok((bank.ok_or("--bank is required")?, config))
}
