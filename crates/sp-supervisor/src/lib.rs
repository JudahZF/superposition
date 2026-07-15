//! Process supervision for isolated helper binaries.

mod scanner;

pub use scanner::{
    BundleFingerprint, CachedScan, ScanCache, ScanDescriptor, ScanStatus, Scanner,
    discover_vst3_bundles, fingerprint_bundle,
};

use std::{
    collections::HashMap,
    io::{Read, Write},
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

/// The helper executable role managed by the application.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HelperKind {
    /// Hosts a plug-in instance outside the application process.
    PluginWorker,
    /// Inspects plug-in bundles outside the application process.
    PluginScanner,
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

/// Fingerprint quarantine used after restart exhaustion or repeated scan/worker faults.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuarantineTable {
    failures: HashMap<String, u32>,
    quarantined: HashMap<String, u32>,
    threshold: u32,
}

impl QuarantineTable {
    /// Creates a table that quarantines after `threshold` recorded failures.
    #[must_use]
    pub fn with_threshold(threshold: u32) -> Self {
        Self {
            failures: HashMap::new(),
            quarantined: HashMap::new(),
            threshold: threshold.max(1),
        }
    }

    /// Creates a table with the default threshold of three failures.
    #[must_use]
    pub fn new() -> Self {
        Self::with_threshold(3)
    }

    /// Records one failure against `fingerprint` and returns whether it is now quarantined.
    pub fn record_failure(&mut self, fingerprint: impl Into<String>) -> bool {
        let fingerprint = fingerprint.into();
        let count = self.failures.entry(fingerprint.clone()).or_insert(0);
        *count = count.saturating_add(1);
        if *count >= self.threshold {
            self.quarantined.insert(fingerprint, *count);
            true
        } else {
            false
        }
    }

    /// Returns whether `fingerprint` is quarantined.
    #[must_use]
    pub fn is_quarantined(&self, fingerprint: &str) -> bool {
        self.quarantined.contains_key(fingerprint)
    }

    /// Clears quarantine and failure counts for one fingerprint.
    pub fn clear(&mut self, fingerprint: &str) {
        self.failures.remove(fingerprint);
        self.quarantined.remove(fingerprint);
    }

    /// Clears all quarantine state.
    pub fn clear_all(&mut self) {
        self.failures.clear();
        self.quarantined.clear();
    }

    /// Returns the configured failure threshold.
    #[must_use]
    pub const fn threshold(&self) -> u32 {
        self.threshold
    }
}

impl Default for QuarantineTable {
    fn default() -> Self {
        Self::new()
    }
}

/// Product control-plane loop over [`ProcessSupervisor`] and [`RackWorkerTable`].
#[derive(Debug, Default)]
pub struct RackSupervisor {
    table: RackWorkerTable,
    quarantine: QuarantineTable,
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

    /// Returns the quarantine table.
    #[must_use]
    pub const fn quarantine(&self) -> &QuarantineTable {
        &self.quarantine
    }

    /// Returns a mutable quarantine table.
    pub fn quarantine_mut(&mut self) -> &mut QuarantineTable {
        &mut self.quarantine
    }

    /// Launches a worker for `rack_index`, replacing any previous assignment.
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

    /// Dual-bank recovery: stop the live worker, launch on the alternate bank, bypass a slot.
    ///
    /// When `fingerprint` is provided and process restarts are exhausted, the fingerprint is
    /// quarantined.
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
        fingerprint: Option<&str>,
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
        let events = processes.reap();
        if events
            .iter()
            .any(|event| matches!(event, SupervisorEvent::RestartExhausted))
            && let Some(fingerprint) = fingerprint
        {
            self.quarantine.record_failure(fingerprint);
        }
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

    /// Stops a child, first using the configured stdin shutdown line when present.
    ///
    /// # Errors
    ///
    /// Returns an error when communicating with or terminating the child fails.
    pub fn stop(&mut self, process_id: u64) -> std::io::Result<()> {
        let Some(mut managed) = self.children.remove(&process_id) else {
            return Ok(());
        };
        if let (Some(line), Some(stdin)) = (&self.shutdown_line, managed.child.stdin.as_mut()) {
            stdin.write_all(line.as_bytes())?;
            stdin.flush()?;
        }
        let deadline = Instant::now() + self.shutdown_timeout;
        while Instant::now() < deadline {
            if managed.child.try_wait()?.is_some() {
                self.statuses.insert(process_id, HelperStatus::Stopped);
                self.events.push(SupervisorEvent::Stopped);
                return Ok(());
            }
            thread::sleep(Duration::from_millis(5));
        }
        managed.child.kill()?;
        managed.child.wait()?;
        self.statuses.insert(process_id, HelperStatus::Stopped);
        self.events.push(SupervisorEvent::Stopped);
        Ok(())
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

    /// Like [`Self::launch_and_wait`], but captures stdout/stderr for disposable helpers.
    ///
    /// Used by the Phase 2 isolated scanner so timeout/crash outcomes can still recover JSON.
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
        let stdout_thread = thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut stdout) = stdout {
                let _ = stdout.read_to_end(&mut bytes);
            }
            bytes
        });
        let stderr_thread = thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut stderr) = stderr {
                let _ = stderr.read_to_end(&mut bytes);
            }
            bytes
        });

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
                self.statuses.insert(process_id, HelperStatus::Failed);
                self.events.push(SupervisorEvent::Exited {
                    code: exit.code().unwrap_or(-1),
                });
                self.schedule_restart(managed);
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
    use std::{path::PathBuf, thread, time::Duration};

    use super::{
        HelperKind, HelperLaunch, HelperStatus, ProcessSupervisor, QuarantineTable, RackSupervisor,
        RackWorkerHandle, RackWorkerTable, RestartPolicy, SupervisorEvent, TimedHelperResult,
    };

    fn launch(executable: &str, arguments: &[&str]) -> HelperLaunch {
        HelperLaunch {
            kind: HelperKind::PluginWorker,
            executable: PathBuf::from(executable),
            arguments: arguments.iter().map(ToString::to_string).collect(),
        }
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
    fn launch_and_wait_reports_timeout() {
        let mut supervisor = ProcessSupervisor::new();
        let result = supervisor
            .launch_and_wait(&launch("sleep", &["2"]), Duration::from_millis(50))
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
        let mut quarantine = QuarantineTable::with_threshold(2);
        assert!(!quarantine.record_failure("fp"));
        assert!(quarantine.record_failure("fp"));
        assert!(quarantine.is_quarantined("fp"));
        quarantine.clear("fp");
        assert!(!quarantine.is_quarantined("fp"));
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
