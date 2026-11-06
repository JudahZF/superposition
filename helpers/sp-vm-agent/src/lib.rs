//! The guest agent of Lambda's Windows VM, and its protocol with the host.
//!
//! The agent runs in the logged-on user's session, so the workers it starts can show their
//! editors on the VM screen. It has one virtio-serial port of its own (the agent port) and
//! one per VM port (the worker ports). On the agent port the host sends [`Request`]s and the
//! agent sends [`Event`]s, one JSON object per line. A worker port is a byte relay: the
//! host's worker control session on one side, the worker's stdin and stdout on the other.
//!
//! One worker runs per VM port. [`Request::Stop`] ends a worker and waits up to
//! [`STOP_TIMEOUT`] for its process to exit: the IVSHMEM driver frees the port only then.

mod agent;

pub use agent::{Agent, AgentConfig, PortIo};

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// How long [`Request::Stop`] waits for a worker process to exit.
pub const STOP_TIMEOUT: Duration = Duration::from_millis(500);
/// How often the agent sends [`Event::Heartbeat`] and looks for exited workers.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);

/// A host request on the agent port.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Asks for [`Event::Hello`].
    Hello,
    /// Starts a worker on `port` with `args`, after ending any worker there.
    Start {
        /// The VM port.
        port: u32,
        /// The worker's arguments.
        args: Vec<String>,
    },
    /// Ends the worker on `port`.
    Stop {
        /// The VM port.
        port: u32,
    },
}

/// An agent event on the agent port.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// The agent's image and its running workers.
    Hello {
        /// The ID of the D: image that holds the agent and the workers.
        image_id: String,
        /// The ports that have a worker.
        workers: Vec<u32>,
    },
    /// A worker runs on `port`.
    Started {
        /// The VM port.
        port: u32,
        /// The worker's process ID.
        pid: u32,
    },
    /// The answer to [`Request::Stop`], or to a start that had to end an old worker.
    Stopped {
        /// The VM port.
        port: u32,
        /// False if the process did not exit within [`STOP_TIMEOUT`]: the port is dead.
        exited: bool,
    },
    /// A worker exited by itself.
    Exited {
        /// The VM port.
        port: u32,
        /// Its exit code, if it has one.
        code: Option<i32>,
    },
    /// The agent is alive. `sequence` counts up from 1.
    Heartbeat {
        /// The heartbeat count.
        sequence: u64,
    },
    /// A request failed.
    Error {
        /// The VM port, if the request had one.
        port: Option<u32>,
        /// What went wrong.
        message: String,
    },
}
