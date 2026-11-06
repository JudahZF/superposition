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
/// How long one bundle's scan may take, as for a Windows scan under Wine.
pub const SCAN_TIMEOUT: Duration = Duration::from_secs(30);
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
        /// The port's real-time vCPU, for the worker's audio thread (`SP_VM_AUDIO_CPU`).
        #[serde(default)]
        audio_cpu: Option<u32>,
    },
    /// Ends the worker on `port`.
    Stop {
        /// The VM port.
        port: u32,
    },
    /// Scans every VST3 bundle in the guest's VST3 folder, one scanner process each. The
    /// agent answers with one [`Event::Scanned`] per bundle, then [`Event::ScanDone`].
    Scan,
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
    /// One bundle's scan.
    Scanned {
        /// The bundle's guest path.
        bundle: String,
        /// SHA-256 (hex) over each file's relative path, size and modification time: it
        /// changes when the bundle is installed again.
        fingerprint: String,
        /// The scanner's exit code, or `None` if it timed out or had none.
        code: Option<i32>,
        /// True if the scanner ran past [`SCAN_TIMEOUT`] and was ended.
        timed_out: bool,
        /// The scanner's report (JSON), if it wrote one.
        report: Option<String>,
    },
    /// The scan has ended.
    ScanDone {
        /// The bundles scanned.
        bundles: u32,
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

/// One JSON line, with its newline: the wire form of a [`Request`] or an [`Event`].
fn line(value: &impl Serialize) -> Vec<u8> {
    // The messages have only strings, integers and lists: serializing cannot fail.
    let mut line = serde_json::to_vec(value).unwrap_or_default();
    line.push(b'\n');
    line
}

impl Request {
    /// The request as one line for the agent port.
    #[must_use]
    pub fn to_line(&self) -> Vec<u8> {
        line(self)
    }

    /// Parses one line from the agent port.
    ///
    /// # Errors
    ///
    /// Returns the parse error as text.
    pub fn from_line(line: &str) -> Result<Self, String> {
        serde_json::from_str(line).map_err(|error| error.to_string())
    }
}

impl Event {
    /// The event as one line for the agent port.
    #[must_use]
    pub fn to_line(&self) -> Vec<u8> {
        line(self)
    }

    /// Parses one line from the agent port.
    ///
    /// # Errors
    ///
    /// Returns the parse error as text.
    pub fn from_line(line: &str) -> Result<Self, String> {
        serde_json::from_str(line).map_err(|error| error.to_string())
    }
}
