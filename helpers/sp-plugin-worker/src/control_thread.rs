//! Worker-side Unix-domain control transport and bounded processing-thread mailbox.
//!
//! The Unix socket is owned by a control thread. That thread never calls plug-ins or touches the
//! shared-memory audio payload. It forwards only validated bounded [`ControlRequest`] values to
//! the dedicated processing thread through fixed-capacity SPSC queues and writes the matching
//! [`ControlResponse`].

use std::{
    io,
    os::unix::net::{UnixListener, UnixStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use rtrb::{Consumer, Producer, RingBuffer};
use sp_protocol::control::{
    ControlErrorCode, ControlErrorRecord, ControlOperation, ControlProtocolError, ControlRequest,
    ControlRequestGate, ControlResponse, ControlResponseStatus, ControlTarget,
    MAX_CONTROL_FRAME_BYTES, MAX_PENDING_CONTROL_REQUESTS,
};

const POLL_INTERVAL: Duration = Duration::from_millis(1);

/// Processing-thread endpoint of the bounded worker control mailbox.
pub struct ProcessingControlEndpoint {
    commands: Consumer<ControlRequest>,
    replies: Producer<ControlResponse>,
}

impl ProcessingControlEndpoint {
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
pub struct SocketControlEndpoint {
    commands: Producer<ControlRequest>,
    replies: Consumer<ControlResponse>,
}

/// Creates the two fixed SPSC channels connecting a Unix control thread and processing thread.
#[must_use]
pub fn control_mailbox() -> (SocketControlEndpoint, ProcessingControlEndpoint) {
    let (command_producer, command_consumer) = RingBuffer::new(MAX_PENDING_CONTROL_REQUESTS);
    let (reply_producer, reply_consumer) = RingBuffer::new(MAX_PENDING_CONTROL_REQUESTS);
    (
        SocketControlEndpoint {
            commands: command_producer,
            replies: reply_consumer,
        },
        ProcessingControlEndpoint {
            commands: command_consumer,
            replies: reply_producer,
        },
    )
}

/// Handle for the worker's dedicated Unix-domain control thread.
pub struct ControlThread {
    shutdown: Arc<AtomicBool>,
    join: Option<JoinHandle<io::Result<()>>>,
}

impl ControlThread {
    /// Starts the supplied bound Unix listener on a separate control thread.
    ///
    /// The listener is configured nonblocking only so orderly shutdown can be observed; each
    /// accepted stream still has a finite read/write timeout.
    ///
    /// # Errors
    ///
    /// Returns an error when the listener cannot be configured for finite shutdown polling.
    pub fn spawn(
        listener: UnixListener,
        target: ControlTarget,
        mailbox: SocketControlEndpoint,
        timeout: Duration,
    ) -> io::Result<Self> {
        if timeout.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "worker control timeout must be nonzero",
            ));
        }
        listener.set_nonblocking(true)?;
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);
        let join = thread::Builder::new()
            .name("sp-worker-control".to_owned())
            .spawn(move || control_loop(listener, target, mailbox, timeout, thread_shutdown))?;
        Ok(Self {
            shutdown,
            join: Some(join),
        })
    }

    /// Requests control-thread retirement and joins it.
    ///
    /// # Errors
    ///
    /// Returns an error if the control thread panicked or its listener/stream failed.
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

// `listener` and the shutdown handle are intentionally moved into the dedicated control thread.
#[allow(clippy::needless_pass_by_value)]
fn control_loop(
    listener: UnixListener,
    target: ControlTarget,
    mut mailbox: SocketControlEndpoint,
    timeout: Duration,
    shutdown: Arc<AtomicBool>,
) -> io::Result<()> {
    while !shutdown.load(Ordering::Acquire) {
        match listener.accept() {
            // A protocol violation or I/O failure closes only that connection: the listener
            // must survive so a recovering application can reconnect to a healthy worker.
            Ok((stream, _)) => {
                let _ = serve_connection(stream, target, &mut mailbox, timeout, &shutdown);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => thread::sleep(POLL_INTERVAL),
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn serve_connection(
    mut stream: UnixStream,
    target: ControlTarget,
    mailbox: &mut SocketControlEndpoint,
    timeout: Duration,
    shutdown: &AtomicBool,
) -> io::Result<()> {
    // On macOS the accepted stream inherits the listener's nonblocking flag. Framed reads must
    // block: a nonblocking `read_exact` can consume a frame's length prefix, hit `EAGAIN`
    // before the header arrives, and silently discard the consumed bytes, desynchronizing the
    // stream for every later request.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let mut gate = ControlRequestGate::new(target);
    while !shutdown.load(Ordering::Acquire) {
        // The frame is accumulated byte-exactly before parsing: an idle timeout between
        // frames retries without losing bytes, while a stall after a frame has begun closes
        // the connection, because a partially read frame can never be re-aligned.
        let Some(frame) = read_complete_frame(&mut stream, shutdown)? else {
            return Ok(());
        };
        let request = match gate.read_from(&mut frame.as_slice()) {
            Ok(request) => request,
            Err(ControlProtocolError::Io(_)) => return Ok(()),
            Err(error) => return Err(io::Error::new(io::ErrorKind::InvalidData, error)),
        };
        let shutdown_after_reply = request.operation() == ControlOperation::Shutdown;
        let response = forward_request(&request, mailbox, timeout, shutdown)?;
        response
            .write_to(&mut stream)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if shutdown_after_reply {
            shutdown.store(true, Ordering::Release);
            return Ok(());
        }
    }
    Ok(())
}

/// Reads one complete length-prefixed control frame, including its prefix.
///
/// Returns `Ok(None)` when the connection should close quietly: the peer disconnected, stalled
/// after starting a frame, or declared an impossible length. Idle timeouts while waiting for a
/// new frame simply retry, pacing the loop for shutdown checks without discarding bytes — the
/// failure mode that previously desynchronized the stream and surfaced as `UnexpectedEof` in
/// the application.
fn read_complete_frame(
    stream: &mut UnixStream,
    shutdown: &AtomicBool,
) -> io::Result<Option<Vec<u8>>> {
    use std::io::Read;
    const PREFIX_BYTES: usize = 4;
    let mut prefix = [0_u8; PREFIX_BYTES];
    let mut have = 0_usize;
    while have < PREFIX_BYTES {
        if shutdown.load(Ordering::Acquire) {
            return Ok(None);
        }
        match stream.read(&mut prefix[have..]) {
            Ok(0) => return Ok(None),
            Ok(read) => have += read,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                if have > 0 {
                    return Ok(None);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    let Ok(length) = usize::try_from(u32::from_le_bytes(prefix)) else {
        return Ok(None);
    };
    if length > MAX_CONTROL_FRAME_BYTES {
        return Ok(None);
    }
    let mut frame = vec![0_u8; PREFIX_BYTES + length];
    frame[..PREFIX_BYTES].copy_from_slice(&prefix);
    let mut filled = PREFIX_BYTES;
    while filled < frame.len() {
        if shutdown.load(Ordering::Acquire) {
            return Ok(None);
        }
        match stream.read(&mut frame[filled..]) {
            Ok(0) => return Ok(None),
            Ok(read) => filled += read,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                return Ok(None);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(Some(frame))
}

fn forward_request(
    request: &ControlRequest,
    mailbox: &mut SocketControlEndpoint,
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

    let deadline = Instant::now() + timeout;
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
            return failure_response(
                request,
                ControlResponseStatus::Rejected,
                ControlErrorCode::UNAVAILABLE,
                "worker processing thread did not complete control request before timeout",
            );
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
/// Unix socket I/O; [`ProcessingControlEndpoint`] carries all control-thread communication.
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
        let (mut socket, mut processing) = control_mailbox();
        for request_id in 1..=u64::try_from(MAX_PENDING_CONTROL_REQUESTS).expect("capacity") {
            socket
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
            socket
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
