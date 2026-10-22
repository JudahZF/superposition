//! Versioned control client for isolated plug-in workers.
//!
//! A worker's stdin and stdout are its control channel, on every platform and under Wine. The
//! client stays outside every real-time path. It launches only the deployed worker helper, sets
//! finite deadlines, and uses the protocol's request gate identities so a response cannot be
//! accepted for a different rack, dual-bank generation, or request ID.

use std::{
    fmt,
    io::{self, Read, Write},
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::Duration,
};

use sp_protocol::control::{
    ControlOperation, ControlProtocolError, ControlRequest, ControlRequestId, ControlResponse,
    ControlResponseStatus, ControlTarget, SlotIdentity, pipe::PipeControlClient,
};
use sp_protocol::payload::{
    ControlPayloadCodec, EditorPreviewDescriptor, EditorPreviewRequest, MAX_STATE_STREAM_BYTES,
    MAX_STATE_TRANSFER_CHUNK_BYTES, StateChunkRequest, StateChunkWrite, StateRestore,
    StateTransferDescriptor, StateTransferId, StateTransferLengths,
};

use crate::{HelperKind, HelperLaunch, ProcessSupervisor, WorkerPipes};

const LONG_CONTROL_TIMEOUT: Duration = Duration::from_secs(30);
/// Sample rates a worker accepts, in hertz.
const WORKER_SAMPLE_RATES: [u32; 2] = [48_000, 96_000];

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
    /// The underlying pipe or process operation failed.
    Io(io::Error),
    /// The bounded control protocol rejected framing, target identity, or payload validation.
    Protocol(ControlProtocolError),
    /// A request response exceeded its finite control deadline.
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

type PendingRequest = Receiver<(
    PipeControlClient,
    Result<ControlResponse, WorkerControlError>,
)>;

/// Bounded, version-aware client for one rack worker's control pipes.
pub struct WorkerControlClient {
    pipe: Option<PipeControlClient>,
    pending: Option<PendingRequest>,
    target: ControlTarget,
    timeout: Duration,
    next_request_id: u64,
    closed: bool,
}

impl WorkerControlClient {
    /// Talks to a launched worker through its control pipes with an explicit rack/bank target.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero timeout or when the response reader cannot start.
    pub fn new(
        pipes: WorkerPipes,
        target: ControlTarget,
        timeout: Duration,
    ) -> Result<Self, WorkerControlError> {
        Self::from_streams(pipes.stdin, pipes.stdout, target, timeout)
    }

    /// Like [`Self::new`] over any request writer and response reader.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero timeout or when the response reader cannot start.
    pub fn from_streams(
        requests: impl Write + Send + 'static,
        responses: impl Read + Send + 'static,
        target: ControlTarget,
        timeout: Duration,
    ) -> Result<Self, WorkerControlError> {
        if timeout.is_zero() {
            return Err(WorkerControlError::TimedOut);
        }
        Ok(Self {
            pipe: Some(PipeControlClient::new(requests, responses)?),
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

    /// Returns the finite timeout applied to every ordinary request.
    #[must_use]
    pub const fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Sends one bounded operation and validates the exact correlated rack/bank response.
    ///
    /// `payload` is operation-specific and bounded by `sp-protocol` before it reaches the pipe.
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
        self.request_within(
            operation,
            slot,
            payload,
            operation_timeout(operation, self.timeout),
        )
    }

    fn request_within(
        &mut self,
        operation: ControlOperation,
        slot: Option<SlotIdentity>,
        payload: &[u8],
        timeout: Duration,
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
        let pipe = self
            .pipe
            .as_mut()
            .ok_or(WorkerControlError::ConnectionClosed)?;
        let result = pipe
            .round_trip(&request, timeout)
            .map_err(WorkerControlError::from);
        // A late response may still arrive, so the pipes cannot carry another request.
        self.closed = result.is_err();
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
        let mut pipe = self
            .pipe
            .take()
            .ok_or(WorkerControlError::ConnectionClosed)?;
        let deadline = operation_timeout(operation, self.timeout);
        let (sender, receiver) = mpsc::channel();
        thread::Builder::new()
            .name("sp-worker-control-request".to_owned())
            .spawn(move || {
                let result = pipe
                    .round_trip(&request, deadline)
                    .map_err(WorkerControlError::from);
                let _ = sender.send((pipe, result));
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
            Ok((pipe, result)) => {
                self.pending = None;
                self.closed = result.is_err();
                self.pipe = Some(pipe);
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
    /// Exact rack/bank target sent on every control request.
    pub target: ControlTarget,
    /// Finite per-request timeout.
    pub timeout: Duration,
}

impl WorkerControlLaunch {
    /// Validates that this configuration can launch only the dedicated deployed worker helper.
    ///
    /// # Errors
    ///
    /// Returns an error for an untrusted helper role/path, zero timeout, or product arguments
    /// which fail to match the explicit rack/bank target.
    pub fn validate(&self) -> Result<(), WorkerControlError> {
        if self.launch.kind != HelperKind::PluginWorker {
            return Err(WorkerControlError::NotPluginWorker);
        }
        self.launch.validate_helper_only()?;
        if self.timeout.is_zero() {
            return Err(WorkerControlError::TimedOut);
        }
        validate_production_worker_arguments(&self.launch.arguments, self.target)
    }
}

fn validate_production_worker_arguments(
    arguments: &[String],
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
    let mut sample_rate_seen = false;
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
            "--sample-rate"
                if !sample_rate_seen
                    && value
                        .parse::<u32>()
                        .is_ok_and(|rate| WORKER_SAMPLE_RATES.contains(&rate)) =>
            {
                sample_rate_seen = true;
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
        && sample_rate_seen
        && bundle_seen
        && class_id_seen
    {
        Ok(())
    } else {
        Err(WorkerControlError::InvalidLaunchArguments(
            "worker launch requires --bank, --worker-id, --rack-index, --rack-generation, --bank-index, --bank-generation, --sample-rate, --bundle, and --class-id"
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
    /// Launches a verified worker helper with its control pipes and sends an initial health
    /// request through the exact rack/bank identity gate.
    ///
    /// # Errors
    ///
    /// Returns an error after stopping the newly launched helper when its initial health
    /// request does not succeed inside the deadline.
    pub fn launch(
        supervisor: &mut ProcessSupervisor,
        configuration: &WorkerControlLaunch,
    ) -> Result<Self, WorkerControlError> {
        configuration.validate()?;
        let (process_id, pipes) = supervisor.launch_with_pipes(&configuration.launch)?;
        match Self::attach(process_id, pipes, configuration) {
            Ok(session) => Ok(session),
            Err(error) => {
                let _ = supervisor.stop(process_id);
                Err(error)
            }
        }
    }

    /// Attaches to an already launched worker's control pipes and validates its first health
    /// response. The worker answers once its first plug-in has loaded, so that response may
    /// take up to the long control deadline (a Wine worker also starts Wine first).
    ///
    /// The caller retains ownership of the child in `ProcessSupervisor`. On failure, the caller
    /// must stop or reap that process; this method never mutates the process supervisor.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid launch metadata or a missing or rejected correlated health
    /// response.
    pub fn attach(
        process_id: u64,
        pipes: WorkerPipes,
        configuration: &WorkerControlLaunch,
    ) -> Result<Self, WorkerControlError> {
        configuration.validate()?;
        if process_id == 0 {
            return Err(WorkerControlError::InvalidLaunchArguments(
                "worker process ID must be nonzero".to_owned(),
            ));
        }
        let mut client =
            WorkerControlClient::new(pipes, configuration.target, configuration.timeout)?;
        let initial_health = match client.request_within(
            ControlOperation::QueryHealth,
            None,
            &[],
            LONG_CONTROL_TIMEOUT.max(configuration.timeout),
        ) {
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
        io::{PipeReader, PipeWriter},
        path::PathBuf,
        sync::mpsc,
        thread,
        time::Instant,
    };

    use sp_protocol::control::{BankIdentity, ControlRequestGate, ControlTarget, RackIdentity};

    use super::*;

    fn target() -> ControlTarget {
        ControlTarget::new(
            RackIdentity::new(2, 11).expect("valid rack"),
            BankIdentity::new(1, 17).expect("valid bank"),
        )
    }

    /// The worker's end of a pair of control pipes.
    struct FakeWorker {
        requests: PipeReader,
        responses: PipeWriter,
        gate: ControlRequestGate,
    }

    impl FakeWorker {
        fn receive(&mut self) -> ControlRequest {
            self.gate.read_from(&mut self.requests).expect("request")
        }

        fn respond(&mut self, response: &ControlResponse) {
            response.write_to(&mut self.responses).expect("response");
        }

        fn succeed(&mut self, request: &ControlRequest, payload: &[u8]) {
            self.respond(
                &ControlResponse::success(
                    request.request_id(),
                    request.target(),
                    request.slot(),
                    payload,
                )
                .expect("response"),
            );
        }
    }

    /// A client and the fake worker on the other end of its pipes.
    fn connected(timeout: Duration) -> (WorkerControlClient, FakeWorker) {
        let (requests, request_writer) = io::pipe().expect("request pipe");
        let (response_reader, responses) = io::pipe().expect("response pipe");
        let client =
            WorkerControlClient::from_streams(request_writer, response_reader, target(), timeout)
                .expect("client");
        let worker = FakeWorker {
            requests,
            responses,
            gate: ControlRequestGate::new(target()),
        };
        (client, worker)
    }

    fn worker_arguments(target: ControlTarget) -> Vec<String> {
        [
            "--worker",
            "--bank",
            "/sp-worker-bank",
            "--worker-id",
            "17",
            "--rack-index",
            &target.rack().index().to_string(),
            "--rack-generation",
            &target.rack().generation().to_string(),
            "--bank-index",
            &target.bank().index().to_string(),
            "--bank-generation",
            &target.bank().generation().to_string(),
            "--sample-rate",
            "96000",
            "--bundle",
            "/Library/Audio/Plug-Ins/VST3/Example.vst3",
            "--class-id",
            "example-class",
        ]
        .map(str::to_owned)
        .to_vec()
    }

    #[test]
    fn client_rejects_response_for_a_stale_bank_generation() {
        let (mut client, mut worker) = connected(Duration::from_secs(1));
        let server = thread::spawn(move || {
            let request = worker.receive();
            let stale_target = ControlTarget::new(
                request.target().rack(),
                BankIdentity::new(1, 18).expect("different bank generation"),
            );
            worker.respond(
                &ControlResponse::success(
                    request.request_id(),
                    stale_target,
                    request.slot(),
                    b"stale",
                )
                .expect("response"),
            );
        });
        assert!(matches!(
            client.request(ControlOperation::QueryHealth, None, &[]),
            Err(WorkerControlError::Protocol(
                ControlProtocolError::CorrelationMismatch
            ))
        ));
        server.join().expect("server joins");
    }

    /// The first health response follows the worker's first plug-in load, which can outlast the
    /// ordinary request deadline.
    #[cfg(unix)]
    #[test]
    fn attach_waits_past_the_ordinary_deadline_for_the_first_health_response() {
        let target = target();
        // The first request on fresh pipes has ID 1, so its response is known in advance.
        let health = ControlRequest::new(
            ControlRequestId::new(1).unwrap(),
            target,
            ControlOperation::QueryHealth,
            None,
            &[],
        )
        .unwrap();
        let response = std::env::temp_dir().join(format!("sp-attach-{}", std::process::id()));
        let mut bytes = Vec::new();
        ControlResponse::success(health.request_id(), target, None, &[])
            .unwrap()
            .write_to(&mut bytes)
            .unwrap();
        std::fs::write(&response, bytes).unwrap();
        let mut processes = ProcessSupervisor::new();
        let (process_id, pipes) = processes
            .launch_with_pipes(&HelperLaunch::native(
                HelperKind::PluginWorker,
                "/bin/sh",
                vec![
                    "-c".to_owned(),
                    format!("sleep 0.3; cat '{}'; cat >/dev/null", response.display()),
                ],
            ))
            .expect("fake worker");
        let configuration = WorkerControlLaunch {
            launch: HelperLaunch::native(
                HelperKind::PluginWorker,
                "/Applications/Superposition.app/Contents/Helpers/sp-plugin-worker",
                worker_arguments(target),
            ),
            target,
            timeout: Duration::from_millis(50),
        };
        let session = WorkerControlSession::attach(process_id, pipes, &configuration)
            .expect("slow first health response");
        assert_eq!(session.initial_health().status(), ControlResponseStatus::Ok);
        // Dropping the session closes the worker's stdin, which ends it.
        drop(session);
        processes.stop(process_id).unwrap();
        std::fs::remove_file(response).unwrap();
    }

    #[test]
    fn worker_launch_validation_requires_consistent_explicit_product_target() {
        let target = target();
        let configuration = WorkerControlLaunch {
            launch: HelperLaunch::native(
                HelperKind::PluginWorker,
                "/Applications/Superposition.app/Contents/Helpers/sp-plugin-worker",
                worker_arguments(target),
            ),
            target,
            timeout: Duration::from_secs(1),
        };
        assert!(configuration.validate().is_ok());

        let mut inconsistent = configuration.clone();
        inconsistent.launch.arguments[12] = "18".to_owned();
        assert!(matches!(
            inconsistent.validate(),
            Err(WorkerControlError::InvalidLaunchArguments(_))
        ));

        let mut unsupported_rate = configuration.clone();
        unsupported_rate.launch.arguments[14] = "44100".to_owned();
        assert!(matches!(
            unsupported_rate.validate(),
            Err(WorkerControlError::InvalidLaunchArguments(_))
        ));

        // Under Wine the same arguments follow the Windows build of the worker.
        let mut windows = configuration;
        windows.launch.executable = PathBuf::from("/opt/sp/helpers/windows/sp-plugin-worker.exe");
        assert!(windows.validate().is_err());
    }

    #[test]
    fn request_ids_are_monotonic_on_one_client_connection() {
        let (mut client, mut worker) = connected(Duration::from_secs(1));
        let server = thread::spawn(move || {
            for _ in 0..2 {
                let request = worker.receive();
                worker.succeed(&request, &[]);
            }
        });
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
    fn a_timed_out_request_closes_the_pipes() {
        let (mut client, mut worker) = connected(Duration::from_millis(50));
        let server = thread::spawn(move || {
            let request = worker.receive();
            thread::sleep(Duration::from_millis(200));
            // The late response must never be taken for the next request's.
            worker.succeed(&request, b"late");
        });
        assert!(matches!(
            client.request(ControlOperation::QueryHealth, None, &[]),
            Err(WorkerControlError::TimedOut)
        ));
        assert!(matches!(
            client.request(ControlOperation::QueryHealth, None, &[]),
            Err(WorkerControlError::ConnectionClosed)
        ));
        server.join().unwrap();
    }

    #[test]
    fn background_request_does_not_block_and_preserves_order() {
        let (mut client, mut worker) = connected(Duration::from_secs(3));
        let (received_tx, received_rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let first = worker.receive();
            received_tx.send(first.operation()).expect("signal request");
            reply_rx.recv().expect("release first reply");
            worker.succeed(&first, b"editor opened");
            let second = worker.receive();
            assert_eq!(second.operation(), ControlOperation::QueryHealth);
            assert!(second.request_id() > first.request_id());
            worker.succeed(&second, b"healthy");
        });
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

        let (mut client, mut server) = connected(Duration::from_secs(2));
        let slot = SlotIdentity::new(5).unwrap();
        let original = StateRestore {
            component: vec![0xA5; MAX_STATE_TRANSFER_CHUNK_BYTES * 8 + 1],
            controller: vec![0x5A; MAX_STATE_TRANSFER_CHUNK_BYTES + 3],
        };
        let expected = original.clone();
        let server = thread::spawn(move || {
            let mut restored = StateRestore {
                component: vec![0; expected.component.len()],
                controller: vec![0; expected.controller.len()],
            };
            let mut capture_released = false;
            loop {
                let request = server.receive();
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
                        server.respond(&response);
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
                server.respond(&response);
            }
        });
        assert_eq!(client.capture_state(slot).unwrap(), original);
        client.restore_state(slot, &original).unwrap();
        server.join().unwrap();
    }
}
