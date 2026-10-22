//! `superposition --doctor`: checks the system settings a Linux engine needs.
//!
//! The engine also runs these checks at start and prints the ones that are not met.

use std::{env, path::PathBuf};

use crate::HelperBinaries;

/// The processing threads' default `SCHED_FIFO` priority (`SUPERPOSITION_RT_PRIORITY`).
const PROCESSING_PRIORITY: u64 = 70;

#[derive(Clone, Copy, Eq, PartialEq)]
enum Status {
    Pass,
    /// Works, with a limit the user should know.
    Warn,
    /// A feature does not work.
    Fail,
}

struct Check {
    name: &'static str,
    status: Status,
    detail: String,
}

impl Check {
    fn new(name: &'static str, status: Status, detail: impl Into<String>) -> Self {
        Self {
            name,
            status,
            detail: detail.into(),
        }
    }

    fn line(&self) -> String {
        let status = match self.status {
            Status::Pass => "PASS",
            Status::Warn => "WARN",
            Status::Fail => "FAIL",
        };
        format!("[{status}] {}: {}", self.name, self.detail)
    }
}

/// Prints every check. Returns the process exit code: 1 when a check fails.
pub(crate) fn run() -> i32 {
    let checks = checks();
    for check in &checks {
        println!("{}", check.line());
    }
    i32::from(checks.iter().any(|check| check.status == Status::Fail))
}

/// Prints the checks that are not met, for the engine's start.
pub(crate) fn warn_at_start() {
    let problems: Vec<Check> = checks()
        .into_iter()
        .filter(|check| check.status != Status::Pass)
        .collect();
    for check in &problems {
        eprintln!("superposition: {}", check.line());
    }
    if !problems.is_empty() {
        eprintln!("superposition: run `superposition --doctor` for every check");
    }
}

fn checks() -> Vec<Check> {
    vec![realtime(), helpers(), display(), wine(), ntsync()]
}

fn realtime() -> Check {
    const NAME: &str = "real-time priority";
    let rtprio = sp_shared_memory_os::realtime_priority_limit();
    if rtprio >= PROCESSING_PRIORITY {
        return Check::new(NAME, Status::Pass, format!("RLIMIT_RTPRIO is {rtprio}"));
    }
    match sp_shared_memory_os::rtkit_max_priority() {
        Ok(priority) => Check::new(
            NAME,
            Status::Warn,
            format!(
                "native threads get SCHED_RR {priority} from rtkit; Windows workers under Wine need \
                 RLIMIT_RTPRIO {PROCESSING_PRIORITY} or more (now {rtprio})"
            ),
        ),
        Err(error) => Check::new(
            NAME,
            Status::Fail,
            format!(
                "RLIMIT_RTPRIO is {rtprio} and rtkit is unreachable ({error}); processing threads \
                 use ordinary scheduling, so blocks below 128 frames may miss deadlines. Grant \
                 rtprio 95 to the audio group (the NixOS module does this)"
            ),
        ),
    }
}

fn helpers() -> Check {
    const NAME: &str = "helpers";
    match HelperBinaries::resolve() {
        Ok(helpers) => {
            let mut missing = Vec::new();
            if helpers.streamer.is_none() {
                missing.push("sp-window-streamer (editors cannot be shown)");
            }
            if helpers.windows.is_none() {
                missing.push("windows/ worker and scanner (Windows plug-ins are unavailable)");
            }
            if missing.is_empty() {
                Check::new(NAME, Status::Pass, "native and Windows helpers are present")
            } else {
                Check::new(
                    NAME,
                    Status::Warn,
                    format!("missing {}", missing.join(", ")),
                )
            }
        }
        Err(error) => Check::new(NAME, Status::Fail, error),
    }
}

fn display() -> Check {
    const NAME: &str = "editor display";
    match program("SUPERPOSITION_XVFB", "Xvfb") {
        Some(path) => Check::new(NAME, Status::Pass, format!("Xvfb at {}", path.display())),
        None => Check::new(
            NAME,
            Status::Fail,
            "Xvfb not found (set SUPERPOSITION_XVFB or install it); plug-in editors cannot open",
        ),
    }
}

fn wine() -> Check {
    const NAME: &str = "Wine";
    match program("SUPERPOSITION_WINE", "wine") {
        Some(path) => Check::new(NAME, Status::Pass, format!("{}", path.display())),
        None => Check::new(
            NAME,
            Status::Warn,
            "not found (set SUPERPOSITION_WINE or install it); Windows plug-ins are unavailable",
        ),
    }
}

fn ntsync() -> Check {
    const NAME: &str = "ntsync";
    if std::path::Path::new("/dev/ntsync").exists() {
        Check::new(NAME, Status::Pass, "/dev/ntsync is present")
    } else {
        Check::new(
            NAME,
            Status::Warn,
            "/dev/ntsync is missing (`modprobe ntsync`); Wine uses slower synchronisation",
        )
    }
}

/// A program from its `SUPERPOSITION_*` variable, else from `PATH`.
fn program(variable: &str, name: &str) -> Option<PathBuf> {
    env::var_os(variable)
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .or_else(|| {
            env::split_paths(&env::var_os("PATH")?)
                .map(|directory| directory.join(name))
                .find(|path| path.is_file())
        })
}
