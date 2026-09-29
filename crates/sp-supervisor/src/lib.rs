//! Process supervision for isolated helper binaries.

mod catalog;
mod quarantine;
mod scanner;
mod worker_control;

pub use catalog::{CATALOG_SCHEMA_VERSION, PluginCatalog};
pub use quarantine::{
    PersistentQuarantine, PluginFailureKind, QuarantineRecord, QuarantineTable, QuarantinedPlugin,
};
pub use scanner::{
    BundleFingerprint, CachedScan, ScanCache, ScanDescriptor, ScanStatus, Scanner,
    discover_vst3_bundles, fingerprint_bundle,
};
pub use worker_control::{
    EditorPreviewPoll, WorkerControlClient, WorkerControlError, WorkerControlLaunch,
    WorkerControlSession,
};

use std::{
    collections::HashMap,
    io::{Read, Write},
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

/// Maximum stdout or stderr retained from a disposable helper process.
const MAX_CAPTURED_HELPER_OUTPUT_BYTES: usize = 1024 * 1024;

/// The helper executable role managed by the application.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HelperKind {
    /// Hosts a plug-in instance outside the application process.
    PluginWorker,
    /// Inspects plug-in bundles outside the application process.
    PluginScanner,
}

impl HelperKind {
    /// Returns the deployment filename allowed for this helper role.
    #[must_use]
    pub const fn executable_name(self) -> &'static str {
        match self {
            Self::PluginWorker => "sp-plugin-worker",
            Self::PluginScanner => "sp-plugin-scanner",
        }
    }
}

/// Declarative request to start a helper process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HelperLaunch {
    /// Role performed by the child process.
    pub kind: HelperKind,
    /// Executable selected by deployment configuration.
    pub executable: PathBuf,
    /// Arguments passed without shell expansion.
    pub arguments: Vec<String>,
}

impl HelperLaunch {
    /// Validates that a control-plane launch targets its dedicated deployed helper.
    ///
    /// The generic process supervisor also serves development tooling, so this check is
    /// deliberately opt-in for product worker lifecycle code rather than imposed on every helper
    /// invocation.
    /// It rejects PATH lookup and role/executable mismatches before the supervisor can create a
    /// process capable of loading third-party plug-in code.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` unless `executable` is an absolute path whose filename exactly
    /// matches the selected helper role.
    pub fn validate_helper_only(&self) -> std::io::Result<()> {
        if !self.executable.is_absolute() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "helper executable must be an absolute deployment path",
            ));
        }
        let actual_name = self.executable.file_name().and_then(|name| name.to_str());
        if actual_name != Some(self.kind.executable_name()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "helper role requires executable `{}`, not `{}`",
                    self.kind.executable_name(),
                    self.executable.display()
                ),
            ));
        }
        Ok(())
    }
}

/// Observable lifecycle state of a helper process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HelperStatus {
    /// The helper has not been started or was stopped deliberately.
    Stopped,
    /// The helper is alive and serving requests.
    Running,
    /// The helper terminated unexpectedly.
    Failed,
}

/// Platform-neutral contract for managing isolated helper processes.
pub trait HelperSupervisor {
    /// Starts a helper and returns an opaque process identifier.
    ///
    /// # Errors
    ///
    /// Returns an error when the helper cannot be started.
    fn launch(
        &mut self,
        launch: &HelperLaunch,
    ) -> Result<u64, Box<dyn std::error::Error + Send + Sync>>;
    /// Returns the current state for a previously launched helper.
    fn status(&self, process_id: u64) -> HelperStatus;
    /// Requests orderly termination of a helper.
    ///
    /// # Errors
    ///
    /// Returns an error when the termination request cannot be delivered.
    fn stop(&mut self, process_id: u64) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

/// Limits automatic restarts after a helper exits.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RestartPolicy {
    /// Number of replacement processes permitted after the original launch.
    pub max_restarts: u32,
    /// Minimum wait before each replacement launch.
    pub cooldown: Duration,
}

/// Identifies the worker currently assigned to a rack.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RackWorkerHandle {
    /// Index of the rack in its session.
    pub rack_index: usize,
    /// Monotonically increasing worker generation for this rack.
    pub generation: u64,
    /// Operating-system process identifier.
    pub process_id: u64,
    /// Bank selected for this worker.
    pub bank_name: String,
}

/// Tracks the live worker handle for each rack index.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RackWorkerTable {
    handles: HashMap<usize, RackWorkerHandle>,
    suspected_slot_bypass: HashMap<usize, Option<usize>>,
    generations: HashMap<usize, u64>,
}

impl RackWorkerTable {
    /// Creates an empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts or replaces the handle for a rack and returns the previous value.
    pub fn assign(&mut self, handle: RackWorkerHandle) -> Option<RackWorkerHandle> {
        self.generations
            .insert(handle.rack_index, handle.generation);
        self.handles.insert(handle.rack_index, handle)
    }

    /// Returns the handle currently assigned to `rack_index`.
    #[must_use]
    pub fn get(&self, rack_index: usize) -> Option<&RackWorkerHandle> {
        self.handles.get(&rack_index)
    }

    /// Removes and returns the handle for `rack_index`.
    pub fn clear(&mut self, rack_index: usize) -> Option<RackWorkerHandle> {
        self.suspected_slot_bypass.remove(&rack_index);
        self.handles.remove(&rack_index)
    }

    /// Returns whether every listed rack index currently has a handle.
    #[must_use]
    pub fn covers(&self, rack_indexes: impl IntoIterator<Item = usize>) -> bool {
        rack_indexes
            .into_iter()
            .all(|rack_index| self.handles.contains_key(&rack_index))
    }

    /// Returns the number of assigned racks.
    #[must_use]
    pub fn len(&self) -> usize {
        self.handles.len()
    }

    /// Returns true when no racks are assigned.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.handles.is_empty()
    }

    /// Returns the rack indexes that currently have assigned workers.
    #[must_use]
    pub fn rack_indexes(&self) -> Vec<usize> {
        self.handles.keys().copied().collect()
    }

    /// Records the plug-in slot suspected of causing the last fault on `rack_index`.
    pub fn set_suspected_slot_bypass(&mut self, rack_index: usize, slot_index: Option<usize>) {
        self.suspected_slot_bypass.insert(rack_index, slot_index);
    }

    /// Returns the suspected slot that recovery should bypass for `rack_index`.
    #[must_use]
    pub fn suspected_slot_bypass(&self, rack_index: usize) -> Option<usize> {
        self.suspected_slot_bypass
            .get(&rack_index)
            .copied()
            .flatten()
    }

    /// Returns the next worker generation for `rack_index`.
    #[must_use]
    pub fn next_generation(&self, rack_index: usize) -> u64 {
        self.generations
            .get(&rack_index)
            .copied()
            .unwrap_or(0)
            .saturating_add(1)
            .max(1)
    }

    /// Alternates dual-bank names for a replacement worker.
    #[must_use]
    pub fn alternate_bank_name(current: &str) -> String {
        if current.ends_with("-a") {
            format!("{}b", &current[..current.len().saturating_sub(1)])
        } else if current.ends_with("-b") {
            format!("{}a", &current[..current.len().saturating_sub(1)])
        } else {
            format!("{current}-b")
        }
    }
}

/// Product control-plane loop over [`ProcessSupervisor`] and [`RackWorkerTable`].
#[derive(Debug, Default)]
pub struct RackSupervisor {
    table: RackWorkerTable,
}

impl RackSupervisor {
    /// Creates an empty rack supervisor.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the worker table.
    #[must_use]
    pub const fn table(&self) -> &RackWorkerTable {
        &self.table
    }

    /// Launches a worker for `rack_index`, replacing any previous assignment.
    ///
    /// This is the low-level launch step. Production VST3 worker launches must use
    /// [`Self::create_or_replace_plugin_worker`] so a quarantined bundle is refused before a
    /// plug-in-loading process can be spawned.
    ///
    /// # Errors
    ///
    /// Returns an error when the helper cannot be started or the previous worker cannot stop.
    pub fn create_or_replace_worker(
        &mut self,
        processes: &mut ProcessSupervisor,
        rack_index: usize,
        bank_name: impl Into<String>,
        launch: &HelperLaunch,
        suspected_slot: Option<usize>,
    ) -> std::io::Result<RackWorkerHandle> {
        if let Some(previous) = self.table.clear(rack_index) {
            let _ = processes.stop(previous.process_id);
        }
        let process_id = processes.launch(launch)?;
        let handle = RackWorkerHandle {
            rack_index,
            generation: self.table.next_generation(rack_index),
            process_id,
            bank_name: bank_name.into(),
        };
        self.table
            .set_suspected_slot_bypass(rack_index, suspected_slot);
        self.table.assign(handle.clone());
        Ok(handle)
    }

    /// Refuses a quarantined plug-in before it can spawn a plug-in-loading worker helper.
    ///
    /// This is the production worker-launch API. It validates the deployed helper role and the
    /// atomically persisted quarantine record before it stops a previous worker or invokes
    /// `Command::spawn`.
    ///
    /// # Errors
    ///
    /// Returns `PermissionDenied` when the fingerprint is quarantined, or an error when helper
    /// validation, process launch, or worker replacement fails.
    #[allow(
        clippy::too_many_arguments,
        reason = "the launch contract names every safety input explicitly at the call site"
    )]
    pub fn create_or_replace_plugin_worker(
        &mut self,
        processes: &mut ProcessSupervisor,
        rack_index: usize,
        bank_name: impl Into<String>,
        launch: &HelperLaunch,
        suspected_slot: Option<usize>,
        fingerprint: &BundleFingerprint,
        quarantine: &PersistentQuarantine,
    ) -> std::io::Result<RackWorkerHandle> {
        quarantine
            .ensure_launch_permitted(fingerprint)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::PermissionDenied, error))?;
        if launch.kind != HelperKind::PluginWorker {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "plug-in launches must use the plug-in worker helper",
            ));
        }
        launch.validate_helper_only()?;
        self.create_or_replace_worker(processes, rack_index, bank_name, launch, suspected_slot)
    }

    /// Records one worker crash, timeout, or hang and atomically persists the failure counter.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` for scanner failure kinds or an error when state cannot persist.
    pub fn record_worker_failure(
        &self,
        quarantine: &mut PersistentQuarantine,
        fingerprint: BundleFingerprint,
        kind: PluginFailureKind,
    ) -> std::io::Result<bool> {
        if !matches!(
            kind,
            PluginFailureKind::WorkerCrash
                | PluginFailureKind::WorkerTimeout
                | PluginFailureKind::WorkerHang
        ) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "record_worker_failure requires a worker failure kind",
            ));
        }
        quarantine.record_failure(fingerprint, kind)
    }

    /// Dual-bank recovery: stop the live worker, launch on the alternate bank, bypass a slot.
    ///
    /// This legacy control-plane operation does not persist a plug-in failure because it has no
    /// canonical fingerprint. Product worker recovery must call [`Self::record_worker_failure`]
    /// with the exact fingerprint observed by the worker.
    ///
    /// # Errors
    ///
    /// Returns an error when launch/stop fails.
    pub fn recover_with_dual_bank(
        &mut self,
        processes: &mut ProcessSupervisor,
        rack_index: usize,
        mut launch: HelperLaunch,
        suspected_slot: Option<usize>,
        _fingerprint: Option<&str>,
    ) -> std::io::Result<RackWorkerHandle> {
        let previous = self.table.get(rack_index).cloned();
        let bank_name = previous.as_ref().map_or_else(
            || format!("rack-{rack_index}-a"),
            |handle| RackWorkerTable::alternate_bank_name(&handle.bank_name),
        );
        // Replace bank argument placeholders if present.
        for argument in &mut launch.arguments {
            if argument.starts_with("bank-") || argument.contains("rack-") {
                // Prefer rewriting only when caller used the previous bank name.
                if let Some(previous) = &previous
                    && argument == &previous.bank_name
                {
                    argument.clone_from(&bank_name);
                }
            }
        }
        let handle = self.create_or_replace_worker(
            processes,
            rack_index,
            bank_name,
            &launch,
            suspected_slot,
        )?;
        let _ = processes.reap();
        Ok(handle)
    }

    /// Marks workers dead according to `is_alive` and returns their rack indexes.
    pub fn poll_heartbeats(
        &mut self,
        mut is_alive: impl FnMut(&RackWorkerHandle) -> bool,
    ) -> Vec<usize> {
        let mut dead = Vec::new();
        for rack_index in self.table.rack_indexes() {
            if let Some(handle) = self.table.get(rack_index)
                && !is_alive(handle)
            {
                dead.push(rack_index);
            }
        }
        for rack_index in &dead {
            let _ = self.table.clear(*rack_index);
        }
        dead
    }
}

/// Result of [`ProcessSupervisor::launch_and_wait`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TimedHelperResult {
    /// The helper exited before the timeout.
    Exited {
        /// Operating-system process identifier.
        process_id: u64,
        /// Platform exit code, or -1 when unavailable.
        code: i32,
    },
    /// The helper was killed after exceeding the timeout.
    TimedOut {
        /// Operating-system process identifier.
        process_id: u64,
    },
}

/// Captured stdio from [`ProcessSupervisor::launch_and_wait_capturing`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapturedHelperOutput {
    /// Timeout or exit outcome.
    pub result: TimedHelperResult,
    /// Bytes written to the helper's stdout.
    pub stdout: Vec<u8>,
    /// Bytes written to the helper's stderr.
    pub stderr: Vec<u8>,
}

/// A lifecycle transition observed by [`ProcessSupervisor`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SupervisorEvent {
    /// A helper was launched.
    Started,
    /// A helper exited with its platform exit code, or -1 when unavailable.
    Exited {
        /// Platform exit code, or -1 when the platform does not provide one.
        code: i32,
    },
    /// A replacement launch will occur after the configured cooldown.
    RestartScheduled,
    /// No further replacement launches are permitted.
    RestartExhausted,
    /// A helper was deliberately stopped.
    Stopped,
}

struct ManagedChild {
    child: Child,
    launch: HelperLaunch,
    restarts: u32,
    stop_requested: bool,
}

struct PendingRestart {
    launch: HelperLaunch,
    restarts: u32,
    at: Instant,
}

/// Local implementation backed by [`std::process::Child`].
pub struct ProcessSupervisor {
    children: HashMap<u64, ManagedChild>,
    statuses: HashMap<u64, HelperStatus>,
    pending_restarts: Vec<PendingRestart>,
    events: Vec<SupervisorEvent>,
    restart_policy: Option<RestartPolicy>,
    shutdown_line: Option<String>,
    shutdown_timeout: Duration,
}

impl ProcessSupervisor {
    /// Creates a supervisor without automatic restarts or a stdin shutdown command.
    pub fn new() -> Self {
        Self {
            children: HashMap::new(),
            statuses: HashMap::new(),
            pending_restarts: Vec::new(),
            events: Vec::new(),
            restart_policy: None,
            shutdown_line: None,
            shutdown_timeout: Duration::from_millis(100),
        }
    }

    /// Enables replacement launches after unexpected exits.
    #[must_use]
    pub fn with_restart_policy(mut self, restart_policy: RestartPolicy) -> Self {
        self.restart_policy = Some(restart_policy);
        self
    }

    /// Sends this line to a child's stdin before forcefully stopping it.
    #[must_use]
    pub fn with_shutdown_line(mut self, shutdown_line: impl Into<String>) -> Self {
        self.shutdown_line = Some(shutdown_line.into());
        self
    }

    /// Sets how long a graceful stdin shutdown may take before the child is killed.
    #[must_use]
    pub fn with_shutdown_timeout(mut self, shutdown_timeout: Duration) -> Self {
        self.shutdown_timeout = shutdown_timeout;
        self
    }

    /// Starts a local child process.
    ///
    /// # Errors
    ///
    /// Returns an error when the executable cannot be spawned.
    pub fn launch(&mut self, launch: &HelperLaunch) -> std::io::Result<u64> {
        self.launch_child(launch.clone(), 0)
    }

    /// Returns the known state for a process ID.
    pub fn status(&self, process_id: u64) -> HelperStatus {
        self.statuses
            .get(&process_id)
            .copied()
            .unwrap_or(HelperStatus::Stopped)
    }

    /// Returns whether a known child has been reaped and no longer owns process resources.
    ///
    /// A child that exited unexpectedly is also reaped, even though its status is `Failed`.
    /// Unknown process IDs return false.
    #[must_use]
    pub fn is_reaped(&self, process_id: u64) -> bool {
        self.statuses.contains_key(&process_id) && !self.children.contains_key(&process_id)
    }

    /// Requests immediate child termination without waiting for process exit.
    ///
    /// The child remains owned by this supervisor. Call [`Self::reap`] until [`Self::is_reaped`]
    /// returns true before reusing resources the child could still access.
    /// Deliberately stopped children do not trigger the automatic restart policy.
    ///
    /// # Errors
    ///
    /// Returns an error when the operating system rejects the kill request.
    pub fn request_stop(&mut self, process_id: u64) -> std::io::Result<()> {
        let Some(managed) = self.children.get_mut(&process_id) else {
            return Ok(());
        };
        if managed.stop_requested {
            return Ok(());
        }
        if managed.child.try_wait()?.is_some() {
            return Ok(());
        }
        match managed.child.kill() {
            Ok(()) => {
                managed.stop_requested = true;
                Ok(())
            }
            Err(error) => {
                if managed.child.try_wait()?.is_some() {
                    Ok(())
                } else {
                    Err(error)
                }
            }
        }
    }

    /// Stops a child, first using the configured stdin shutdown line when present.
    /// The child remains owned if termination or reaping fails. A broken shutdown pipe is
    /// reported after the child has been killed and reaped.
    ///
    /// # Errors
    ///
    /// Returns an error when communicating with or terminating the child fails.
    pub fn stop(&mut self, process_id: u64) -> std::io::Result<()> {
        let Some(managed) = self.children.get_mut(&process_id) else {
            return Ok(());
        };
        let mut graceful_error = None;
        if !managed.stop_requested
            && let (Some(line), Some(stdin)) = (&self.shutdown_line, managed.child.stdin.as_mut())
            && let Err(error) = stdin
                .write_all(line.as_bytes())
                .and_then(|()| stdin.flush())
        {
            graceful_error = Some(error);
        }
        let mut exited = false;
        if graceful_error.is_none() {
            let deadline = Instant::now() + self.shutdown_timeout;
            while Instant::now() < deadline {
                if managed.child.try_wait()?.is_some() {
                    exited = true;
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
        }
        if !exited {
            if let Err(error) = managed.child.kill()
                && managed.child.try_wait()?.is_none()
            {
                return Err(error);
            }
            managed.child.wait()?;
        }
        self.children.remove(&process_id);
        self.statuses.insert(process_id, HelperStatus::Stopped);
        self.events.push(SupervisorEvent::Stopped);
        graceful_error.map_or(Ok(()), Err)
    }

    /// Launches a helper and waits until it exits or `timeout` elapses.
    ///
    /// On timeout the child is killed and [`TimedHelperResult::TimedOut`] is returned.
    ///
    /// # Errors
    ///
    /// Returns an error when the helper cannot be started or terminated.
    pub fn launch_and_wait(
        &mut self,
        launch: &HelperLaunch,
        timeout: Duration,
    ) -> std::io::Result<TimedHelperResult> {
        let process_id = self.launch(launch)?;
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(managed) = self.children.get_mut(&process_id)
                && let Some(status) = managed.child.try_wait()?
            {
                self.children.remove(&process_id);
                self.statuses.insert(process_id, HelperStatus::Failed);
                let code = status.code().unwrap_or(-1);
                self.events.push(SupervisorEvent::Exited { code });
                return Ok(TimedHelperResult::Exited { process_id, code });
            }
            if Instant::now() >= deadline {
                self.stop(process_id)?;
                return Ok(TimedHelperResult::TimedOut { process_id });
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Runs a disposable helper with all stdio detached and enforces `timeout`.
    ///
    /// This avoids waiting for pipe EOF when third-party code spawns a descendant that keeps a
    /// standard stream open after the helper exits.
    ///
    /// # Errors
    ///
    /// Returns an error when the helper cannot be started or polled.
    pub fn launch_and_wait_silenced(
        &mut self,
        launch: &HelperLaunch,
        timeout: Duration,
    ) -> std::io::Result<TimedHelperResult> {
        let mut command = Command::new(&launch.executable);
        command.args(&launch.arguments);
        command.stdin(Stdio::null());
        command.stdout(Stdio::null());
        command.stderr(Stdio::null());
        let mut child = command.spawn()?;
        let process_id = u64::from(child.id());
        self.statuses.insert(process_id, HelperStatus::Running);
        self.events.push(SupervisorEvent::Started);

        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = child.try_wait()? {
                let code = status.code().unwrap_or(-1);
                self.statuses.insert(process_id, HelperStatus::Failed);
                self.events.push(SupervisorEvent::Exited { code });
                return Ok(TimedHelperResult::Exited { process_id, code });
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                self.statuses.insert(process_id, HelperStatus::Stopped);
                self.events.push(SupervisorEvent::Stopped);
                return Ok(TimedHelperResult::TimedOut { process_id });
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Like [`Self::launch_and_wait`], but captures stdout/stderr for disposable helpers.
    ///
    /// Retained for disposable helpers that need bounded diagnostic capture.
    ///
    /// # Errors
    ///
    /// Returns an error when the helper cannot be started, read, or terminated.
    pub fn launch_and_wait_capturing(
        &mut self,
        launch: &HelperLaunch,
        timeout: Duration,
    ) -> std::io::Result<CapturedHelperOutput> {
        let mut command = Command::new(&launch.executable);
        command.args(&launch.arguments);
        command.stdin(Stdio::null());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        let mut child = command.spawn()?;
        let process_id = u64::from(child.id());
        self.statuses.insert(process_id, HelperStatus::Running);
        self.events.push(SupervisorEvent::Started);

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdout_thread =
            thread::spawn(move || stdout.map_or_else(Vec::new, read_bounded_helper_output));
        let stderr_thread =
            thread::spawn(move || stderr.map_or_else(Vec::new, read_bounded_helper_output));

        let deadline = Instant::now() + timeout;
        let result = loop {
            if let Some(status) = child.try_wait()? {
                let code = status.code().unwrap_or(-1);
                self.statuses.insert(process_id, HelperStatus::Failed);
                self.events.push(SupervisorEvent::Exited { code });
                break TimedHelperResult::Exited { process_id, code };
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                self.statuses.insert(process_id, HelperStatus::Stopped);
                self.events.push(SupervisorEvent::Stopped);
                break TimedHelperResult::TimedOut { process_id };
            }
            thread::sleep(Duration::from_millis(5));
        };

        let stdout = stdout_thread.join().unwrap_or_default();
        let stderr = stderr_thread.join().unwrap_or_default();
        Ok(CapturedHelperOutput {
            result,
            stdout,
            stderr,
        })
    }

    /// Reaps exited children, starts due replacements, and returns accumulated events.
    pub fn reap(&mut self) -> Vec<SupervisorEvent> {
        self.start_due_restarts();
        let process_ids: Vec<u64> = self.children.keys().copied().collect();
        for process_id in process_ids {
            let exit = self
                .children
                .get_mut(&process_id)
                .and_then(|managed| managed.child.try_wait().ok().flatten());
            if let Some(exit) = exit
                && let Some(managed) = self.children.remove(&process_id)
            {
                if managed.stop_requested {
                    self.statuses.insert(process_id, HelperStatus::Stopped);
                    self.events.push(SupervisorEvent::Stopped);
                } else {
                    self.statuses.insert(process_id, HelperStatus::Failed);
                    self.events.push(SupervisorEvent::Exited {
                        code: exit.code().unwrap_or(-1),
                    });
                    self.schedule_restart(managed);
                }
            }
        }
        std::mem::take(&mut self.events)
    }

    fn launch_child(&mut self, launch: HelperLaunch, restarts: u32) -> std::io::Result<u64> {
        let mut command = Command::new(&launch.executable);
        command.args(&launch.arguments);
        command.stdin(if self.shutdown_line.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        let child = command.spawn()?;
        let process_id = u64::from(child.id());
        self.children.insert(
            process_id,
            ManagedChild {
                child,
                launch,
                restarts,
                stop_requested: false,
            },
        );
        self.statuses.insert(process_id, HelperStatus::Running);
        self.events.push(SupervisorEvent::Started);
        Ok(process_id)
    }

    fn schedule_restart(&mut self, managed: ManagedChild) {
        let Some(policy) = &self.restart_policy else {
            return;
        };
        if managed.restarts >= policy.max_restarts {
            self.events.push(SupervisorEvent::RestartExhausted);
            return;
        }
        self.pending_restarts.push(PendingRestart {
            launch: managed.launch,
            restarts: managed.restarts + 1,
            at: Instant::now() + policy.cooldown,
        });
        self.events.push(SupervisorEvent::RestartScheduled);
    }

    fn start_due_restarts(&mut self) {
        let now = Instant::now();
        let mut remaining = Vec::new();
        for restart in std::mem::take(&mut self.pending_restarts) {
            if restart.at > now {
                remaining.push(restart);
            } else if self.launch_child(restart.launch, restart.restarts).is_err() {
                self.events.push(SupervisorEvent::RestartExhausted);
            }
        }
        self.pending_restarts = remaining;
    }
}

fn read_bounded_helper_output(mut reader: impl Read) -> Vec<u8> {
    let mut retained = Vec::new();
    let mut buffer = [0_u8; 8 * 1024];
    while let Ok(read) = reader.read(&mut buffer) {
        if read == 0 {
            break;
        }
        let remaining = MAX_CAPTURED_HELPER_OUTPUT_BYTES.saturating_sub(retained.len());
        retained.extend_from_slice(&buffer[..read.min(remaining)]);
    }
    retained
}

impl Default for ProcessSupervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl HelperSupervisor for ProcessSupervisor {
    fn launch(
        &mut self,
        launch: &HelperLaunch,
    ) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
        Ok(ProcessSupervisor::launch(self, launch)?)
    }

    fn status(&self, process_id: u64) -> HelperStatus {
        ProcessSupervisor::status(self, process_id)
    }

    fn stop(&mut self, process_id: u64) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(ProcessSupervisor::stop(self, process_id)?)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        thread,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use super::{
        BundleFingerprint, HelperKind, HelperLaunch, HelperStatus, PersistentQuarantine,
        PluginFailureKind, ProcessSupervisor, QuarantineTable, RackSupervisor, RackWorkerHandle,
        RackWorkerTable, RestartPolicy, SupervisorEvent, TimedHelperResult,
    };

    fn launch(executable: &str, arguments: &[&str]) -> HelperLaunch {
        HelperLaunch {
            kind: HelperKind::PluginWorker,
            executable: PathBuf::from(executable),
            arguments: arguments.iter().map(ToString::to_string).collect(),
        }
    }

    #[test]
    fn helper_only_validation_requires_the_deployed_role_binary() {
        let worker = HelperLaunch {
            kind: HelperKind::PluginWorker,
            executable: PathBuf::from(
                "/Applications/Superposition.app/Contents/Helpers/sp-plugin-worker",
            ),
            arguments: Vec::new(),
        };
        assert!(worker.validate_helper_only().is_ok());

        let relative = HelperLaunch {
            executable: PathBuf::from("sp-plugin-worker"),
            ..worker.clone()
        };
        assert!(relative.validate_helper_only().is_err());

        let mismatched = HelperLaunch {
            kind: HelperKind::PluginScanner,
            ..worker
        };
        assert!(mismatched.validate_helper_only().is_err());
    }

    #[test]
    fn plugin_worker_launch_is_rejected_before_spawn_when_fingerprint_is_quarantined() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let state_path = std::env::temp_dir().join(format!("sp-launch-quarantine-{unique}.json"));
        let fingerprint = BundleFingerprint {
            algorithm: "sha256".to_owned(),
            digest: "quarantined".to_owned(),
        };
        let mut quarantine = PersistentQuarantine::open(&state_path, 1).expect("open quarantine");
        quarantine
            .record_failure(fingerprint.clone(), PluginFailureKind::WorkerCrash)
            .expect("persist failure");

        let mut processes = ProcessSupervisor::new();
        let mut racks = RackSupervisor::new();
        let error = racks
            .create_or_replace_plugin_worker(
                &mut processes,
                0,
                "rack-0-a",
                &HelperLaunch {
                    kind: HelperKind::PluginWorker,
                    executable: PathBuf::from(
                        "/Applications/Superposition.app/Contents/Helpers/sp-plugin-worker",
                    ),
                    arguments: Vec::new(),
                },
                None,
                &fingerprint,
                &quarantine,
            )
            .expect_err("quarantined worker must not launch");
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(racks.table().is_empty());
        fs::remove_file(state_path).expect("remove quarantine state");
    }

    fn reap_until_exit(supervisor: &mut ProcessSupervisor) -> Vec<SupervisorEvent> {
        for _ in 0..50 {
            let events = supervisor.reap();
            if events
                .iter()
                .any(|event| matches!(event, SupervisorEvent::Exited { .. }))
            {
                return events;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("child did not exit")
    }

    #[test]
    fn reaps_short_lived_children() {
        let mut supervisor = ProcessSupervisor::new();
        let process_id = supervisor
            .launch(&launch("true", &[]))
            .expect("launch true");
        let initial_events = supervisor.reap();
        assert!(initial_events.contains(&SupervisorEvent::Started));
        let events = if initial_events
            .iter()
            .any(|event| matches!(event, SupervisorEvent::Exited { .. }))
        {
            initial_events
        } else {
            reap_until_exit(&mut supervisor)
        };
        assert_eq!(supervisor.status(process_id), HelperStatus::Failed);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, SupervisorEvent::Exited { code: 0 }))
        );
    }

    #[test]
    fn restarts_then_exhausts_short_lived_children() {
        let mut supervisor = ProcessSupervisor::new().with_restart_policy(RestartPolicy {
            max_restarts: 1,
            cooldown: Duration::ZERO,
        });
        supervisor
            .launch(&launch("false", &[]))
            .expect("launch false");
        let first_events = reap_until_exit(&mut supervisor);
        assert!(first_events.contains(&SupervisorEvent::RestartScheduled));
        let second_events = reap_until_exit(&mut supervisor);
        assert!(second_events.contains(&SupervisorEvent::RestartExhausted));
    }

    #[test]
    fn stops_sleeping_child() {
        let mut supervisor = ProcessSupervisor::new().with_shutdown_line("shutdown\n");
        let process_id = supervisor
            .launch(&launch("sleep", &["5"]))
            .expect("launch sleep");
        supervisor.stop(process_id).expect("stop sleep");
        assert_eq!(supervisor.status(process_id), HelperStatus::Stopped);
        assert!(supervisor.reap().contains(&SupervisorEvent::Stopped));
    }

    #[test]
    fn broken_shutdown_pipe_still_reaps_targeted_child() {
        let mut supervisor = ProcessSupervisor::new().with_shutdown_line("shutdown\n");
        let process_id = supervisor
            .launch(&launch("sh", &["-c", "exec 0<&-; sleep 5"]))
            .expect("launch child that closes stdin");
        thread::sleep(Duration::from_millis(50));
        let result = supervisor.stop(process_id);
        assert!(result.is_err(), "closed stdin must report its write error");
        assert_eq!(supervisor.status(process_id), HelperStatus::Stopped);
        assert!(supervisor.is_reaped(process_id));
        assert!(supervisor.reap().contains(&SupervisorEvent::Stopped));
    }

    #[test]
    fn is_reaped_distinguishes_unknown_and_unexpected_exit() {
        let mut supervisor = ProcessSupervisor::new();
        assert!(!supervisor.is_reaped(99));
        let process_id = supervisor
            .launch(&launch("true", &[]))
            .expect("launch true");
        assert!(!supervisor.is_reaped(process_id));
        let _ = reap_until_exit(&mut supervisor);
        assert_eq!(supervisor.status(process_id), HelperStatus::Failed);
        assert!(supervisor.is_reaped(process_id));
        supervisor
            .request_stop(process_id)
            .expect("already reaped stop is harmless");
        assert!(supervisor.is_reaped(process_id));
    }

    #[test]
    fn stop_request_after_natural_exit_preserves_failure_status() {
        let mut supervisor = ProcessSupervisor::new();
        let process_id = supervisor
            .launch(&launch("false", &[]))
            .expect("launch failing child");
        let mut exited = false;
        for _ in 0..50 {
            let managed = supervisor
                .children
                .get_mut(&process_id)
                .expect("owned child");
            if managed.child.try_wait().expect("check exit").is_some() {
                exited = true;
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(exited, "child must exit before stop request");
        assert_eq!(supervisor.status(process_id), HelperStatus::Running);
        supervisor
            .request_stop(process_id)
            .expect("already-exited child needs no kill");
        let events = supervisor.reap();
        assert_eq!(supervisor.status(process_id), HelperStatus::Failed);
        assert!(supervisor.is_reaped(process_id));
        assert!(events.contains(&SupervisorEvent::Exited { code: 1 }));
    }

    #[test]
    fn requested_stop_is_reaped_as_stopped_without_restart() {
        let mut supervisor = ProcessSupervisor::new().with_restart_policy(RestartPolicy {
            max_restarts: 1,
            cooldown: Duration::ZERO,
        });
        let process_id = supervisor
            .launch(&launch("sleep", &["5"]))
            .expect("launch sleep");
        supervisor.request_stop(process_id).expect("request stop");
        assert_eq!(supervisor.status(process_id), HelperStatus::Running);
        let mut stopped = false;
        for _ in 0..50 {
            let events = supervisor.reap();
            if supervisor.status(process_id) == HelperStatus::Stopped {
                assert!(events.contains(&SupervisorEvent::Stopped));
                assert!(!events.contains(&SupervisorEvent::RestartScheduled));
                stopped = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(stopped, "stopped child was not reaped");
        assert!(
            !supervisor
                .reap()
                .contains(&SupervisorEvent::RestartScheduled)
        );
    }

    #[test]
    fn launch_and_wait_reports_timeout() {
        let mut supervisor = ProcessSupervisor::new();
        let result = supervisor
            .launch_and_wait(&launch("sleep", &["2"]), Duration::from_millis(50))
            .expect("launch sleep");
        assert!(matches!(result, TimedHelperResult::TimedOut { .. }));
    }

    #[test]
    fn silenced_helper_still_obeys_timeout() {
        let mut supervisor = ProcessSupervisor::new();
        let result = supervisor
            .launch_and_wait_silenced(&launch("sleep", &["2"]), Duration::from_millis(50))
            .expect("launch sleep");
        assert!(matches!(result, TimedHelperResult::TimedOut { .. }));
    }

    #[test]
    fn rack_worker_table_tracks_replacement() {
        let mut table = RackWorkerTable::new();
        assert!(table.is_empty());
        table.assign(RackWorkerHandle {
            rack_index: 0,
            generation: 1,
            process_id: 10,
            bank_name: "bank-a".into(),
        });
        let previous = table.assign(RackWorkerHandle {
            rack_index: 0,
            generation: 2,
            process_id: 11,
            bank_name: "bank-b".into(),
        });
        assert_eq!(previous.map(|handle| handle.generation), Some(1));
        assert_eq!(table.get(0).map(|handle| handle.process_id), Some(11));
        assert!(table.covers([0]));
        assert!(!table.covers([0, 1]));
        let cleared = table.clear(0).expect("handle present");
        assert_eq!(cleared.bank_name, "bank-b");
        assert!(table.is_empty());
    }

    #[test]
    fn quarantine_trips_at_threshold() {
        let fingerprint = BundleFingerprint {
            algorithm: "sha256".to_owned(),
            digest: "fp".to_owned(),
        };
        let mut quarantine = QuarantineTable::with_threshold(2);
        assert!(!quarantine.record_failure(fingerprint.clone(), PluginFailureKind::WorkerCrash));
        assert!(quarantine.record_failure(fingerprint.clone(), PluginFailureKind::WorkerCrash));
        assert!(quarantine.is_quarantined(&fingerprint));
        quarantine.clear(&fingerprint);
        assert!(!quarantine.is_quarantined(&fingerprint));
    }

    #[test]
    fn dual_bank_names_alternate() {
        assert_eq!(RackWorkerTable::alternate_bank_name("rack-0-a"), "rack-0-b");
        assert_eq!(RackWorkerTable::alternate_bank_name("rack-0-b"), "rack-0-a");
    }

    #[test]
    fn rack_supervisor_launches_and_reaps_dead_heartbeats() {
        let mut processes = ProcessSupervisor::new();
        let mut racks = RackSupervisor::new();
        let handle = racks
            .create_or_replace_worker(&mut processes, 0, "rack-0-a", &launch("true", &[]), Some(1))
            .expect("launch");
        assert_eq!(handle.generation, 1);
        assert_eq!(racks.table().suspected_slot_bypass(0), Some(1));
        let dead = racks.poll_heartbeats(|_| false);
        assert_eq!(dead, vec![0]);
        assert!(racks.table().is_empty());
    }
}
