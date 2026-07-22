//! Versioned Unix-domain control client for isolated plug-in workers.
//!
//! The client stays outside every real-time path. It launches only the deployed worker helper,
//! sets finite socket deadlines, and uses the protocol's request gate identities so a response
//! cannot be accepted for a different rack, dual-bank generation, or request ID.

use std::{
    fmt, io,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use sp_protocol::control::{
    ControlOperation, ControlProtocolError, ControlRequest, ControlRequestId, ControlResponse,
    ControlResponseStatus, ControlTarget, SlotIdentity, unix::UnixControlClient,
};

use crate::{HelperKind, HelperLaunch, ProcessSupervisor};

const CONNECT_RETRY_INTERVAL: Duration = Duration::from_millis(5);

/// Errors returned by a versioned worker-control request or launch lifecycle operation.
#[derive(Debug)]
pub enum WorkerControlError {
    /// The underlying Unix socket or process operation failed.
    Io(io::Error),
    /// The bounded control protocol rejected framing, target identity, or payload validation.
    Protocol(ControlProtocolError),
    /// A socket connection or request response exceeded its finite control deadline.
    TimedOut,
    /// A launch selected a role other than the dedicated worker helper.
    NotPluginWorker,
    /// A helper launch omitted or contradicted the required production worker arguments.
    InvalidLaunchArguments(String),
    /// The initial correlated health response rejected the newly launched worker.
    InitialHealthRejected(ControlResponseStatus),
}

impl fmt::Display for WorkerControlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "worker control I/O failed: {error}"),
            Self::Protocol(error) => write!(formatter, "worker control protocol failed: {error}"),
            Self::TimedOut => formatter.write_str("worker control operation timed out"),
            Self::NotPluginWorker => {
                formatter.write_str("worker control can launch only sp-plugin-worker")
            }
            Self::InvalidLaunchArguments(message) => {
                write!(
                    formatter,
                    "invalid production worker launch arguments: {message}"
                )
            }
            Self::InitialHealthRejected(status) => {
                write!(
                    formatter,
                    "worker rejected initial health request with {status:?}"
                )
            }
        }
    }
}

impl std::error::Error for WorkerControlError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Protocol(error) => Some(error),
            Self::TimedOut
            | Self::NotPluginWorker
            | Self::InvalidLaunchArguments(_)
            | Self::InitialHealthRejected(_) => None,
        }
    }
}

impl From<io::Error> for WorkerControlError {
    fn from(error: io::Error) -> Self {
        if matches!(
            error.kind(),
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        ) {
            Self::TimedOut
        } else {
            Self::Io(error)
        }
    }
}

impl From<ControlProtocolError> for WorkerControlError {
    fn from(error: ControlProtocolError) -> Self {
        if let ControlProtocolError::Io(error) = &error
            && matches!(
                error.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            )
        {
            return Self::TimedOut;
        }
        Self::Protocol(error)
    }
}

/// Bounded, version-aware client for one connected rack worker.
pub struct WorkerControlClient {
    stream: UnixControlClient,
    target: ControlTarget,
    timeout: Duration,
    next_request_id: u64,
}

impl WorkerControlClient {
    /// Connects to a worker's Unix-domain control endpoint with an explicit rack/bank target.
    ///
    /// # Errors
    ///
    /// Returns an error for an unavailable endpoint or invalid timeout configuration.
    pub fn connect(
        socket_path: impl AsRef<Path>,
        target: ControlTarget,
        timeout: Duration,
    ) -> Result<Self, WorkerControlError> {
        if timeout.is_zero() {
            return Err(WorkerControlError::TimedOut);
        }
        let stream = UnixStream::connect(socket_path)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        Ok(Self {
            stream: UnixControlClient::from_stream(stream),
            target,
            timeout,
            next_request_id: 1,
        })
    }

    /// Returns the immutable rack/bank identity accepted for this connection.
    #[must_use]
    pub const fn target(&self) -> ControlTarget {
        self.target
    }

    /// Returns the finite timeout applied to every socket read and write.
    #[must_use]
    pub const fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Sends one bounded operation and validates the exact correlated rack/bank response.
    ///
    /// `payload` is operation-specific and bounded by `sp-protocol` before it reaches the socket.
    /// The returned response may carry a worker-side error status; such a response is a valid
    /// transport result for the supervisor's recovery policy.
    ///
    /// # Errors
    ///
    /// Returns an error for framing, I/O, protocol-version, target, request-ID, or payload checks.
    pub fn request(
        &mut self,
        operation: ControlOperation,
        slot: Option<SlotIdentity>,
        payload: &[u8],
    ) -> Result<ControlResponse, WorkerControlError> {
        let request = ControlRequest::new(
            self.allocate_request_id()?,
            self.target,
            operation,
            slot,
            payload,
        )?;
        Ok(self.stream.round_trip(&request)?)
    }

    fn allocate_request_id(&mut self) -> Result<ControlRequestId, WorkerControlError> {
        let request_id = ControlRequestId::new(self.next_request_id)?;
        self.next_request_id = self.next_request_id.checked_add(1).ok_or_else(|| {
            WorkerControlError::InvalidLaunchArguments(
                "control request ID space is exhausted for this connection".to_owned(),
            )
        })?;
        Ok(request_id)
    }
}

/// Trusted worker launch inputs owned by the supervisor, not the audio callback.
#[derive(Clone, Debug)]
pub struct WorkerControlLaunch {
    /// The deployed helper launch command.
    pub launch: HelperLaunch,
    /// Absolute Unix-domain socket path created by the worker.
    pub control_socket: PathBuf,
    /// Exact rack/bank target sent on every control request.
    pub target: ControlTarget,
    /// Finite connection and per-request timeout.
    pub timeout: Duration,
}

impl WorkerControlLaunch {
    /// Validates that this configuration can launch only the dedicated deployed worker helper.
    ///
    /// # Errors
    ///
    /// Returns an error for an untrusted helper role/path, relative endpoint, zero timeout, or
    /// product arguments which fail to match the explicit rack/bank target.
    pub fn validate(&self) -> Result<(), WorkerControlError> {
        if self.launch.kind != HelperKind::PluginWorker {
            return Err(WorkerControlError::NotPluginWorker);
        }
        self.launch.validate_helper_only()?;
        if !self.control_socket.is_absolute() {
            return Err(WorkerControlError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "worker control socket must be an absolute path",
            )));
        }
        if self.timeout.is_zero() {
            return Err(WorkerControlError::TimedOut);
        }
        validate_production_worker_arguments(
            &self.launch.arguments,
            &self.control_socket,
            self.target,
        )
    }
}

fn validate_production_worker_arguments(
    arguments: &[String],
    control_socket: &Path,
    target: ControlTarget,
) -> Result<(), WorkerControlError> {
    let Some((mode, options)) = arguments.split_first() else {
        return Err(WorkerControlError::InvalidLaunchArguments(
            "missing `--worker` mode selector".to_owned(),
        ));
    };
    if mode != "--worker" {
        return Err(WorkerControlError::InvalidLaunchArguments(
            "first argument must be `--worker`".to_owned(),
        ));
    }
    if !options.len().is_multiple_of(2) {
        return Err(WorkerControlError::InvalidLaunchArguments(
            "worker options must be option/value pairs".to_owned(),
        ));
    }

    let mut bank_seen = false;
    let mut worker_id_seen = false;
    let mut rack_index_seen = false;
    let mut rack_generation_seen = false;
    let mut bank_index_seen = false;
    let mut bank_generation_seen = false;
    let mut socket_seen = false;
    let mut bundle_seen = false;
    let mut class_id_seen = false;
    for pair in options.chunks_exact(2) {
        let option = &pair[0];
        let value = &pair[1];
        match option.as_str() {
            "--bank" if !bank_seen && !value.is_empty() => bank_seen = true,
            "--worker-id"
                if !worker_id_seen
                    && value
                        .parse::<u32>()
                        .is_ok_and(|worker_id| worker_id != 0 && worker_id < u32::MAX - 2) =>
            {
                worker_id_seen = true;
            }
            "--rack-index"
                if !rack_index_seen && value.parse::<u8>().ok() == Some(target.rack().index()) =>
            {
                rack_index_seen = true;
            }
            "--rack-generation"
                if !rack_generation_seen
                    && value.parse::<u64>().ok() == Some(target.rack().generation()) =>
            {
                rack_generation_seen = true;
            }
            "--bank-index"
                if !bank_index_seen && value.parse::<u8>().ok() == Some(target.bank().index()) =>
            {
                bank_index_seen = true;
            }
            "--bank-generation"
                if !bank_generation_seen
                    && value.parse::<u64>().ok() == Some(target.bank().generation()) =>
            {
                bank_generation_seen = true;
            }
            "--control-socket"
                if !socket_seen
                    && Path::new(value).is_absolute()
                    && Path::new(value) == control_socket =>
            {
                socket_seen = true;
            }
            "--bundle" if !bundle_seen && !value.is_empty() => bundle_seen = true,
            "--class-id" if !class_id_seen && !value.is_empty() => class_id_seen = true,
            _ => {
                return Err(WorkerControlError::InvalidLaunchArguments(format!(
                    "unexpected, duplicate, or inconsistent `{option}` option"
                )));
            }
        }
    }
    if bank_seen
        && worker_id_seen
        && rack_index_seen
        && rack_generation_seen
        && bank_index_seen
        && bank_generation_seen
        && socket_seen
        && bundle_seen
        && class_id_seen
    {
        Ok(())
    } else {
        Err(WorkerControlError::InvalidLaunchArguments(
            "worker launch requires --bank, --worker-id, --rack-index, --rack-generation, --bank-index, --bank-generation, --control-socket, --bundle, and --class-id"
                .to_owned(),
        ))
    }
}

/// A launched worker process paired with its validated versioned control client.
pub struct WorkerControlSession {
    process_id: u64,
    client: WorkerControlClient,
    initial_health: ControlResponse,
}

impl WorkerControlSession {
    /// Launches a verified worker helper, waits only until the bounded socket deadline, and sends
    /// an initial health request through the exact rack/bank identity gate.
    ///
    /// # Errors
    ///
    /// Returns an error after stopping the newly launched helper when its endpoint or initial
    /// health request cannot be established inside the configured deadline.
    pub fn launch(
        supervisor: &mut ProcessSupervisor,
        configuration: &WorkerControlLaunch,
    ) -> Result<Self, WorkerControlError> {
        configuration.validate()?;
        let process_id = supervisor.launch(&configuration.launch)?;
        let deadline = Instant::now() + configuration.timeout;
        let client = loop {
            match WorkerControlClient::connect(
                &configuration.control_socket,
                configuration.target,
                configuration.timeout,
            ) {
                Ok(client) => break client,
                Err(WorkerControlError::Io(error))
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound
                            | io::ErrorKind::ConnectionRefused
                            | io::ErrorKind::WouldBlock
                    ) && Instant::now() < deadline =>
                {
                    thread::sleep(CONNECT_RETRY_INTERVAL);
                }
                Err(error) => {
                    let _ = supervisor.stop(process_id);
                    return Err(error);
                }
            }
            if Instant::now() >= deadline {
                let _ = supervisor.stop(process_id);
                return Err(WorkerControlError::TimedOut);
            }
        };
        let mut client = client;
        let initial_health = match client.request(ControlOperation::QueryHealth, None, &[]) {
            Ok(response) if response.status() == ControlResponseStatus::Ok => response,
            Ok(response) => {
                let _ = supervisor.stop(process_id);
                return Err(WorkerControlError::InitialHealthRejected(response.status()));
            }
            Err(error) => {
                let _ = supervisor.stop(process_id);
                return Err(error);
            }
        };
        Ok(Self {
            process_id,
            client,
            initial_health,
        })
    }

    /// Returns the operating-system identifier of the launched worker process.
    #[must_use]
    pub const fn process_id(&self) -> u64 {
        self.process_id
    }

    /// Returns the correlated initial health response that proved the worker control gate was live.
    #[must_use]
    pub fn initial_health(&self) -> &ControlResponse {
        &self.initial_health
    }

    /// Borrows the bounded client for lifecycle and state operations.
    pub fn client_mut(&mut self) -> &mut WorkerControlClient {
        &mut self.client
    }

    /// Requests protocol shutdown, then ensures the child process has terminated.
    ///
    /// # Errors
    ///
    /// Returns a control error if the shutdown response fails validation or process termination
    /// fails. A worker-side failure status remains visible in the returned response.
    pub fn shutdown(
        mut self,
        supervisor: &mut ProcessSupervisor,
    ) -> Result<ControlResponse, WorkerControlError> {
        match self.client.request(ControlOperation::Shutdown, None, &[]) {
            Ok(response) => {
                supervisor.stop(self.process_id)?;
                Ok(response)
            }
            Err(error) => {
                let _ = supervisor.stop(self.process_id);
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        os::unix::net::{UnixListener, UnixStream},
        sync::mpsc,
        thread,
    };

    use sp_protocol::control::{
        BankIdentity, ControlTarget, RackIdentity, unix::UnixControlServer,
    };

    use super::*;

    fn target() -> ControlTarget {
        ControlTarget::new(
            RackIdentity::new(2, 11).expect("valid rack"),
            BankIdentity::new(1, 17).expect("valid bank"),
        )
    }

    #[test]
    fn client_rejects_response_for_a_stale_bank_generation() {
        let directory = std::env::temp_dir().join(format!(
            "superposition-worker-control-{}-{}",
            std::process::id(),
            1
        ));
        std::fs::create_dir_all(&directory).expect("temporary control directory");
        let socket = directory.join("worker.sock");
        let listener = UnixListener::bind(&socket).expect("control listener");
        let (ready_tx, ready_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            ready_tx.send(()).expect("server is ready");
            let (stream, _) = listener.accept().expect("client connection");
            let mut server = UnixControlServer::from_stream(stream, target());
            let request = server.receive().expect("request frame");
            let stale_target = ControlTarget::new(
                request.target().rack(),
                BankIdentity::new(1, 18).expect("different bank generation"),
            );
            let response = ControlResponse::success(
                request.request_id(),
                stale_target,
                request.slot(),
                b"stale",
            )
            .expect("response");
            server.respond(&response).expect("response frame");
        });
        ready_rx.recv().expect("listener started");

        let mut client = WorkerControlClient::connect(&socket, target(), Duration::from_secs(1))
            .expect("client connects");
        assert!(matches!(
            client.request(ControlOperation::QueryHealth, None, &[]),
            Err(WorkerControlError::Protocol(
                ControlProtocolError::CorrelationMismatch
            ))
        ));

        server.join().expect("server joins");
        std::fs::remove_dir_all(directory).expect("temporary directory cleanup");
    }

    #[test]
    fn worker_launch_validation_requires_consistent_explicit_product_target() {
        let target = target();
        let configuration = WorkerControlLaunch {
            launch: HelperLaunch {
                kind: HelperKind::PluginWorker,
                executable: PathBuf::from(
                    "/Applications/Superposition.app/Contents/Helpers/sp-plugin-worker",
                ),
                arguments: vec![
                    "--worker".to_owned(),
                    "--bank".to_owned(),
                    "/sp-worker-bank".to_owned(),
                    "--worker-id".to_owned(),
                    "17".to_owned(),
                    "--rack-index".to_owned(),
                    "2".to_owned(),
                    "--rack-generation".to_owned(),
                    "11".to_owned(),
                    "--bank-index".to_owned(),
                    "1".to_owned(),
                    "--bank-generation".to_owned(),
                    "17".to_owned(),
                    "--control-socket".to_owned(),
                    "/tmp/superposition-worker.sock".to_owned(),
                    "--bundle".to_owned(),
                    "/Library/Audio/Plug-Ins/VST3/Example.vst3".to_owned(),
                    "--class-id".to_owned(),
                    "example-class".to_owned(),
                ],
            },
            control_socket: PathBuf::from("/tmp/superposition-worker.sock"),
            target,
            timeout: Duration::from_secs(1),
        };
        assert!(configuration.validate().is_ok());

        let mut inconsistent = configuration;
        inconsistent.launch.arguments[12] = "18".to_owned();
        assert!(matches!(
            inconsistent.validate(),
            Err(WorkerControlError::InvalidLaunchArguments(_))
        ));
    }

    #[test]
    fn request_ids_are_monotonic_on_one_client_connection() {
        let (client_stream, server_stream) = UnixStream::pair().expect("Unix pair");
        let server = thread::spawn(move || {
            let mut server = UnixControlServer::from_stream(server_stream, target());
            for _ in 0..2 {
                let request = server.receive().expect("request");
                let response = ControlResponse::success(
                    request.request_id(),
                    request.target(),
                    request.slot(),
                    &[],
                )
                .expect("response");
                server.respond(&response).expect("reply");
            }
        });
        client_stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("timeout");
        client_stream
            .set_write_timeout(Some(Duration::from_secs(1)))
            .expect("timeout");
        let mut client = WorkerControlClient {
            stream: UnixControlClient::from_stream(client_stream),
            target: target(),
            timeout: Duration::from_secs(1),
            next_request_id: 1,
        };
        assert!(
            client
                .request(ControlOperation::QueryHealth, None, &[])
                .is_ok()
        );
        assert!(
            client
                .request(ControlOperation::QuerySlotAttribution, None, &[])
                .is_ok()
        );
        server.join().expect("server joins");
    }
}
