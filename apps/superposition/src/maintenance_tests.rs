//! Focused control-plane tests for a planned rack worker handoff.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use sp_model::{EndpointId, GainDb, Rack, RackId, RackTopology, SourceId};
use sp_protocol::control::{ControlRequestId, ControlResponse};
use sp_shared_memory_os::{RackRecoverySignal, RackRecoveryState, SharedMemoryRegion};
use sp_supervisor::{
    BundleFingerprint, HelperKind, HelperLaunch, PersistentQuarantine, PluginCatalog,
    ProcessSupervisor, Scanner, WorkerControlLaunch, WorkerControlSession,
};

use super::{
    HeartbeatWatch, HelperBinaries, LaunchedWorker, PlannedMaintenance, ProductRuntime,
    RestartBudget, RetainedWorker, maintenance,
};

static NEXT_TEST_PATH: AtomicU64 = AtomicU64::new(1);

fn runtime() -> ProductRuntime {
    let support = std::env::temp_dir().join(format!(
        "sp-maintenance-test-{}-{}",
        std::process::id(),
        NEXT_TEST_PATH.fetch_add(1, Ordering::Relaxed)
    ));
    let catalog = PluginCatalog::open(support.join("catalog.json")).expect("empty catalog");
    let quarantine =
        PersistentQuarantine::open(support.join("quarantine.json"), 3).expect("empty quarantine");
    let (maintenance_events_tx, maintenance_events_rx) = mpsc::channel();
    ProductRuntime {
        scanner: Scanner::new("/missing/scanner", Duration::from_secs(1), catalog)
            .with_quarantine(quarantine),
        helpers: HelperBinaries {
            scanner: PathBuf::from("/missing/scanner"),
            worker: PathBuf::from("/missing/worker"),
            #[cfg(target_os = "linux")]
            streamer: None,
            windows: None,
        },
        processes: ProcessSupervisor::new(),
        workers: BTreeMap::new(),
        editor_requests: BTreeSet::new(),
        diagnostics: Vec::new(),
        pending_parameter_updates: BTreeMap::new(),
        maintenance_events_tx,
        maintenance_events_rx,
        next_maintenance_task_id: 1,
        cancelled_banks: Vec::new(),
        retiring_workers: Vec::new(),
        sample_rate_hz: 48_000,
        wine: None,
        windows_programs: Vec::new(),
        #[cfg(target_os = "linux")]
        editor_display: None,
    }
}

/// A session for `process_id` whose control pipes belong to a stand-in worker (`sh`) that
/// answers the initial health request.
fn fake_session(runtime: &mut ProductRuntime, process_id: u64) -> WorkerControlSession {
    let target = super::control_target(0, 1, 12).expect("replacement target");
    // The first request on fresh pipes has ID 1, so the health response is known in advance.
    let mut health = Vec::new();
    ControlResponse::success(ControlRequestId::new(1).unwrap(), target, None, &[])
        .expect("health response")
        .write_to(&mut health)
        .expect("encode health response");
    let response = std::env::temp_dir().join(format!(
        "sp-maintenance-health-{}-{}",
        std::process::id(),
        NEXT_TEST_PATH.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&response, health).expect("write health response");
    let (_, pipes) = runtime
        .processes
        .launch_with_pipes(&HelperLaunch::native(
            HelperKind::PluginWorker,
            "/bin/sh",
            vec![
                "-c".to_owned(),
                format!("cat '{}'; cat >/dev/null", response.display()),
            ],
        ))
        .expect("stand-in worker");
    let launch = WorkerControlLaunch {
        launch: HelperLaunch::native(
            HelperKind::PluginWorker,
            "/Applications/Superposition.app/Contents/Helpers/sp-plugin-worker",
            [
                "--worker",
                "--bank",
                "/sp-test-bank",
                "--worker-id",
                "1",
                "--rack-index",
                "0",
                "--rack-generation",
                "12",
                "--bank-index",
                "1",
                "--bank-generation",
                "12",
                "--sample-rate",
                "48000",
                "--bundle",
                "/tmp/Test.vst3",
                "--class-id",
                "test-class",
            ]
            .map(str::to_owned)
            .to_vec(),
        ),
        target,
        timeout: Duration::from_secs(1),
    };
    let session =
        WorkerControlSession::attach(process_id, pipes, &launch).expect("fake worker session");
    let _ = std::fs::remove_file(response);
    session
}

fn quiescent_worker(task_id: u64) -> RetainedWorker {
    let recovery = Arc::new(RackRecoverySignal::new());
    assert!(recovery.request_quiesce());
    assert!(recovery.mark_quiescent());
    RetainedWorker {
        wire_index: 0,
        platform: sp_supervisor::PluginPlatform::Native,
        sample_rate_hz: 48_000,
        fingerprint: BundleFingerprint {
            algorithm: "sha256".to_owned(),
            digest: "test".to_owned(),
        },
        banks: [
            SharedMemoryRegion::create(11).expect("active bank"),
            SharedMemoryRegion::create(12).expect("replacement bank"),
        ],
        active_bank_index: 0,
        session: None,
        parameters: Vec::new(),
        parameter_feedback: Vec::new(),
        restart_cursors: Vec::new(),
        loaded_slots: Vec::new(),
        rack: Rack {
            id: RackId("rack".to_owned()),
            name: "rack".to_owned(),
            source_id: SourceId("source".to_owned()),
            endpoint_id: EndpointId("endpoint".to_owned()),
            topology: RackTopology::default(),
            gain_db: GainDb::default(),
            muted: false,
            bypassed: false,
            slots: Vec::new(),
        },
        saved_states: Vec::new(),
        recovery,
        recovery_failed: false,
        recovery_recorded: false,
        restart_budget: RestartBudget::default(),
        suspected_slot: None,
        latency_samples: 0,
        restart_flags: 0,
        parameter_mirror_incomplete: false,
        editors: Vec::new(),
        next_preview_poll: Instant::now(),
        segments: Vec::new(),
        chain: None,
        link_failures: 0,
        editor_worker: 0,
        maintenance: Some(PlannedMaintenance {
            flags: 1,
            requests: Vec::new(),
            terminal: false,
            failed: false,
            task_id: Some(task_id),
            cancel: Some(Arc::new(AtomicBool::new(false))),
            old_pid: None,
            new_pid: None,
            ready: None,
        }),
        maintenance_budget: RestartBudget::default(),
        heartbeat: HeartbeatWatch::new(Instant::now()),
        restarts: 0,
    }
}

#[test]
fn maintenance_events_require_exact_task_generation_and_recovery_identity() {
    let mut runtime = runtime();
    runtime.workers.insert(0, quiescent_worker(9));
    let recovery = Arc::clone(&runtime.workers[&0].recovery);
    assert!(runtime.maintenance_event_matches(0, 9, 12, &recovery));
    assert!(!runtime.maintenance_event_matches(0, 8, 12, &recovery));
    assert!(!runtime.maintenance_event_matches(0, 9, 13, &recovery));
    assert!(!runtime.maintenance_event_matches(0, 9, 12, &Arc::new(RackRecoverySignal::new())));
    runtime
        .workers
        .get_mut(&0)
        .expect("worker")
        .maintenance
        .as_ref()
        .expect("maintenance")
        .cancel
        .as_ref()
        .expect("cancel flag")
        .store(true, Ordering::Release);
    assert!(!runtime.maintenance_event_matches(0, 9, 12, &recovery));
}

#[test]
fn cancelled_spawn_event_is_rejected_before_launch_validation() {
    let mut runtime = runtime();
    runtime.workers.insert(0, quiescent_worker(9));
    let recovery = Arc::clone(&runtime.workers[&0].recovery);
    runtime
        .workers
        .get_mut(&0)
        .expect("worker")
        .maintenance
        .as_ref()
        .expect("maintenance")
        .cancel
        .as_ref()
        .expect("cancel flag")
        .store(true, Ordering::Release);
    let (reply, receiver) = mpsc::channel();
    runtime
        .maintenance_events_tx
        .send(maintenance::Event::Spawn {
            rack_index: 0,
            task_id: 9,
            generation: 12,
            recovery,
            launch: WorkerControlLaunch {
                launch: HelperLaunch::native(
                    HelperKind::PluginScanner,
                    "/definitely/not/a/worker",
                    Vec::new(),
                ),
                target: super::control_target(0, 1, 12).expect("target"),
                timeout: Duration::from_secs(1),
            },
            reply,
        })
        .expect("queue event");
    runtime.poll_maintenance_events();
    assert_eq!(
        receiver
            .recv()
            .expect("spawn reply")
            .expect_err("stale task"),
        "stale or cancelled rack maintenance launch"
    );
    assert!(
        runtime.workers[&0]
            .maintenance
            .as_ref()
            .expect("maintenance")
            .new_pid
            .is_none()
    );
}

#[test]
fn unload_after_spawn_retains_banks_until_both_children_are_reaped() {
    let mut runtime = runtime();
    runtime.workers.insert(0, quiescent_worker(9));
    let child = HelperLaunch::native(HelperKind::PluginWorker, "/bin/sleep", vec!["5".to_owned()]);
    let old_pid = runtime.processes.launch(&child).expect("old child");
    let new_pid = runtime
        .processes
        .launch(&child)
        .expect("spawned replacement");
    let maintenance = runtime
        .workers
        .get_mut(&0)
        .expect("worker")
        .maintenance
        .as_mut()
        .expect("maintenance");
    maintenance.old_pid = Some(old_pid);
    maintenance.new_pid = Some(new_pid);
    runtime.unload_rack(0);
    assert!(runtime.workers.is_empty());
    assert_eq!(runtime.cancelled_banks.len(), 1);
    assert!(!runtime.processes.is_reaped(old_pid) || !runtime.processes.is_reaped(new_pid));
    for _ in 0..100 {
        runtime.poll();
        if runtime.cancelled_banks.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(runtime.processes.is_reaped(old_pid));
    assert!(runtime.processes.is_reaped(new_pid));
    assert!(runtime.cancelled_banks.is_empty());
}

#[test]
fn reaped_replacement_cannot_be_installed_even_when_ready() {
    let mut runtime = runtime();
    runtime.workers.insert(0, quiescent_worker(9));
    let child = HelperLaunch::native(HelperKind::PluginWorker, "/bin/sleep", vec!["5".to_owned()]);
    let old_pid = runtime.processes.launch(&child).expect("old child");
    let new_pid = runtime.processes.launch(&child).expect("replacement child");
    let session = fake_session(&mut runtime, new_pid);
    runtime
        .processes
        .request_stop(old_pid)
        .expect("stop old child");
    runtime
        .processes
        .request_stop(new_pid)
        .expect("stop replacement child");
    for _ in 0..100 {
        runtime.processes.reap();
        if runtime.processes.is_reaped(old_pid) && runtime.processes.is_reaped(new_pid) {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert!(runtime.processes.is_reaped(old_pid));
    assert!(runtime.processes.is_reaped(new_pid));
    let worker = runtime.workers.get_mut(&0).expect("worker");
    let maintenance = worker.maintenance.as_mut().expect("maintenance");
    maintenance.old_pid = Some(old_pid);
    maintenance.new_pid = Some(new_pid);
    maintenance.ready = Some(maintenance::Ready {
        launched: LaunchedWorker {
            platform: worker.platform,
            fingerprint: worker.fingerprint.clone(),
            session,
            parameters: Vec::new(),
            loaded_slots: Vec::new(),
        },
        rack: worker.rack.clone(),
        states: Vec::new(),
    });
    runtime.finish_ready_maintenance();
    let worker = &runtime.workers[&0];
    assert_eq!(worker.active_bank_index, 0);
    assert!(worker.session.is_none());
    assert_eq!(worker.recovery.state(), RackRecoveryState::Quiescent);
    let maintenance = worker.maintenance.as_ref().expect("maintenance");
    assert!(maintenance.failed);
    assert!(maintenance.ready.is_none());
    assert!(runtime.worker_recovery_failed(0));
    assert!(!runtime.worker_recovering(0));
}
