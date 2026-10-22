//! Engine-owned private X server for plug-in editor windows on Linux.
//!
//! Every plug-in editor renders on this server, never on the user's desktop. The supervisor
//! starts `Xvfb` with `-displayfd`, so the server picks a free display number and reports it
//! once it accepts connections. TCP is disabled; clients use the normal Unix socket in
//! `/tmp/.X11-unix`. A random MIT-MAGIC-COOKIE-1 in a private authority file keeps other local
//! users off the server. Workers and the window streamer reach it through
//! [`DisplayEnvironment`].

use std::{
    env,
    ffi::OsString,
    fs::{self, File},
    io::{self, BufRead, BufReader, Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use rustix::process::{Pid, Signal, kill_process};

/// Screen size of the private server. Large enough that editors never need to overlap.
const SCREEN: &str = "3840x2160x24";
/// How long `Xvfb` may take to report its display number.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `Xvfb` may take to exit after `SIGTERM` before it is killed.
const STOP_TIMEOUT: Duration = Duration::from_secs(2);
const COOKIE_NAME: &[u8] = b"MIT-MAGIC-COOKIE-1";
/// Cookie bytes. Alphanumerics keep about 95 bits of entropy in 16 bytes and let clients that
/// take the cookie as text, such as xcb's `AuthInfo`, connect without an authority file.
const COOKIE_ALPHABET: &[u8; 62] =
    b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
static NEXT_AUTHORITY_DIRECTORY: AtomicU64 = AtomicU64::new(0);

/// What a child process needs to reach a private display: `DISPLAY` and `XAUTHORITY`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DisplayEnvironment {
    /// `DISPLAY` value, such as `:7`.
    pub display: String,
    /// `XAUTHORITY` file that holds the display's access cookie.
    pub authority: PathBuf,
}

/// Points a child at `display`, or at no display at all.
///
/// Plug-ins must never reach the user's desktop, so the inherited `DISPLAY`, `XAUTHORITY`, and
/// `WAYLAND_DISPLAY` never pass through.
pub(crate) fn apply_display(command: &mut Command, display: Option<&DisplayEnvironment>) {
    command.env_remove("WAYLAND_DISPLAY");
    match display {
        Some(display) => command
            .env("DISPLAY", &display.display)
            .env("XAUTHORITY", &display.authority),
        None => command.env_remove("DISPLAY").env_remove("XAUTHORITY"),
    };
}

/// A running private `Xvfb` server. Dropping it kills and reaps the server and removes its
/// authority file.
pub struct PrivateDisplay {
    xvfb: PathBuf,
    server: Child,
    number: u32,
    authority: AuthorityDirectory,
}

impl PrivateDisplay {
    /// Starts `Xvfb` from `SUPERPOSITION_XVFB`, or else from `PATH`.
    ///
    /// # Errors
    ///
    /// Returns an error when the server cannot start or does not report a display in time.
    pub fn start() -> io::Result<Self> {
        Self::start_with(env::var_os("SUPERPOSITION_XVFB").unwrap_or_else(|| "Xvfb".into()))
    }

    /// Starts the given `Xvfb` executable on a free display number.
    ///
    /// # Errors
    ///
    /// Returns an error when the authority file cannot be written, or the server cannot start
    /// or does not report a display in time.
    pub fn start_with(xvfb: impl Into<PathBuf>) -> io::Result<Self> {
        let xvfb = xvfb.into();
        let authority = AuthorityDirectory::create()?;
        let (server, number) = spawn_server(&xvfb, authority.file(), None)?;
        Ok(Self {
            xvfb,
            server,
            number,
            authority,
        })
    }

    /// The `DISPLAY` name, such as `:7`.
    #[must_use]
    pub fn name(&self) -> String {
        format!(":{}", self.number)
    }

    /// The authority file that clients need in `XAUTHORITY`.
    #[must_use]
    pub fn authority(&self) -> &Path {
        self.authority.file()
    }

    /// The `DISPLAY` and `XAUTHORITY` values for a client of this server.
    #[must_use]
    pub fn environment(&self) -> DisplayEnvironment {
        DisplayEnvironment {
            display: self.name(),
            authority: self.authority().to_path_buf(),
        }
    }

    /// Restarts the server if it has exited. It keeps its display number when that number is
    /// still free, so clients can reconnect with the same environment. Returns whether the
    /// server was restarted; the caller must then re-apply [`Self::environment`] when the name
    /// changed, and reopen editors.
    ///
    /// # Errors
    ///
    /// Returns an error when the server cannot be polled or restarted. A later call retries.
    pub fn ensure_running(&mut self) -> io::Result<bool> {
        if self.server.try_wait()?.is_none() {
            return Ok(false);
        }
        let (server, number) =
            spawn_server(&self.xvfb, self.authority.file(), Some(self.number))
                .or_else(|_| spawn_server(&self.xvfb, self.authority.file(), None))?;
        self.server = server;
        self.number = number;
        Ok(true)
    }
}

impl Drop for PrivateDisplay {
    /// `SIGTERM` lets `Xvfb` remove its socket and lock file. `SIGKILL` follows if it hangs.
    fn drop(&mut self) {
        if let Ok(None) = self.server.try_wait() {
            let _ = kill_process(Pid::from_child(&self.server), Signal::TERM);
            let deadline = Instant::now() + STOP_TIMEOUT;
            while matches!(self.server.try_wait(), Ok(None)) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
        }
        let _ = self.server.kill();
        let _ = self.server.wait();
    }
}

/// Starts `Xvfb` with `-displayfd 1` and waits for the display number it writes to stdout.
fn spawn_server(xvfb: &Path, authority: &Path, number: Option<u32>) -> io::Result<(Child, u32)> {
    let mut command = Command::new(xvfb);
    if let Some(number) = number {
        command.arg(format!(":{number}"));
    }
    command
        .args(["-displayfd", "1", "-nolisten", "tcp", "-noreset"])
        .args(["-screen", "0", SCREEN])
        .arg("-auth")
        .arg(authority)
        .stdin(Stdio::null())
        .stdout(Stdio::piped());
    let mut server = command.spawn().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("could not start {}: {error}", xvfb.display()),
        )
    })?;
    let stdout = server
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("Xvfb display report pipe is unavailable"))?;
    // The read blocks until Xvfb is ready or exits, so it runs off this thread under a deadline.
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("sp-xvfb-displayfd".to_owned())
        .spawn(move || {
            let mut line = String::new();
            let result = BufReader::new(stdout).read_line(&mut line).map(|_| line);
            let _ = sender.send(result);
        })?;
    let reported = receiver
        .recv_timeout(STARTUP_TIMEOUT)
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Xvfb did not report a display"))
        .and_then(|result| result)
        .and_then(|line| {
            parse_display_number(&line).ok_or_else(|| {
                io::Error::other(format!("Xvfb exited before reporting a display: {line:?}"))
            })
        });
    match reported {
        Ok(number) => Ok((server, number)),
        Err(error) => {
            let _ = server.kill();
            let status = server.wait();
            Err(io::Error::new(
                error.kind(),
                format!("{error} (Xvfb status {status:?})"),
            ))
        }
    }
}

/// Parses the decimal display number `Xvfb -displayfd` writes, followed by a newline.
fn parse_display_number(line: &str) -> Option<u32> {
    line.strip_suffix('\n')?.parse().ok()
}

/// A private directory holding one Xauthority file. Removed on drop.
struct AuthorityDirectory {
    directory: PathBuf,
    file: PathBuf,
}

impl AuthorityDirectory {
    fn create() -> io::Result<Self> {
        let base = env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .unwrap_or_else(env::temp_dir);
        let directory = loop {
            let mut name = OsString::from("superposition-display-");
            name.push(format!(
                "{}-{}",
                std::process::id(),
                NEXT_AUTHORITY_DIRECTORY.fetch_add(1, Ordering::Relaxed)
            ));
            let directory = base.join(name);
            match fs::DirBuilder::new().mode(0o700).create(&directory) {
                Ok(()) => break directory,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        };
        let this = Self {
            file: directory.join("Xauthority"),
            directory,
        };
        let mut cookie = [0_u8; 16];
        File::open("/dev/urandom")?.read_exact(&mut cookie)?;
        let cookie = cookie.map(|byte| COOKIE_ALPHABET[usize::from(byte) % COOKIE_ALPHABET.len()]);
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&this.file)?
            .write_all(&authority_entry(&cookie))?;
        Ok(this)
    }

    fn file(&self) -> &Path {
        &self.file
    }
}

impl Drop for AuthorityDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.file);
        let _ = fs::remove_dir(&self.directory);
    }
}

/// One Xauthority record that grants `cookie` for any host and display number. The server
/// loads every record's cookie; Xau matches the wildcard family and empty number for clients.
fn authority_entry(cookie: &[u8; 16]) -> Vec<u8> {
    const FAMILY_WILD: u16 = 0xffff;
    let mut entry = Vec::with_capacity(2 + 2 + 2 + 2 + COOKIE_NAME.len() + 2 + cookie.len());
    entry.extend(FAMILY_WILD.to_be_bytes());
    // Empty address and display number, each a big-endian length followed by no bytes.
    entry.extend([0, 0, 0, 0]);
    for field in [COOKIE_NAME, cookie.as_slice()] {
        let length = u16::try_from(field.len()).expect("Xauthority fields are short");
        entry.extend(length.to_be_bytes());
        entry.extend(field);
    }
    entry
}

#[cfg(test)]
mod tests {
    use std::{os::unix::net::UnixStream, path::Path, process::Command, thread, time::Duration};

    use super::{
        DisplayEnvironment, PrivateDisplay, apply_display, authority_entry, parse_display_number,
    };

    #[test]
    fn authority_entry_is_a_wildcard_mit_cookie_record() {
        let cookie = [7_u8; 16];
        let entry = authority_entry(&cookie);
        let mut expected = vec![0xff, 0xff, 0, 0, 0, 0, 0, 18];
        expected.extend(b"MIT-MAGIC-COOKIE-1");
        expected.extend([0, 16]);
        expected.extend(cookie);
        assert_eq!(entry, expected);
    }

    #[test]
    fn display_number_requires_one_complete_line() {
        assert_eq!(parse_display_number("12\n"), Some(12));
        assert_eq!(parse_display_number(""), None, "Xvfb exited first");
        assert_eq!(parse_display_number("1"), None, "incomplete report");
        assert_eq!(parse_display_number(":1\n"), None);
    }

    /// A child never inherits the user's desktop display, with or without a private one.
    #[test]
    fn children_reach_only_the_private_display() {
        let environment = |display| {
            let mut command = Command::new("true");
            apply_display(&mut command, display);
            let mut variables = command
                .get_envs()
                .map(|(name, value)| {
                    (
                        name.to_string_lossy().into_owned(),
                        value.map(|value| value.to_string_lossy().into_owned()),
                    )
                })
                .collect::<Vec<_>>();
            variables.sort();
            variables
        };
        let private = DisplayEnvironment {
            display: ":7".to_owned(),
            authority: "/run/user/1/sp/Xauthority".into(),
        };
        assert_eq!(
            environment(Some(&private)),
            [
                ("DISPLAY".to_owned(), Some(":7".to_owned())),
                ("WAYLAND_DISPLAY".to_owned(), None),
                (
                    "XAUTHORITY".to_owned(),
                    Some("/run/user/1/sp/Xauthority".to_owned())
                ),
            ]
        );
        assert_eq!(
            environment(None),
            [
                ("DISPLAY".to_owned(), None),
                ("WAYLAND_DISPLAY".to_owned(), None),
                ("XAUTHORITY".to_owned(), None),
            ]
        );
    }

    fn socket(name: &str) -> String {
        format!("/tmp/.X11-unix/X{}", name.trim_start_matches(':'))
    }

    #[test]
    #[ignore = "starts Xvfb from SUPERPOSITION_XVFB or PATH"]
    fn private_display_restarts_on_its_number_and_cleans_up() {
        let mut display = PrivateDisplay::start().expect("start Xvfb");
        let name = display.name();
        let authority = display.authority().to_path_buf();
        UnixStream::connect(socket(&name)).expect("server accepts local connections");
        assert!(!display.ensure_running().expect("poll server"));

        // SIGKILL leaves a stale socket and lock file behind, as a crash does.
        display.server.kill().expect("kill server");
        let mut restarted = false;
        for _ in 0..100 {
            restarted = display.ensure_running().expect("restart server");
            if restarted {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(restarted, "a dead server restarts");
        assert_eq!(
            display.name(),
            name,
            "a restart keeps a free display number"
        );
        UnixStream::connect(socket(&name)).expect("restarted server accepts connections");

        drop(display);
        assert!(!authority.exists(), "the authority file is removed");
        assert!(
            !Path::new(&socket(&name)).exists(),
            "Xvfb removes its socket when it exits"
        );
    }
}
