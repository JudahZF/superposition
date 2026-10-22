//! Linux: the engine-owned display that plug-in editors and Windows programs draw on.
//!
//! The engine starts a private Xvfb server and `sp-window-streamer` on it. Workers and Wine get the
//! display through their environment; the UI reads the streamer's socket. Neither reaches the
//! user's desktop.

use std::{
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::Duration,
};

use sp_supervisor::{DisplayEnvironment, PrivateDisplay};

const STREAMER_READY_TIMEOUT: Duration = Duration::from_secs(10);

/// The private display and its streamer.
pub(crate) struct EditorDisplay {
    display: PrivateDisplay,
    streamer_binary: PathBuf,
    streamer: Child,
    socket: PathBuf,
}

impl EditorDisplay {
    /// Starts Xvfb, then the streamer listening at `<runtime_dir>/windows.sock`.
    pub(crate) fn start(streamer_binary: &Path, runtime_dir: &Path) -> Result<Self, String> {
        let display = PrivateDisplay::start()
            .map_err(|error| format!("cannot start the private editor display (Xvfb): {error}"))?;
        let socket = runtime_dir.join("windows.sock");
        let streamer = start_streamer(streamer_binary, &display, &socket)?;
        Ok(Self {
            display,
            streamer_binary: streamer_binary.to_path_buf(),
            streamer,
            socket,
        })
    }

    /// What workers and Wine need to reach the display.
    pub(crate) fn environment(&self) -> DisplayEnvironment {
        self.display.environment()
    }

    /// The streamer's socket, for the UI.
    pub(crate) fn socket(&self) -> &Path {
        &self.socket
    }

    /// Restarts the display or streamer if either died. Returns `true` when the display
    /// restarted, so workers must get the new environment and editors must reopen.
    pub(crate) fn ensure_running(&mut self) -> Result<bool, String> {
        let restarted = self
            .display
            .ensure_running()
            .map_err(|error| format!("cannot restart the private editor display: {error}"))?;
        let streamer_died = self.streamer.try_wait().ok().flatten().is_some();
        if restarted || streamer_died {
            let _ = self.streamer.kill();
            let _ = self.streamer.wait();
            self.streamer = start_streamer(&self.streamer_binary, &self.display, &self.socket)?;
        }
        Ok(restarted)
    }
}

impl Drop for EditorDisplay {
    fn drop(&mut self) {
        // Closing stdin stops the streamer; kill covers a hung one.
        drop(self.streamer.stdin.take());
        let _ = self.streamer.kill();
        let _ = self.streamer.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}

fn start_streamer(binary: &Path, display: &PrivateDisplay, socket: &Path) -> Result<Child, String> {
    let _ = std::fs::remove_file(socket);
    let mut streamer = Command::new(binary)
        .arg("--listen")
        .arg(socket)
        .arg("--display")
        .arg(display.name())
        .env("XAUTHORITY", display.authority())
        .env_remove("WAYLAND_DISPLAY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot start {}: {error}", binary.display()))?;
    let stdout = streamer.stdout.take().expect("piped stdout");
    let (ready, ready_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut lines = BufReader::new(stdout).lines();
        let listening = lines.any(|line| line.is_ok_and(|line| line.starts_with("listening")));
        let _ = ready.send(listening);
        // Keep draining so the streamer never blocks on a full pipe.
        lines.for_each(drop);
    });
    if ready_rx.recv_timeout(STREAMER_READY_TIMEOUT) == Ok(true) {
        return Ok(streamer);
    }
    let _ = streamer.kill();
    let _ = streamer.wait();
    Err("the window streamer did not start".to_owned())
}

/// A private per-process directory for sockets: `$XDG_RUNTIME_DIR/superposition-<pid>`.
pub(crate) fn runtime_dir() -> Result<PathBuf, String> {
    let base = std::env::var_os("XDG_RUNTIME_DIR").map_or_else(std::env::temp_dir, PathBuf::from);
    let directory = base.join(format!("superposition-{}", std::process::id()));
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("cannot create {}: {error}", directory.display()))?;
    std::fs::set_permissions(
        &directory,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .map_err(|error| format!("cannot secure {}: {error}", directory.display()))?;
    Ok(directory)
}
