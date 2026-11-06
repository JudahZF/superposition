use crate::{Event, HEARTBEAT_INTERVAL, Request, SCAN_TIMEOUT, STOP_TIMEOUT};
use std::{
    collections::HashMap,
    fs::File,
    io::{self, BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    thread,
    time::{Duration, Instant},
};

/// A virtio-serial port, or a stand-in for one in tests: a two-way byte stream.
pub trait PortIo: Read + Write + Send {
    /// A second handle to the same port.
    ///
    /// # Errors
    ///
    /// Returns the operating-system error if the handle cannot be duplicated.
    fn try_clone_port(&self) -> io::Result<Box<dyn PortIo>>;
}

impl PortIo for File {
    fn try_clone_port(&self) -> io::Result<Box<dyn PortIo>> {
        Ok(Box::new(self.try_clone()?))
    }
}

#[cfg(unix)]
impl PortIo for std::os::unix::net::UnixStream {
    fn try_clone_port(&self) -> io::Result<Box<dyn PortIo>> {
        Ok(Box::new(self.try_clone()?))
    }
}

/// Opens the agent port (`None`) or the worker port of a VM port (`Some(port)`).
pub type PortOpener = dyn Fn(Option<u32>) -> io::Result<Box<dyn PortIo>> + Send + Sync;

/// What the agent needs.
pub struct AgentConfig {
    /// The worker executable.
    pub worker: PathBuf,
    /// The scanner executable.
    pub scanner: PathBuf,
    /// The folder of installed VST3 bundles.
    pub vst3: PathBuf,
    /// A writable folder for scan reports.
    pub reports: PathBuf,
    /// The ID of the image that holds the agent and the worker.
    pub image_id: String,
    /// Opens the ports.
    pub open_port: Box<PortOpener>,
}

/// The agent: its ports, its workers and their relays.
pub struct Agent {
    config: AgentConfig,
    state: Arc<State>,
}

struct State {
    events: Mutex<Box<dyn PortIo>>,
    workers: Mutex<HashMap<u32, Child>>,
    /// Each worker port, opened once: the stdin of its current worker and a handle to the
    /// port. A port's input relay lives as long as the agent and writes to whichever worker
    /// the port has now.
    inputs: Mutex<HashMap<u32, WorkerPort>>,
}

struct WorkerPort {
    input: Arc<Mutex<Option<ChildStdin>>>,
    port: Box<dyn PortIo>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl State {
    /// Sends one event. A host that is not listening loses it.
    fn emit(&self, event: &Event) {
        let mut port = lock(&self.events);
        let _ = port.write_all(&event.to_line()).and_then(|()| port.flush());
    }

    /// Reports and forgets workers that exited by themselves.
    fn reap(&self) {
        let mut workers = lock(&self.workers);
        let exited: Vec<(u32, Option<i32>)> = workers
            .iter_mut()
            .filter_map(|(port, child)| {
                child
                    .try_wait()
                    .ok()
                    .flatten()
                    .map(|status| (*port, status.code()))
            })
            .collect();
        for (port, code) in exited {
            workers.remove(&port);
            self.clear_input(port);
            self.emit(&Event::Exited { port, code });
        }
    }

    fn clear_input(&self, port: u32) {
        if let Some(worker_port) = lock(&self.inputs).get(&port) {
            *lock(&worker_port.input) = None;
        }
    }

    /// Ends the worker on `port`. Returns whether no worker runs there now.
    fn stop(&self, port: u32) -> bool {
        let mut workers = lock(&self.workers);
        let Some(child) = workers.get_mut(&port) else {
            return true;
        };
        let _ = child.kill();
        let deadline = Instant::now() + STOP_TIMEOUT;
        loop {
            if matches!(child.try_wait(), Ok(Some(_))) {
                workers.remove(&port);
                self.clear_input(port);
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Agent {
    /// Opens the agent port and makes the agent.
    ///
    /// # Errors
    ///
    /// Returns the error of opening the agent port.
    pub fn new(config: AgentConfig) -> io::Result<Self> {
        let events = (config.open_port)(None)?;
        Ok(Self {
            config,
            state: Arc::new(State {
                events: Mutex::new(events),
                workers: Mutex::new(HashMap::new()),
                inputs: Mutex::new(HashMap::new()),
            }),
        })
    }

    /// Serves requests for ever. Sends a heartbeat every [`HEARTBEAT_INTERVAL`] from a
    /// thread of its own.
    ///
    /// A virtio-serial port reads nothing while no host is connected, and a second open of
    /// the port fails while the first is open. So after an empty or failed read the agent
    /// waits and reads the same handle again; its workers keep running.
    pub fn run(self) -> ! {
        let state = Arc::clone(&self.state);
        thread::spawn(move || {
            for sequence in 1.. {
                thread::sleep(HEARTBEAT_INTERVAL);
                state.reap();
                state.emit(&Event::Heartbeat { sequence });
            }
        });
        let mut requests = loop {
            // Not in a `match` scrutinee: its lock guard would live through the arms.
            let port = lock(&self.state.events).try_clone_port();
            match port {
                Ok(port) => break BufReader::new(port),
                Err(_) => thread::sleep(Duration::from_secs(1)),
            }
        };
        let mut line = String::new();
        loop {
            match requests.read_line(&mut line) {
                Ok(_) if line.ends_with('\n') => {
                    self.handle_line(line.trim());
                    line.clear();
                }
                Ok(0) | Err(_) => thread::sleep(Duration::from_millis(100)),
                // Part of a line: read the rest.
                Ok(_) => {}
            }
        }
    }

    fn handle_line(&self, line: &str) {
        if line.is_empty() {
            return;
        }
        match Request::from_line(line) {
            Ok(request) => self.handle(request),
            Err(error) => self.state.emit(&Event::Error {
                port: None,
                message: format!("bad request: {error}"),
            }),
        }
    }

    fn handle(&self, request: Request) {
        match request {
            Request::Hello => {
                let mut workers: Vec<u32> = lock(&self.state.workers).keys().copied().collect();
                workers.sort_unstable();
                self.state.emit(&Event::Hello {
                    image_id: self.config.image_id.clone(),
                    workers,
                });
            }
            Request::Scan => {
                let state = Arc::clone(&self.state);
                let (scanner, vst3, reports) = (
                    self.config.scanner.clone(),
                    self.config.vst3.clone(),
                    self.config.reports.clone(),
                );
                // Scans take seconds each; requests keep being served meanwhile.
                thread::spawn(move || scan(&state, &scanner, &vst3, &reports));
            }
            Request::Stop { port } => {
                let exited = self.state.stop(port);
                self.state.emit(&Event::Stopped { port, exited });
            }
            Request::Start { port, args } => {
                if !self.state.stop(port) {
                    self.state.emit(&Event::Stopped {
                        port,
                        exited: false,
                    });
                    return;
                }
                match self.start(port, &args) {
                    Ok(pid) => self.state.emit(&Event::Started { port, pid }),
                    Err(error) => self.state.emit(&Event::Error {
                        port: Some(port),
                        message: format!("worker did not start: {error}"),
                    }),
                }
            }
        }
    }

    fn start(&self, port: u32, args: &[String]) -> io::Result<u32> {
        let (input, output) = self.worker_port(port)?;
        let mut command = Command::new(&self.config.worker);
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // A console worker would open a console window on the VM screen.
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = command.spawn()?;
        let pid = child.id();
        *lock(&input) = child.stdin.take();
        if let Some(stdout) = child.stdout.take() {
            thread::spawn(move || relay_output(stdout, output));
        }
        lock(&self.state.workers).insert(port, child);
        Ok(pid)
    }

    /// The stdin slot of `port` and a handle to its worker port. The first call opens the
    /// port and starts its input relay.
    #[allow(clippy::type_complexity, reason = "a pair returned to one caller")]
    fn worker_port(
        &self,
        port: u32,
    ) -> io::Result<(Arc<Mutex<Option<ChildStdin>>>, Box<dyn PortIo>)> {
        let mut inputs = lock(&self.state.inputs);
        if let Some(worker_port) = inputs.get(&port) {
            return Ok((
                Arc::clone(&worker_port.input),
                worker_port.port.try_clone_port()?,
            ));
        }
        let opened = (self.config.open_port)(Some(port))?;
        let from_host = opened.try_clone_port()?;
        let to_host = opened.try_clone_port()?;
        let input = Arc::new(Mutex::new(None));
        inputs.insert(
            port,
            WorkerPort {
                input: Arc::clone(&input),
                port: opened,
            },
        );
        let relay = Arc::clone(&input);
        thread::spawn(move || relay_input(from_host, &relay));
        Ok((input, to_host))
    }
}

/// Host to worker, for the agent's lifetime. Bytes that arrive with no worker are dropped.
fn relay_input(mut from_host: Box<dyn PortIo>, input: &Mutex<Option<ChildStdin>>) {
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        match from_host.read(&mut buffer) {
            // No host on the port, or an error: try again soon.
            Ok(0) | Err(_) => thread::sleep(Duration::from_millis(50)),
            Ok(read) => {
                let mut slot = lock(input);
                if let Some(stdin) = slot.as_mut()
                    && stdin
                        .write_all(&buffer[..read])
                        .and_then(|()| stdin.flush())
                        .is_err()
                {
                    *slot = None;
                }
            }
        }
    }
}

/// Worker to host, until the worker's stdout closes.
fn relay_output(mut stdout: impl Read, mut to_host: Box<dyn PortIo>) {
    let mut buffer = vec![0_u8; 64 * 1024];
    while let Ok(read) = stdout.read(&mut buffer) {
        if read == 0 || to_host.write_all(&buffer[..read]).is_err() {
            return;
        }
        let _ = to_host.flush();
    }
}

/// Scans every bundle under `vst3`, one scanner process each, and reports each one.
fn scan(state: &State, scanner: &Path, vst3: &Path, reports: &Path) {
    let mut bundles = Vec::new();
    find_bundles(vst3, &mut bundles);
    bundles.sort();
    let _ = std::fs::create_dir_all(reports);
    for (index, bundle) in bundles.iter().enumerate() {
        let report_path = reports.join(format!("scan-{index}.json"));
        let _ = std::fs::remove_file(&report_path);
        let (code, timed_out) = run_scanner(scanner, bundle, &report_path);
        state.emit(&Event::Scanned {
            bundle: bundle.display().to_string(),
            code,
            timed_out,
            report: std::fs::read_to_string(&report_path).ok(),
        });
    }
    state.emit(&Event::ScanDone {
        bundles: u32::try_from(bundles.len()).unwrap_or(u32::MAX),
    });
}

/// Every `.vst3` bundle (file or folder) under `folder`, without looking inside bundles.
fn find_bundles(folder: &Path, bundles: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return;
    };
    for path in entries.flatten().map(|entry| entry.path()) {
        if path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("vst3"))
        {
            bundles.push(path);
        } else if path.is_dir() {
            find_bundles(&path, bundles);
        }
    }
}

/// Runs the scanner on one bundle. Returns its exit code and whether it timed out.
fn run_scanner(scanner: &Path, bundle: &Path, report: &Path) -> (Option<i32>, bool) {
    let mut command = Command::new(scanner);
    command
        .arg("--bundle")
        .arg(bundle)
        .arg("--sdk-enumerate")
        .arg("--report-path")
        .arg(report)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let Ok(mut child) = command.spawn() else {
        return (None, false);
    };
    let deadline = Instant::now() + SCAN_TIMEOUT;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return (status.code(), false);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return (None, true);
        }
        thread::sleep(Duration::from_millis(20));
    }
}
