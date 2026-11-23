//! Phase V0 probe of the Windows plug-in VM plan: the round trip from a Linux host to a
//! worker in the VM, over one VM port, at a real block size.
//!
//! ```text
//! vm-probe host --socket PATH [--port N] [--frames 32] [--rate 96000] [--seconds 60]
//!               [--window-us US] [--cpu N] [--realtime] [--interval-seconds 10]
//! vm-probe fake-guest --socket PATH [--port N] [--cpu N] [--realtime]   (Linux, plays the guest)
//! vm-probe guest [--port N] [--cpu N]                      (Windows, in the VM)
//! ```
//!
//! `host` prints progress on stderr and the JSON report on stdout. A port server
//! (`sp-ivshmem-server`) must already serve the socket. In the report and the progress
//! lines, a percentile of 10000 µs means "10 ms or more": the histogram ends there.
//! `max_us` is the true worst round trip.

// The probe runs on a Linux host and in a Windows guest. Elsewhere it prints its usage.
#[cfg(any(target_os = "linux", windows))]
mod guest;
#[cfg(target_os = "linux")]
mod host;
#[cfg(target_os = "linux")]
mod stats;

use std::{collections::HashMap, process::ExitCode};

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(mode) = args.next() else {
        return usage();
    };
    let mut flags = HashMap::new();
    while let Some(flag) = args.next() {
        let Some(name) = flag.strip_prefix("--") else {
            return usage();
        };
        let value = if name == "realtime" {
            String::new()
        } else {
            let Some(value) = args.next() else {
                return usage();
            };
            value
        };
        flags.insert(name.to_owned(), value);
    }
    match run(&mode, &flags) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("vm-probe: {error}");
            ExitCode::FAILURE
        }
    }
}

fn usage() -> ExitCode {
    eprintln!(
        "usage: vm-probe host --socket PATH [--port N] [--frames N] [--rate HZ] [--seconds S] \
         [--window-us US] [--cpu N] [--realtime] [--interval-seconds S]\n       \
         vm-probe fake-guest --socket PATH [--port N] [--cpu N] [--realtime]\n       \
         vm-probe guest [--port N] [--cpu N]"
    );
    ExitCode::from(2)
}

fn number<T: std::str::FromStr>(
    flags: &HashMap<String, String>,
    name: &str,
    default: T,
) -> Result<T, String> {
    flags.get(name).map_or(Ok(default), |value| {
        value
            .parse()
            .map_err(|_| format!("--{name}: not a number: {value}"))
    })
}

fn run(mode: &str, flags: &HashMap<String, String>) -> Result<ExitCode, String> {
    let port: u32 = number(flags, "port", 0)?;
    let cpu: Option<usize> = flags
        .get("cpu")
        .map(|cpu| {
            cpu.parse()
                .map_err(|_| format!("--cpu: not a number: {cpu}"))
        })
        .transpose()?;
    match mode {
        #[cfg(target_os = "linux")]
        "host" => {
            let socket = flags.get("socket").ok_or("--socket is required")?;
            let options = host::HostOptions {
                socket: socket.into(),
                port,
                frames: number(flags, "frames", 32)?,
                sample_rate: number(flags, "rate", 96_000)?,
                duration: std::time::Duration::from_secs(number(flags, "seconds", 60)?),
                window: flags
                    .get("window-us")
                    .map(|_| number(flags, "window-us", 0).map(std::time::Duration::from_micros))
                    .transpose()?,
                cpu,
                realtime: flags.contains_key("realtime"),
                interval: std::time::Duration::from_secs(number(flags, "interval-seconds", 10)?),
            };
            let report = host::run(&options).map_err(|error| error.to_string())?;
            let json = serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?;
            println!("{json}");
            Ok(if report.misses == 0 && report.lost == 0 {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
        #[cfg(target_os = "linux")]
        "fake-guest" => {
            let socket = flags.get("socket").ok_or("--socket is required")?;
            let realtime = flags.contains_key("realtime");
            Err(host::fake_guest(socket.as_ref(), port, cpu, realtime).to_string())
        }
        #[cfg(windows)]
        "guest" => windows_guest(port, cpu),
        _ => {
            let _ = (port, cpu);
            Ok(usage())
        }
    }
}

/// Opens bank 0 of `port` once the host has written it, and answers until it fails; then
/// opens it again, so a host restart needs no guest restart.
#[cfg(windows)]
fn windows_guest(port: u32, cpu: Option<usize>) -> Result<ExitCode, String> {
    use sp_shared_memory_os::{MonotonicClock, SharedMemoryRegion, port_bank_name};
    use std::{io::ErrorKind, time::Duration};

    let cpu = cpu
        .map(u32::try_from)
        .transpose()
        .map_err(|_| "--cpu is out of range")?;
    let class = guest::enter_realtime(cpu).map_err(|error| format!("scheduling: {error}"))?;
    eprintln!("vm-probe: guest on port {port}, priority class {class:#x}");
    let clock = MonotonicClock::new().map_err(|error| error.to_string())?;
    let name = port_bank_name(port, 0);
    loop {
        match SharedMemoryRegion::open(&name) {
            Ok(mut region) => {
                eprintln!("vm-probe: serving {name} as peer {:?}", region.vm_peer_id());
                let error = guest::serve(&mut region, clock);
                eprintln!("vm-probe: {name}: {error}; opening again");
            }
            Err(error)
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::InvalidData) => {}
            Err(error) => return Err(format!("{name}: {error}")),
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
