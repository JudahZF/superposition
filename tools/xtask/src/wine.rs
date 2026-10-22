//! Builds the `sp_wine_bridge` unixlib and checks the Linux↔Wine shared-memory boundary.
//!
//! `wine-bridge` writes Wine's builtin-DLL layout, which `WINEDLLPATH` must point at:
//! `<out>/x86_64-windows/sp_wine_bridge.dll` (PE, marked builtin) and
//! `<out>/x86_64-unix/sp_wine_bridge.so`. Wine pairs the two only when it finds the PE through
//! that layout.
//!
//! `wine-check` runs the Windows build of `sp-shared-memory-os` under Wine against a bank this
//! Linux process creates, and measures futex round trips through the bridge.
//!
//! `windows-helpers` puts the Windows plug-in helpers and the bridge beside the native helpers,
//! where the app looks for them:
//!
//! ```text
//! <helpers>/windows/sp-plugin-worker.exe
//! <helpers>/windows/sp-plugin-scanner.exe
//! <helpers>/wine-bridge/x86_64-windows/sp_wine_bridge.dll
//! <helpers>/wine-bridge/x86_64-unix/sp_wine_bridge.so
//! ```

use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

use sp_shared_memory_os::{MonotonicClock, SharedMemoryRegion};

const ROUNDS: u32 = 20_000;
/// A reply this late means the wake was lost, not merely scheduled late.
const LOST_WAKE: Duration = Duration::from_secs(1);
const READY_TIMEOUT: Duration = Duration::from_mins(2);

/// The directory that holds `x86_64-windows/` and `x86_64-unix/`.
pub(crate) fn bridge_dir(workspace_root: &Path) -> PathBuf {
    workspace_root.join("target").join("wine-bridge")
}

/// The pinned Wine installation: `SUPERPOSITION_WINE_ROOT`, or the parent of `wine` on PATH.
fn wine_root() -> Result<PathBuf, String> {
    if let Some(root) = std::env::var_os("SUPERPOSITION_WINE_ROOT") {
        return Ok(PathBuf::from(root));
    }
    let output = Command::new("sh")
        .args(["-c", "command -v wine"])
        .output()
        .map_err(|error| format!("could not look for wine: {error}"))?;
    let wine = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    wine.canonicalize()
        .ok()
        .and_then(|wine| wine.parent()?.parent().map(Path::to_path_buf))
        .ok_or_else(|| "wine is not on PATH; run inside `nix develop`".to_owned())
}

fn run(command: &mut Command) -> Result<(), String> {
    let status = command
        .status()
        .map_err(|error| format!("could not run {command:?}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{command:?} failed with {status}"))
    }
}

/// Builds the bridge into [`bridge_dir`].
pub(crate) fn build_bridge(workspace_root: &Path) -> Result<PathBuf, String> {
    let wine = wine_root()?;
    let source = workspace_root.join("crates/sp-shared-memory-os/wine");
    let out = bridge_dir(workspace_root);
    let pe_dir = out.join("x86_64-windows");
    let unix_dir = out.join("x86_64-unix");
    for dir in [&pe_dir, &unix_dir] {
        std::fs::create_dir_all(dir)
            .map_err(|error| format!("could not create {}: {error}", dir.display()))?;
    }
    let mingw = std::env::var("SUPERPOSITION_MINGW_CC")
        .unwrap_or_else(|_| "x86_64-w64-mingw32-gcc".to_owned());
    // Without its setup hook, the Nix mingw wrapper needs its runtime library directories.
    let mingw_lib_dirs = std::env::var("SUPERPOSITION_MINGW_LIB_DIRS").unwrap_or_default();
    let dll = pe_dir.join("sp_wine_bridge.dll");
    run(Command::new(&mingw)
        .args(std::env::split_paths(&mingw_lib_dirs).map(|dir| format!("-L{}", dir.display())))
        .args(["-O2", "-shared", "-o"])
        .arg(&dll)
        .arg(format!("-I{}", wine.join("include/wine/windows").display()))
        .arg(format!("-I{}", wine.join("include").display()))
        .arg(source.join("sp_wine_bridge_pe.c"))
        .arg(format!(
            "-L{}",
            wine.join("lib/wine/x86_64-windows").display()
        ))
        .args(["-lwinecrt0", "-lntdll", "-lkernel32"]))?;
    run(Command::new(wine.join("bin/winebuild"))
        .arg("--builtin")
        .arg(&dll))?;
    run(Command::new("cc")
        .args(["-O2", "-shared", "-fPIC", "-Wall", "-Werror", "-o"])
        .arg(unix_dir.join("sp_wine_bridge.so"))
        .arg(source.join("sp_wine_bridge_unix.c")))?;
    println!("built {}", out.display());
    Ok(out)
}

pub(crate) fn run_bridge(workspace_root: &Path, arguments: &[String]) -> Result<(), String> {
    if !arguments.is_empty() {
        return Err("wine-bridge does not accept arguments".to_owned());
    }
    build_bridge(workspace_root).map(|_| ())
}

/// Builds the Windows helpers and lays them out under `--out` (default `target/debug`, or
/// `target/release` with `--release`).
pub(crate) fn run_windows_helpers(
    workspace_root: &Path,
    arguments: &[String],
) -> Result<(), String> {
    let mut release = false;
    let mut out = None;
    let mut arguments = arguments.iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--release" => release = true,
            "--out" => {
                out = Some(PathBuf::from(
                    arguments.next().ok_or("--out requires a directory")?,
                ));
            }
            other => return Err(format!("unknown windows-helpers argument `{other}`")),
        }
    }
    let profile = if release { "release" } else { "debug" };
    let out = out.unwrap_or_else(|| workspace_root.join("target").join(profile));
    let bridge = build_bridge(workspace_root)?;
    run(
        Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned()))
            .current_dir(workspace_root)
            .args(["build", "--target", "x86_64-pc-windows-gnu"])
            .args(release.then_some("--release"))
            .args(["-p", "sp-plugin-worker", "-p", "sp-plugin-scanner"]),
    )?;
    let built = workspace_root
        .join("target/x86_64-pc-windows-gnu")
        .join(profile);
    let copy = |from: &Path, to: &Path| {
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
        }
        std::fs::copy(from, to)
            .map(drop)
            .map_err(|error| format!("could not copy {}: {error}", from.display()))
    };
    for helper in ["sp-plugin-worker.exe", "sp-plugin-scanner.exe"] {
        copy(&built.join(helper), &out.join("windows").join(helper))?;
    }
    for library in [
        "x86_64-windows/sp_wine_bridge.dll",
        "x86_64-unix/sp_wine_bridge.so",
    ] {
        copy(
            &bridge.join(library),
            &out.join("wine-bridge").join(library),
        )?;
    }
    println!("Windows helpers in {}", out.display());
    Ok(())
}

struct Worker(Child);

impl Drop for Worker {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

pub(crate) fn run_check(workspace_root: &Path, arguments: &[String]) -> Result<(), String> {
    if !arguments.is_empty() {
        return Err("wine-check does not accept arguments".to_owned());
    }
    let bridge = build_bridge(workspace_root)?;
    run(
        Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned()))
            .current_dir(workspace_root)
            .args(["build", "--release", "--target", "x86_64-pc-windows-gnu"])
            .args(["-p", "sp-shared-memory-os", "--example", "wine_ping"]),
    )?;
    let exe = workspace_root.join("target/x86_64-pc-windows-gnu/release/examples/wine_ping.exe");
    let prefix = workspace_root.join("target/wine-check-prefix");

    let region = SharedMemoryRegion::create(1).map_err(|error| error.to_string())?;
    let clock = MonotonicClock::new().map_err(|error| error.to_string())?;
    let header = &region.bank().header;
    let mut worker = Worker(
        Command::new("wine")
            .arg(&exe)
            .arg(region.name())
            .arg(ROUNDS.to_string())
            .env("WINEPREFIX", &prefix)
            .env("WINEDLLPATH", &bridge)
            .env("WINEDEBUG", "-all")
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .stdin(Stdio::null())
            .spawn()
            .map_err(|error| format!("could not start wine: {error}"))?,
    );

    // The first run also creates the prefix, which can take a while.
    let started = Instant::now();
    let worker_tick = loop {
        let tick = header.worker_heartbeat_tick.load(Ordering::Acquire);
        if tick != 0 {
            break tick;
        }
        if let Some(status) = worker.0.try_wait().map_err(|error| error.to_string())? {
            return Err(format!(
                "the Wine worker exited before it was ready: {status}"
            ));
        }
        if started.elapsed() > READY_TIMEOUT {
            return Err("the Wine worker did not open the bank".to_owned());
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let skew = clock.ticks_to_duration(clock.now_ticks().abs_diff(worker_tick));
    if skew > Duration::from_secs(5) {
        return Err(format!(
            "the Wine worker's clock is {skew:?} away from the host's; the domains differ"
        ));
    }
    println!("Wine worker opened the bank after {:?}", started.elapsed());

    let mut latencies = Vec::with_capacity(ROUNDS as usize);
    for _ in 0..ROUNDS {
        let sent = clock.now_ticks();
        if !region.notify_request() {
            return Err("request wake failed".to_owned());
        }
        let sequence = u64::from(region.request_wake_sequence());
        while header.worker_busy_operations.load(Ordering::Acquire) != sequence {
            let waited = clock.ticks_to_duration(clock.now_ticks() - sent);
            if waited > LOST_WAKE {
                return Err(format!(
                    "no reply to wake {sequence} within {LOST_WAKE:?} after {} round trips",
                    latencies.len()
                ));
            }
            std::hint::spin_loop();
        }
        latencies.push(clock.ticks_to_duration(clock.now_ticks() - sent));
        std::thread::sleep(Duration::from_micros(500));
    }
    let status = worker.0.wait().map_err(|error| error.to_string())?;
    if !status.success() {
        return Err(format!("the Wine worker failed: {status}"));
    }
    latencies.sort_unstable();
    let at = |permille: usize| latencies[(latencies.len() - 1) * permille / 1000];
    let over = |limit: Duration| latencies.iter().filter(|latency| **latency > limit).count();
    println!(
        "{ROUNDS} futex round trips through sp_wine_bridge: p50 {:?}, p99 {:?}, p99.9 {:?}, \
         max {:?}; {} over 1 ms, {} over 5 ms (without real-time priority these are scheduling \
         delays, not lost wakes)",
        at(500),
        at(990),
        at(999),
        latencies[latencies.len() - 1],
        over(Duration::from_millis(1)),
        over(Duration::from_millis(5)),
    );
    Ok(())
}
