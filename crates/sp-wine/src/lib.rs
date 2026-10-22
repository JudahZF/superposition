//! The Wine environment that Windows plug-ins, their installers, and licence managers share.
//!
//! One prefix holds every Windows plug-in and licence, so each licence is activated once
//! (iLok and others bind to the prefix). Every Windows process Superposition starts gets the same
//! pinned Wine, prefix, `sp_wine_bridge` DLL path, and display:
//!
//! - Wine: `SUPERPOSITION_WINE` (the dev shell and package set it), else `wine` on `PATH`.
//!   Licences can break when Wine changes, so the package pins one build.
//! - Prefix: `<app support>/wine` unless `SUPERPOSITION_WINE_PREFIX` overrides it.
//! - Bridge: the directory holding `x86_64-windows/sp_wine_bridge.dll` and
//!   `x86_64-unix/sp_wine_bridge.so` (see `cargo xtask wine-bridge`), passed as `WINEDLLPATH`.
//! - Display: the engine's private X server, so plug-in windows never reach the desktop.
//!
//! [`WineLauncher`] only describes how to start a Windows program; the process supervisor uses
//! it for workers and scanners and adds the display itself. [`WineEnvironment`] owns the prefix
//! and keeps one `wineserver` alive, so worker starts do not pay for a cold Wine start.

use std::{
    ffi::{OsStr, OsString},
    io,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};

/// File name of the bridge DLL in Wine's builtin-DLL layout.
pub const BRIDGE_DLL: &str = "sp_wine_bridge.dll";

/// Where the shared prefix keeps Windows VST3 bundles.
pub const VST3_DIR_IN_PREFIX: &str = "drive_c/Program Files/Common Files/VST3";

/// Extra Windows VST3 folders, as absolute Unix paths separated like `PATH`.
pub const EXTRA_VST3_PATH_VARIABLE: &str = "SUPERPOSITION_WINDOWS_VST3_PATH";

/// How to start a Windows program with the pinned Wine, the shared prefix, and the bridge.
///
/// Cheap to clone and compare; it starts nothing itself and leaves `DISPLAY` to the caller.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WineLauncher {
    wine: PathBuf,
    prefix: PathBuf,
    bridge_dir: PathBuf,
}

impl WineLauncher {
    /// A launcher for `wine` with `prefix` and the bridge layout in `bridge_dir`.
    #[must_use]
    pub const fn new(wine: PathBuf, prefix: PathBuf, bridge_dir: PathBuf) -> Self {
        Self {
            wine,
            prefix,
            bridge_dir,
        }
    }

    /// A command that runs `program` (a Unix path to a Windows `.exe`; Wine maps it through `Z:`)
    /// under Wine. It sets the prefix, bridge path, and quiet logging, and drops
    /// `WAYLAND_DISPLAY` so Wine's Wayland driver cannot bypass the private X server.
    #[must_use]
    pub fn command(&self, program: impl AsRef<OsStr>) -> Command {
        let mut command = Command::new(&self.wine);
        command.arg(program);
        self.apply_environment(&mut command);
        command
    }

    /// The shared prefix.
    #[must_use]
    pub fn prefix(&self) -> &Path {
        &self.prefix
    }

    /// The prefix folder that Windows VST3 installers write to.
    #[must_use]
    pub fn vst3_dir(&self) -> PathBuf {
        self.prefix.join(VST3_DIR_IN_PREFIX)
    }

    /// Every folder scanned for Windows VST3 bundles: the prefix folder, then each absolute
    /// folder in [`EXTRA_VST3_PATH_VARIABLE`].
    #[must_use]
    pub fn vst3_dirs(&self) -> Vec<PathBuf> {
        let mut dirs = vec![self.vst3_dir()];
        if let Some(extra) = std::env::var_os(EXTRA_VST3_PATH_VARIABLE) {
            dirs.extend(std::env::split_paths(&extra).filter(|path| path.is_absolute()));
        }
        dirs
    }

    /// A Wine tool such as `wineboot` or `wineserver`, which live beside the pinned `wine`.
    fn tool(&self, tool: &str) -> Command {
        let tool = self
            .wine
            .parent()
            .map(|dir| dir.join(tool))
            .filter(|path| path.is_file())
            .unwrap_or_else(|| PathBuf::from(tool));
        let mut command = Command::new(tool);
        self.apply_environment(&mut command);
        command
    }

    fn apply_environment(&self, command: &mut Command) {
        command
            .env("WINEPREFIX", &self.prefix)
            .env("WINEDLLPATH", &self.bridge_dir)
            .env("WINEDEBUG", "-all")
            .env_remove("WAYLAND_DISPLAY");
    }
}

/// The X display Windows windows open on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WineDisplay {
    /// `DISPLAY`, such as `:7`.
    pub display: OsString,
    /// `XAUTHORITY` file with the display's cookie.
    pub authority: PathBuf,
}

/// The shared prefix and its persistent `wineserver`.
#[derive(Debug)]
pub struct WineEnvironment {
    launcher: WineLauncher,
    display: Option<WineDisplay>,
    server: Option<Child>,
}

/// Why the Wine environment cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum WineError {
    /// No Wine binary was found.
    #[error("Wine is not installed; set SUPERPOSITION_WINE or put `wine` on PATH")]
    WineMissing,
    /// The bridge DLL is not in the expected layout.
    #[error("sp_wine_bridge is missing from {0}; run `cargo xtask wine-bridge`")]
    BridgeMissing(PathBuf),
    /// Starting or waiting for a Wine process failed.
    #[error("{action}: {source}")]
    Io {
        /// What was being done.
        action: &'static str,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// `wineboot` could not create or update the prefix.
    #[error("wineboot failed with {0}")]
    PrefixFailed(std::process::ExitStatus),
}

impl WineEnvironment {
    /// Resolves Wine and checks the bridge layout. Does not start any process.
    ///
    /// `app_support` is the app's data directory; `bridge_dir` holds the bridge layout.
    ///
    /// # Errors
    /// Returns [`WineError::WineMissing`] or [`WineError::BridgeMissing`].
    pub fn new(app_support: &Path, bridge_dir: PathBuf) -> Result<Self, WineError> {
        let wine = std::env::var_os("SUPERPOSITION_WINE")
            .map(PathBuf::from)
            .or_else(|| find_on_path("wine"))
            .ok_or(WineError::WineMissing)?;
        if !bridge_dir.join("x86_64-windows").join(BRIDGE_DLL).is_file()
            || !bridge_dir
                .join("x86_64-unix")
                .join("sp_wine_bridge.so")
                .is_file()
        {
            return Err(WineError::BridgeMissing(bridge_dir));
        }
        let prefix = std::env::var_os("SUPERPOSITION_WINE_PREFIX")
            .map_or_else(|| app_support.join("wine"), PathBuf::from);
        Ok(Self {
            launcher: WineLauncher::new(wine, prefix, bridge_dir),
            display: None,
            server: None,
        })
    }

    /// How workers and scanners start under this environment.
    #[must_use]
    pub const fn launcher(&self) -> &WineLauncher {
        &self.launcher
    }

    /// The shared prefix.
    #[must_use]
    pub fn prefix(&self) -> &Path {
        self.launcher.prefix()
    }

    /// The prefix folder that Windows VST3 installers write to.
    #[must_use]
    pub fn vst3_dir(&self) -> PathBuf {
        self.launcher.vst3_dir()
    }

    /// Whether [`Self::prepare`] has a live `wineserver`.
    pub fn is_prepared(&mut self) -> bool {
        self.server
            .as_mut()
            .is_some_and(|server| server.try_wait().ok().flatten().is_none())
    }

    /// Sends later Windows windows to `display`, or to none. Wine's desktop process keeps the
    /// display it started with, so a change stops the prefix's processes and server; the next
    /// [`Self::prepare`] starts them on the new display.
    pub fn set_display(&mut self, display: Option<WineDisplay>) {
        if self.display != display {
            self.stop_server();
            self.display = display;
        }
    }

    /// Creates or updates the prefix, then keeps one `wineserver` running.
    ///
    /// The first call can take tens of seconds. Call it off the audio thread.
    ///
    /// # Errors
    /// Returns an error when `wineboot` or `wineserver` fails.
    pub fn prepare(&mut self) -> Result<(), WineError> {
        if self.is_prepared() {
            return Ok(());
        }
        std::fs::create_dir_all(self.prefix()).map_err(|source| WineError::Io {
            action: "create the Wine prefix",
            source,
        })?;
        // `-p` keeps the server alive without clients; `-f` keeps it in the foreground, so this
        // value owns and reaps it. It starts first so wineboot does not start its own.
        let mut server = self.launcher.tool("wineserver");
        self.apply_display(&mut server);
        let server = server
            .args(["-f", "-p"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|source| WineError::Io {
                action: "start wineserver",
                source,
            })?;
        self.server = Some(server);
        let mut wineboot = self.launcher.tool("wineboot");
        self.apply_display(&mut wineboot);
        let status = wineboot
            .arg("--init")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|source| WineError::Io {
                action: "run wineboot",
                source,
            })?;
        if !status.success() {
            self.stop_server();
            return Err(WineError::PrefixFailed(status));
        }
        std::fs::create_dir_all(self.vst3_dir()).map_err(|source| WineError::Io {
            action: "create the prefix VST3 directory",
            source,
        })
    }

    /// Starts an installer or licence manager in the shared prefix. Its windows appear on the
    /// engine's display, where the window streamer shows them. Call [`Self::prepare`] first.
    ///
    /// # Errors
    /// Returns an error when Wine cannot start the program.
    pub fn run_program(&self, program: &Path) -> Result<Child, WineError> {
        // msiexec installs a Windows Installer package; Wine runs every other program itself.
        let is_msi = program
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("msi"));
        let mut command = if is_msi {
            let mut command = self.launcher.command("msiexec");
            command.arg("/i").arg(program);
            command
        } else {
            self.launcher.command(program)
        };
        self.apply_display(&mut command);
        if let Some(directory) = program.parent() {
            command.current_dir(directory);
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|source| WineError::Io {
                action: "start the Windows program",
                source,
            })
    }

    /// Converts a Unix path to the Windows path a Wine process sees, through the `Z:` drive.
    #[must_use]
    pub fn windows_path(path: &Path) -> String {
        format!("Z:{}", path.display()).replace('/', "\\")
    }

    fn apply_display(&self, command: &mut Command) {
        match &self.display {
            Some(display) => command
                .env("DISPLAY", &display.display)
                .env("XAUTHORITY", &display.authority),
            None => command.env_remove("DISPLAY").env_remove("XAUTHORITY"),
        };
    }

    /// Ends the prefix's processes and its server.
    fn stop_server(&mut self) {
        if let Some(mut server) = self.server.take() {
            // `wineserver -k` ends the prefix's processes cleanly before the server exits.
            let _ = self.launcher.tool("wineserver").arg("-k").status();
            let _ = server.kill();
            let _ = server.wait();
        }
    }
}

impl Drop for WineEnvironment {
    fn drop(&mut self) {
        self.stop_server();
    }
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::WineEnvironment;

    #[test]
    fn unix_paths_map_through_the_z_drive() {
        assert_eq!(
            WineEnvironment::windows_path(Path::new("/home/me/Setup Tool.exe")),
            "Z:\\home\\me\\Setup Tool.exe"
        );
    }
}
