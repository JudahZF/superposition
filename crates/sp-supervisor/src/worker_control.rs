//! Versioned Unix-domain control client for isolated plug-in workers.
//!
//! The client stays outside every real-time path. It launches only the deployed worker helper,
//! sets finite socket deadlines, and uses the protocol's request gate identities so a response
//! cannot be accepted for a different rack, dual-bank generation, or request ID.

use std::{
    fmt, io,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::{Duration, Instant},
};

use sp_protocol::control::{
    ControlOperation, ControlProtocolError, ControlRequest, ControlRequestId, ControlResponse,
    ControlResponseStatus, ControlTarget, SlotIdentity, unix::UnixControlClient,
};
use sp_protocol::payload::{
    ControlPayloadCodec, EditorPreviewDescriptor, EditorPreviewRequest, MAX_STATE_STREAM_BYTES,
    MAX_STATE_TRANSFER_CHUNK_BYTES, StateChunkRequest, StateChunkWrite, StateRestore,
    StateTransferDescriptor, StateTransferId, StateTransferLengths,
};

use crate::{HelperKind, HelperLaunch, ProcessSupervisor};

const CONNECT_RETRY_INTERVAL: Duration = Duration::from_millis(5);
const LONG_CONTROL_TIMEOUT: Duration = Duration::from_secs(30);

fn operation_timeout(operation: ControlOperation, timeout: Duration) -> Duration {
    if matches!(
        operation,
        ControlOperation::BeginStateCapture
            | ControlOperation::CommitStateRestore
            | ControlOperation::OpenNativeEditor
            | ControlOperation::CloseNativeEditor
            | ControlOperation::LoadPlugin
            | ControlOperation::UnloadSlot
    ) {
        LONG_CONTROL_TIMEOUT.max(timeout)
    } else {
        timeout
    }
}

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
    /// The worker rejected a state operation.
    Rejected {
        /// Worker response status.
        status: ControlResponseStatus,
        /// Worker-provided error message.
        message: String,
    },
    /// This connection was closed after a deadline expired.
    ConnectionClosed,
    /// A control request is already in flight on this connection.
    Busy,
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
            Self::Rejected { status, message } => write!(
                formatter,
                "worker rejected state operation with {status:?}: {message}"
            ),
            Self::ConnectionClosed => formatter.write_str("worker control connection is closed"),
            Self::Busy => formatter.write_str("worker control request is already pending"),
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
            | Self::InitialHealthRejected(_)
            | Self::Rejected { .. }
            | Self::ConnectionClosed
            | Self::Busy => None,
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

/// A worker's answer to an editor-picture poll for one slot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EditorPreviewPoll {
    /// The worker's newest capture sequence; zero before the first capture.
    pub sequence: u64,
    /// When the newest picture was captured, in Unix milliseconds.
    pub captured_at_unix_ms: u64,
    /// Whether the slot's editor window is open.
    pub editor_open: bool,
    /// PNG bytes of the newest picture when its sequence differs from the known one.
    pub png: Option<Vec<u8>>,
}

/// Bounded, version-aware client for one connected rack worker.
pub struct WorkerControlClient {
    stream: Option<UnixControlClient>,
    pending: Option<
        Receiver<(
            UnixControlClient,
            Result<ControlResponse, WorkerControlError>,
        )>,
    >,
    target: ControlTarget,
    timeout: Duration,
    next_request_id: u64,
    closed: bool,
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
            stream: Some(UnixControlClient::from_stream(stream)),
            pending: None,
            target,
            timeout,
            next_request_id: 1,
            closed: false,
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
        if self.pending.is_some() {
            return Err(WorkerControlError::Busy);
        }
        if self.closed {
            return Err(WorkerControlError::ConnectionClosed);
        }
        let request = ControlRequest::new(
            self.allocate_request_id()?,
            self.target,
            operation,
            slot,
            payload,
        )?;
        let stream = self
            .stream
            .as_mut()
            .ok_or(WorkerControlError::ConnectionClosed)?;
        stream.set_timeout(operation_timeout(operation, self.timeout))?;
        let result = stream
            .round_trip(&request)
            .map_err(WorkerControlError::from);
        if result.is_err() {
            self.closed = true;
            let _ = stream.close();
        }
        result
    }

    /// Starts one bounded request without waiting for the worker response.
    ///
    /// Poll [`Self::poll_request`] until it returns the correlated result. Other requests return
    /// [`WorkerControlError::Busy`] while this request is pending.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid framing, a closed or busy connection, or thread startup.
    pub fn begin_request(
        &mut self,
        operation: ControlOperation,
        slot: Option<SlotIdentity>,
        payload: &[u8],
    ) -> Result<(), WorkerControlError> {
        if self.pending.is_some() {
            return Err(WorkerControlError::Busy);
        }
        if self.closed {
            return Err(WorkerControlError::ConnectionClosed);
        }
        let request = ControlRequest::new(
            self.allocate_request_id()?,
            self.target,
            operation,
            slot,
            payload,
        )?;
        let stream = self
            .stream
            .take()
            .ok_or(WorkerControlError::ConnectionClosed)?;
        let deadline = operation_timeout(operation, self.timeout);
        let (sender, receiver) = mpsc::channel();
        thread::Builder::new()
            .name("sp-worker-control-request".to_owned())
            .spawn(move || {
                let mut stream = stream;
                let result = stream
                    .set_timeout(deadline)
                    .and_then(|()| stream.round_trip(&request))
                    .map_err(WorkerControlError::from);
                if result.is_err() {
                    let _ = stream.close();
                }
                let _ = sender.send((stream, result));
            })
            .map_err(|error| {
                self.closed = true;
                WorkerControlError::Io(error)
            })?;
        self.pending = Some(receiver);
        Ok(())
    }

    /// Returns whether a background request has not yet been polled to completion.
    #[must_use]
    pub const fn request_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Polls a pending request without blocking the caller.
    ///
    /// Returns `None` when idle or when the request has not completed. A completed result is
    /// returned once, and the client becomes available for another request on success.
    pub fn poll_request(&mut self) -> Option<Result<ControlResponse, WorkerControlError>> {
        let receiver = self.pending.as_ref()?;
        match receiver.try_recv() {
            Ok((stream, result)) => {
                self.pending = None;
                self.closed = result.is_err();
                self.stream = Some(stream);
                Some(result)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.pending = None;
                self.closed = true;
                Some(Err(WorkerControlError::ConnectionClosed))
            }
        }
    }

    fn state_request(
        &mut self,
        operation: ControlOperation,
        slot: SlotIdentity,
        payload: &[u8],
    ) -> Result<Vec<u8>, WorkerControlError> {
        let response = self.request(operation, Some(slot), payload)?;
        if response.status() != ControlResponseStatus::Ok {
            return Err(WorkerControlError::Rejected {
                status: response.status(),
                message: response
                    .error_record()
                    .map_or(String::new(), |record| record.message().to_owned()),
            });
        }
        Ok(response.payload().to_vec())
    }

    /// Captures both VST3 state streams through bounded control frames.
    ///
    /// # Errors
    ///
    /// Returns a worker, transport, or protocol error when capture cannot complete.
    pub fn capture_state(
        &mut self,
        slot: SlotIdentity,
    ) -> Result<StateRestore, WorkerControlError> {
        let payload = self.state_request(ControlOperation::BeginStateCapture, slot, &[])?;
        let descriptor = StateTransferDescriptor::decode(&payload)?;
        let result = (|| {
            let component =
                self.read_state_stream(slot, descriptor.id, 0, descriptor.component_len)?;
            let controller =
                self.read_state_stream(slot, descriptor.id, 1, descriptor.controller_len)?;
            Ok(StateRestore {
                component,
                controller,
            })
        })();
        let release = self.state_request(
            ControlOperation::ReleaseStateTransfer,
            slot,
            &StateTransferId { id: descriptor.id }.encode()?,
        );
        match result {
            Err(error) => Err(error),
            Ok(state) => {
                release?;
                Ok(state)
            }
        }
    }

    /// Polls a slot's newest editor picture. PNG bytes are transferred only when the worker's
    /// sequence differs from `known_sequence`. The worker answers from stored captures and never
    /// captures during the request.
    ///
    /// # Errors
    ///
    /// Returns a worker, transport, or protocol error when the poll cannot complete.
    pub fn capture_editor_preview(
        &mut self,
        slot: SlotIdentity,
        known_sequence: u64,
    ) -> Result<EditorPreviewPoll, WorkerControlError> {
        let payload = self.state_request(
            ControlOperation::CaptureEditorPreview,
            slot,
            &EditorPreviewRequest { known_sequence }.encode()?,
        )?;
        let descriptor = EditorPreviewDescriptor::decode(&payload)?;
        let png = match descriptor.transfer_id {
            None => None,
            Some(id) => {
                let png = self.read_state_stream(slot, id, 0, descriptor.png_len);
                let release = self.state_request(
                    ControlOperation::ReleaseStateTransfer,
                    slot,
                    &StateTransferId { id }.encode()?,
                );
                let png = png?;
                release?;
                Some(png)
            }
        };
        Ok(EditorPreviewPoll {
            sequence: descriptor.sequence,
            captured_at_unix_ms: descriptor.captured_at_unix_ms,
            editor_open: descriptor.editor_open,
            png,
        })
    }

    fn read_state_stream(
        &mut self,
        slot: SlotIdentity,
        id: u64,
        stream: u32,
        length: u32,
    ) -> Result<Vec<u8>, WorkerControlError> {
        let mut data = Vec::new();
        data.try_reserve_exact(length as usize)
            .map_err(|_| ControlProtocolError::InvalidPayload)?;
        while data.len() < length as usize {
            let offset =
                u32::try_from(data.len()).map_err(|_| ControlProtocolError::InvalidPayload)?;
            let count =
                u32::try_from((length as usize - data.len()).min(MAX_STATE_TRANSFER_CHUNK_BYTES))
                    .map_err(|_| ControlProtocolError::InvalidPayload)?;
            let request = StateChunkRequest {
                id,
                stream,
                offset,
                length: count,
            };
            let chunk =
                self.state_request(ControlOperation::ReadStateChunk, slot, &request.encode()?)?;
            if chunk.len() != count as usize {
                return Err(ControlProtocolError::InvalidPayload.into());
            }
            data.extend_from_slice(&chunk);
        }
        Ok(data)
    }

    /// Restores both VST3 state streams and commits only after all chunks are acknowledged.
    ///
    /// # Errors
    ///
    /// Returns a worker, transport, or protocol error when restore cannot complete.
    pub fn restore_state(
        &mut self,
        slot: SlotIdentity,
        state: &StateRestore,
    ) -> Result<(), WorkerControlError> {
        if state.component.len() > MAX_STATE_STREAM_BYTES
            || state.controller.len() > MAX_STATE_STREAM_BYTES
        {
            return Err(ControlProtocolError::InvalidPayload.into());
        }
        let lengths = StateTransferLengths {
            component_len: u32::try_from(state.component.len())
                .map_err(|_| ControlProtocolError::InvalidPayload)?,
            controller_len: u32::try_from(state.controller.len())
                .map_err(|_| ControlProtocolError::InvalidPayload)?,
        };
        let payload = self.state_request(
            ControlOperation::BeginStateRestore,
            slot,
            &lengths.encode()?,
        )?;
        let descriptor = StateTransferDescriptor::decode(&payload)?;
        let result = (|| {
            if descriptor.component_len != lengths.component_len
                || descriptor.controller_len != lengths.controller_len
            {
                return Err(ControlProtocolError::InvalidPayload.into());
            }
            self.write_state_stream(slot, descriptor.id, 0, &state.component)?;
            self.write_state_stream(slot, descriptor.id, 1, &state.controller)?;
            let response = self.state_request(
                ControlOperation::CommitStateRestore,
                slot,
                &StateTransferId { id: descriptor.id }.encode()?,
            )?;
            if !response.is_empty() {
                return Err(ControlProtocolError::InvalidPayload.into());
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = self.state_request(
                ControlOperation::ReleaseStateTransfer,
                slot,
                &StateTransferId { id: descriptor.id }.encode()?,
            );
        }
        result
    }

    fn write_state_stream(
        &mut self,
        slot: SlotIdentity,
        id: u64,
        stream: u32,
        data: &[u8],
    ) -> Result<(), WorkerControlError> {
        for (index, chunk) in data.chunks(MAX_STATE_TRANSFER_CHUNK_BYTES).enumerate() {
            let write = StateChunkWrite {
                id,
                stream,
                offset: u32::try_from(index * MAX_STATE_TRANSFER_CHUNK_BYTES)
                    .map_err(|_| ControlProtocolError::InvalidPayload)?,
                data: chunk.to_vec(),
            };
            let response =
                self.state_request(ControlOperation::WriteStateChunk, slot, &write.encode()?)?;
            if !response.is_empty() {
                return Err(ControlProtocolError::InvalidPayload.into());
            }
        }
        Ok(())
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
        match Self::connect_launched(process_id, configuration) {
            Ok(session) => Ok(session),
            Err(error) => {
                let _ = supervisor.stop(process_id);
                Err(error)
            }
        }
    }

    /// Connects to an already launched worker and validates its first health response.
    ///
    /// The caller retains ownership of the child in `ProcessSupervisor`. On failure, the caller
    /// must stop or reap that process; this method never mutates the process supervisor.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid launch metadata, an unavailable endpoint, or a rejected
    /// correlated health response before the finite connection deadline.
    pub fn connect_launched(
        process_id: u64,
        configuration: &WorkerControlLaunch,
    ) -> Result<Self, WorkerControlError> {
        configuration.validate()?;
        if process_id == 0 {
            return Err(WorkerControlError::InvalidLaunchArguments(
                "worker process ID must be nonzero".to_owned(),
            ));
        }
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
                Err(error) => return Err(error),
            }
            if Instant::now() >= deadline {
                return Err(WorkerControlError::TimedOut);
            }
        };
        let mut client = client;
        let initial_health = match client.request(ControlOperation::QueryHealth, None, &[]) {
            Ok(response) if response.status() == ControlResponseStatus::Ok => response,
            Ok(response) => {
                return Err(WorkerControlError::InitialHealthRejected(response.status()));
            }
            Err(error) => return Err(error),
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
    fn connect_launched_waits_for_delayed_socket_without_owning_child() {
        let directory = PathBuf::from("/tmp").join(format!("sp-connect-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("temporary control directory");
        let socket = directory.join("worker.sock");
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
                    "/sp-test-bank".to_owned(),
                    "--worker-id".to_owned(),
                    "1".to_owned(),
                    "--rack-index".to_owned(),
                    target.rack().index().to_string(),
                    "--rack-generation".to_owned(),
                    target.rack().generation().to_string(),
                    "--bank-index".to_owned(),
                    target.bank().index().to_string(),
                    "--bank-generation".to_owned(),
                    target.bank().generation().to_string(),
                    "--control-socket".to_owned(),
                    socket.display().to_string(),
                    "--bundle".to_owned(),
                    "/tmp/Test.vst3".to_owned(),
                    "--class-id".to_owned(),
                    "test-class".to_owned(),
                ],
            },
            control_socket: socket.clone(),
            target,
            timeout: Duration::from_secs(1),
        };
        let (ready_tx, ready_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            let listener = UnixListener::bind(&socket).expect("bind delayed socket");
            ready_tx.send(()).expect("signal bound socket");
            let (stream, _) = listener.accept().expect("accept connection");
            let mut server = UnixControlServer::from_stream(stream, target);
            let request = server.receive().expect("health request");
            assert_eq!(request.operation(), ControlOperation::QueryHealth);
            server
                .respond(
                    &ControlResponse::success(
                        request.request_id(),
                        request.target(),
                        request.slot(),
                        &[],
                    )
                    .unwrap(),
                )
                .unwrap();
        });
        let session = WorkerControlSession::connect_launched(42, &configuration)
            .expect("connect already launched worker");
        assert_eq!(session.process_id(), 42);
        assert_eq!(session.initial_health().status(), ControlResponseStatus::Ok);
        ready_rx.recv().unwrap();
        server.join().unwrap();
        std::fs::remove_dir_all(directory).expect("remove temporary control directory");
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
            stream: Some(UnixControlClient::from_stream(client_stream)),
            pending: None,
            target: target(),
            timeout: Duration::from_secs(1),
            next_request_id: 1,
            closed: false,
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

    #[test]
    fn background_request_does_not_block_and_preserves_socket_order() {
        let (client_stream, server_stream) = UnixStream::pair().expect("Unix pair");
        let (received_tx, received_rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let mut server = UnixControlServer::from_stream(server_stream, target());
            let first = server.receive().expect("first request");
            received_tx.send(first.operation()).expect("signal request");
            reply_rx.recv().expect("release first reply");
            server
                .respond(
                    &ControlResponse::success(
                        first.request_id(),
                        first.target(),
                        first.slot(),
                        b"editor opened",
                    )
                    .unwrap(),
                )
                .unwrap();
            let second = server.receive().expect("second request");
            assert_eq!(second.operation(), ControlOperation::QueryHealth);
            assert!(second.request_id() > first.request_id());
            server
                .respond(
                    &ControlResponse::success(
                        second.request_id(),
                        second.target(),
                        second.slot(),
                        b"healthy",
                    )
                    .unwrap(),
                )
                .unwrap();
        });
        let mut client = WorkerControlClient {
            stream: Some(UnixControlClient::from_stream(client_stream)),
            pending: None,
            target: target(),
            timeout: Duration::from_secs(3),
            next_request_id: 1,
            closed: false,
        };
        let slot = SlotIdentity::new(1).unwrap();
        client
            .begin_request(ControlOperation::OpenNativeEditor, Some(slot), &[])
            .unwrap();
        assert!(client.request_pending());
        received_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("server received request");
        assert!(client.poll_request().is_none());
        assert!(matches!(
            client.request(ControlOperation::QueryHealth, None, &[]),
            Err(WorkerControlError::Busy)
        ));
        assert!(matches!(
            client.begin_request(ControlOperation::OpenNativeEditor, Some(slot), &[]),
            Err(WorkerControlError::Busy)
        ));
        reply_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let response = loop {
            if let Some(result) = client.poll_request() {
                break result.unwrap();
            }
            assert!(Instant::now() < deadline, "background reply timed out");
            thread::sleep(Duration::from_millis(1));
        };
        assert_eq!(response.payload(), b"editor opened");
        assert!(!client.request_pending());
        assert_eq!(
            client
                .request(ControlOperation::QueryHealth, None, &[])
                .unwrap()
                .payload(),
            b"healthy"
        );
        server.join().unwrap();
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn chunked_state_round_trips_across_multiple_frames() {
        use sp_protocol::payload::{
            StateChunkRequest, StateChunkWrite, StateTransferDescriptor, StateTransferId,
            StateTransferLengths,
        };

        let (client_stream, server_stream) = UnixStream::pair().expect("Unix pair");
        client_stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        client_stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let slot = SlotIdentity::new(5).unwrap();
        let original = StateRestore {
            component: vec![0xA5; MAX_STATE_TRANSFER_CHUNK_BYTES * 8 + 1],
            controller: vec![0x5A; MAX_STATE_TRANSFER_CHUNK_BYTES + 3],
        };
        let expected = original.clone();
        let server = thread::spawn(move || {
            let mut server = UnixControlServer::from_stream(server_stream, target());
            let mut restored = StateRestore {
                component: vec![0; expected.component.len()],
                controller: vec![0; expected.controller.len()],
            };
            let mut capture_released = false;
            loop {
                let request = server.receive().expect("request");
                let response = match request.operation() {
                    ControlOperation::BeginStateCapture => StateTransferDescriptor {
                        id: 1,
                        component_len: u32::try_from(expected.component.len()).unwrap(),
                        controller_len: u32::try_from(expected.controller.len()).unwrap(),
                    }
                    .encode()
                    .unwrap(),
                    ControlOperation::ReadStateChunk => {
                        let chunk = StateChunkRequest::decode(request.payload()).unwrap();
                        let bytes = if chunk.stream == 0 {
                            &expected.component
                        } else {
                            &expected.controller
                        };
                        bytes[chunk.offset as usize..(chunk.offset + chunk.length) as usize]
                            .to_vec()
                    }
                    ControlOperation::ReleaseStateTransfer => {
                        assert_eq!(StateTransferId::decode(request.payload()).unwrap().id, 1);
                        capture_released = true;
                        Vec::new()
                    }
                    ControlOperation::BeginStateRestore => {
                        assert!(capture_released);
                        let lengths = StateTransferLengths::decode(request.payload()).unwrap();
                        StateTransferDescriptor {
                            id: 2,
                            component_len: lengths.component_len,
                            controller_len: lengths.controller_len,
                        }
                        .encode()
                        .unwrap()
                    }
                    ControlOperation::WriteStateChunk => {
                        let chunk = StateChunkWrite::decode(request.payload()).unwrap();
                        let bytes = if chunk.stream == 0 {
                            &mut restored.component
                        } else {
                            &mut restored.controller
                        };
                        bytes[chunk.offset as usize..chunk.offset as usize + chunk.data.len()]
                            .copy_from_slice(&chunk.data);
                        Vec::new()
                    }
                    ControlOperation::CommitStateRestore => {
                        assert_eq!(StateTransferId::decode(request.payload()).unwrap().id, 2);
                        assert_eq!(restored, expected);
                        let response = ControlResponse::success(
                            request.request_id(),
                            request.target(),
                            request.slot(),
                            &[],
                        )
                        .unwrap();
                        server.respond(&response).unwrap();
                        break;
                    }
                    operation => panic!("unexpected {operation:?}"),
                };
                let response = ControlResponse::success(
                    request.request_id(),
                    request.target(),
                    request.slot(),
                    &response,
                )
                .unwrap();
                server.respond(&response).unwrap();
            }
        });
        let mut client = WorkerControlClient {
            stream: Some(UnixControlClient::from_stream(client_stream)),
            pending: None,
            target: target(),
            timeout: Duration::from_secs(2),
            next_request_id: 1,
            closed: false,
        };
        assert_eq!(client.capture_state(slot).unwrap(), original);
        client.restore_state(slot, &original).unwrap();
        server.join().unwrap();
    }
}
