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
//! The environment keeps one `wineserver` alive for its lifetime, so worker starts do not pay
//! for a cold Wine start.

use std::{
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};

/// File name of the bridge DLL in Wine's builtin-DLL layout.
pub const BRIDGE_DLL: &str = "sp_wine_bridge.dll";

/// Where the shared prefix keeps Windows VST3 bundles.
pub const VST3_DIR_IN_PREFIX: &str = "drive_c/Program Files/Common Files/VST3";

/// A Wine installation, prefix, and bridge shared by every Windows process.
#[derive(Debug)]
pub struct WineEnvironment {
    wine: PathBuf,
    prefix: PathBuf,
    bridge_dir: PathBuf,
    display: Option<OsString>,
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
            wine,
            prefix,
            bridge_dir,
            display: None,
            server: None,
        })
    }

    /// Sends every later Windows window to `display` (the engine's private X server).
    #[must_use]
    pub fn with_display(mut self, display: impl Into<OsString>) -> Self {
        self.display = Some(display.into());
        self
    }

    /// The shared prefix.
    #[must_use]
    pub fn prefix(&self) -> &Path {
        &self.prefix
    }

    /// The prefix directory that Windows VST3 installers write to.
    #[must_use]
    pub fn vst3_dir(&self) -> PathBuf {
        self.prefix.join(VST3_DIR_IN_PREFIX)
    }

    /// Creates or updates the prefix, then keeps one `wineserver` running.
    ///
    /// The first call can take tens of seconds. Call it off the UI and audio threads.
    ///
    /// # Errors
    /// Returns an error when `wineboot` or `wineserver` fails.
    pub fn prepare(&mut self) -> Result<(), WineError> {
        std::fs::create_dir_all(&self.prefix).map_err(|source| WineError::Io {
            action: "create the Wine prefix",
            source,
        })?;
        let status = self
            .command_for("wineboot")
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
            return Err(WineError::PrefixFailed(status));
        }
        std::fs::create_dir_all(self.vst3_dir()).map_err(|source| WineError::Io {
            action: "create the prefix VST3 directory",
            source,
        })?;
        if self
            .server
            .as_mut()
            .is_none_or(|server| server.try_wait().ok().flatten().is_some())
        {
            // `-p` keeps the server alive without clients; `-f` keeps it in the foreground,
            // so this value owns and reaps it.
            let server = self
                .command_for("wineserver")
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
        }
        Ok(())
    }

    /// A command that runs `program` (a Windows `.exe`, or a Unix path Wine maps through `Z:`)
    /// under Wine with the shared prefix, bridge, and display.
    #[must_use]
    pub fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(&self.wine);
        command.arg(program);
        self.apply_environment(&mut command);
        command
    }

    /// Starts an installer or licence manager in the shared prefix. Its windows appear on the
    /// engine's display, where the window streamer shows them.
    ///
    /// # Errors
    /// Returns an error when Wine cannot start the program.
    pub fn run_program(&self, program: &Path) -> Result<Child, WineError> {
        self.command(program)
            .stdin(Stdio::null())
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

    fn command_for(&self, tool: &str) -> Command {
        // wineboot and wineserver live beside the pinned wine binary.
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
            // Wine's Wayland driver would bypass the private X server.
            .env_remove("WAYLAND_DISPLAY");
        match &self.display {
            Some(display) => command.env("DISPLAY", display),
            None => command.env_remove("DISPLAY"),
        };
    }
}

impl Drop for WineEnvironment {
    fn drop(&mut self) {
        if let Some(mut server) = self.server.take() {
            // `wineserver -k` ends the prefix's processes cleanly before the server exits.
            let _ = self.command_for("wineserver").arg("-k").status();
            let _ = server.kill();
            let _ = server.wait();
        }
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
