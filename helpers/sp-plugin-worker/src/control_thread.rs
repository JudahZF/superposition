//! Worker-side control transport and bounded processing-thread mailbox.
//!
//! Requests arrive as frames on the worker's original stdin and responses leave on its original
//! stdout (see [`crate::control_stdio`]). A control thread owns both. That thread never calls
//! plug-ins or touches the shared-memory audio payload. It forwards only validated bounded
//! [`ControlRequest`] values to the dedicated processing thread through fixed-capacity SPSC
//! queues and writes the matching [`ControlResponse`].

use std::{
    io::{self, Read, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use rtrb::{Consumer, Producer, RingBuffer};
use sp_protocol::control::{
    ControlErrorCode, ControlErrorRecord, ControlOperation, ControlRequest, ControlRequestGate,
    ControlResponse, ControlResponseStatus, ControlTarget, MAX_PENDING_CONTROL_REQUESTS,
    pipe::{FrameReceiver, FrameWait},
};

const POLL_INTERVAL: Duration = Duration::from_millis(1);
/// How long the control thread waits for a request before it checks for shutdown.
const IDLE_INTERVAL: Duration = Duration::from_millis(10);
const SLOW_CONTROL_TIMEOUT: Duration = Duration::from_secs(30);

/// Processing-thread endpoint of the bounded worker control mailbox.
pub struct ProcessingControlEndpoint {
    commands: Consumer<ControlRequest>,
    replies: Producer<ControlResponse>,
}

impl ProcessingControlEndpoint {
    /// Reports queued control work without consuming it or blocking.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        !self.commands.is_empty()
    }

    /// Pops one validated request without blocking the processing thread.
    #[must_use]
    pub fn try_receive(&mut self) -> Option<ControlRequest> {
        self.commands.pop().ok()
    }

    /// Publishes one response without blocking the processing thread.
    ///
    /// # Errors
    ///
    /// Returns the unqueued response when the control thread has stopped consuming replies. The
    /// caller must retain worker health and continue processing audio rather than block.
    pub fn try_reply(&mut self, response: ControlResponse) -> Result<(), ControlResponse> {
        self.replies.push(response).map_err(|error| match error {
            rtrb::PushError::Full(response) => response,
        })
    }
}

/// Control-thread endpoint of the bounded worker control mailbox.
pub struct PipeControlEndpoint {
    commands: Producer<ControlRequest>,
    replies: Consumer<ControlResponse>,
    request_wake: Option<sp_shared_memory_os::SharedMemoryRegion>,
}

impl PipeControlEndpoint {
    /// Opens a separate mapping to wake the processing thread after publishing control work.
    ///
    /// # Errors
    /// Returns an error when the live worker bank cannot be mapped.
    pub fn set_request_wake(&mut self, bank_name: &str) -> io::Result<()> {
        self.request_wake = Some(sp_shared_memory_os::SharedMemoryRegion::open(bank_name)?);
        Ok(())
    }
}

/// Creates the two fixed SPSC channels connecting the control thread and processing thread.
#[must_use]
pub fn control_mailbox() -> (PipeControlEndpoint, ProcessingControlEndpoint) {
    let (command_producer, command_consumer) = RingBuffer::new(MAX_PENDING_CONTROL_REQUESTS);
    let (reply_producer, reply_consumer) = RingBuffer::new(MAX_PENDING_CONTROL_REQUESTS);
    (
        PipeControlEndpoint {
            commands: command_producer,
            replies: reply_consumer,
            request_wake: None,
        },
        ProcessingControlEndpoint {
            commands: command_consumer,
            replies: reply_producer,
        },
    )
}

/// Handle for the worker's dedicated control thread.
pub struct ControlThread {
    shutdown: Arc<AtomicBool>,
    join: Option<JoinHandle<io::Result<()>>>,
}

impl ControlThread {
    /// Returns whether the control thread has retired: the host closed the channel, sent a
    /// malformed frame, or a request missed its deadline.
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }

    /// Starts the control thread on the worker's request and response streams.
    ///
    /// A separate reader thread collects whole request frames, so the control thread can stop
    /// at any time without leaving part of a frame unread. There is one channel and no
    /// reconnection: when it closes or is corrupted, the worker shuts down.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero timeout or when a thread cannot start.
    pub fn spawn(
        requests: impl Read + Send + 'static,
        responses: impl Write + Send + 'static,
        target: ControlTarget,
        mailbox: PipeControlEndpoint,
        timeout: Duration,
    ) -> io::Result<Self> {
        if timeout.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "worker control timeout must be nonzero",
            ));
        }
        let frames = FrameReceiver::spawn(requests)?;
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);
        let join = thread::Builder::new()
            .name("sp-worker-control".to_owned())
            .spawn(move || {
                let result =
                    control_loop(&frames, responses, target, mailbox, timeout, &thread_shutdown);
                thread_shutdown.store(true, Ordering::Release);
                result
            })?;
        Ok(Self {
            shutdown,
            join: Some(join),
        })
    }

    /// Requests control-thread retirement and joins it.
    ///
    /// # Errors
    ///
    /// Returns an error if the control thread panicked or its streams failed.
    pub fn shutdown(mut self) -> io::Result<()> {
        self.shutdown.store(true, Ordering::Release);
        let Some(join) = self.join.take() else {
            return Ok(());
        };
        join.join()
            .map_err(|_| io::Error::other("worker control thread panicked"))?
    }
}

impl Drop for ControlThread {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
    }
}

fn control_loop(
    frames: &FrameReceiver,
    mut responses: impl Write,
    target: ControlTarget,
    mut mailbox: PipeControlEndpoint,
    timeout: Duration,
    shutdown: &AtomicBool,
) -> io::Result<()> {
    let mut gate = ControlRequestGate::new(target);
    while !shutdown.load(Ordering::Acquire) {
        let frame = match frames.wait(IDLE_INTERVAL) {
            FrameWait::Frame(frame) => frame,
            FrameWait::TimedOut => continue,
            // The host closed the channel (it stopped or dropped this worker) or broke framing.
            FrameWait::Closed => return Ok(()),
        };
        let request = gate
            .read_from(&mut frame.as_slice())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let shutdown_after_reply = request.operation() == ControlOperation::Shutdown;
        let response = forward_request(&request, &mut mailbox, timeout, shutdown)?;
        response
            .write_to(&mut responses)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        responses.flush()?;
        if shutdown_after_reply {
            return Ok(());
        }
    }
    Ok(())
}

fn forward_request(
    request: &ControlRequest,
    mailbox: &mut PipeControlEndpoint,
    timeout: Duration,
    shutdown: &AtomicBool,
) -> io::Result<ControlResponse> {
    if mailbox.commands.push(request.clone()).is_err() {
        return failure_response(
            request,
            ControlResponseStatus::Rejected,
            ControlErrorCode::UNAVAILABLE,
            "worker control queue is full",
        );
    }
    if let Some(region) = &mailbox.request_wake
        && !region.notify_request()
    {
        shutdown.store(true, Ordering::Release);
        return Err(io::Error::other(
            "could not wake worker for control request",
        ));
    }

    let operation_timeout = if matches!(
        request.operation(),
        ControlOperation::BeginStateCapture
            | ControlOperation::CommitStateRestore
            | ControlOperation::OpenNativeEditor
            | ControlOperation::CloseNativeEditor
    ) {
        SLOW_CONTROL_TIMEOUT
    } else {
        timeout
    };
    let deadline = Instant::now() + operation_timeout;
    loop {
        // The real reply is preferred even during shutdown: a `Shutdown` request races its own
        // orderly teardown, and the completed response may already be queued.
        if let Ok(response) = mailbox.replies.pop() {
            response
                .validate_for(request)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            return Ok(response);
        }
        if shutdown.load(Ordering::Acquire) {
            // `ShuttingDown` is a success status and must not carry an error record.
            return ControlResponse::shutting_down(
                request.request_id(),
                request.target(),
                request.slot(),
            )
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
        }
        if Instant::now() >= deadline {
            // A late reply would otherwise be mistaken for the next request on this mailbox.
            // Retire the worker instead of reusing a poisoned request/reply sequence.
            shutdown.store(true, Ordering::Release);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "worker did not complete the control request before its deadline",
            ));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn failure_response(
    request: &ControlRequest,
    status: ControlResponseStatus,
    code: ControlErrorCode,
    message: &'static str,
) -> io::Result<ControlResponse> {
    let error = ControlErrorRecord::new(code, message)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    ControlResponse::error(
        request.request_id(),
        request.target(),
        request.slot(),
        status,
        error,
    )
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Handles one validated request on the dedicated processing thread.
///
/// Implementations own the helper-safe VST3 facade and fixed serial rack. They must not perform
/// control I/O; [`ProcessingControlEndpoint`] carries all control-thread communication.
pub trait ProcessingControlHandler {
    /// Produces exactly one response for the received request.
    fn handle_control(&mut self, request: ControlRequest) -> ControlResponse;
}

#[cfg(test)]
mod tests {
    use super::*;
    use sp_protocol::control::{
        BankIdentity, ControlOperation, ControlRequestId, RackIdentity, SlotIdentity,
    };

    fn target() -> ControlTarget {
        ControlTarget::new(
            RackIdentity::new(0, 1).expect("rack"),
            BankIdentity::new(0, 1).expect("bank"),
        )
    }

    #[test]
    fn full_mailbox_returns_busy_without_losing_the_processing_endpoint() {
        let (mut pipe, mut processing) = control_mailbox();
        for request_id in 1..=u64::try_from(MAX_PENDING_CONTROL_REQUESTS).expect("capacity") {
            pipe
                .commands
                .push(
                    ControlRequest::new(
                        ControlRequestId::new(request_id).expect("request ID"),
                        target(),
                        ControlOperation::QueryHealth,
                        None,
                        &[],
                    )
                    .expect("valid request"),
                )
                .expect("fixed queue accepts capacity entries");
        }
        assert!(
            pipe
                .commands
                .push(
                    ControlRequest::new(
                        ControlRequestId::new(99).expect("request ID"),
                        target(),
                        ControlOperation::OpenNativeEditor,
                        Some(SlotIdentity::new(1).expect("slot")),
                        &[],
                    )
                    .expect("valid request"),
                )
                .is_err()
        );
        assert_eq!(
            processing
                .try_receive()
                .map(|request| request.request_id().get()),
            Some(1)
        );
    }
}
