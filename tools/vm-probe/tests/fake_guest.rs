//! The probe pair over a real port server: the host and a Linux process that plays the guest.

#![cfg(target_os = "linux")]

use sp_ivshmem::PortServer;
use sp_shared_memory_os::port_region_bytes;
use std::{
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

struct Killed(Child);

impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn the_host_measures_round_trips_through_a_fake_guest() {
    let socket = std::env::temp_dir().join(format!("vm-probe-{}", std::process::id()));
    let _ = std::fs::remove_file(&socket);
    let mut server = PortServer::bind(&socket, port_region_bytes(), 1).expect("port server");
    let stop = Arc::new(AtomicBool::new(false));
    let serving = std::thread::spawn({
        let stop = Arc::clone(&stop);
        move || {
            while !stop.load(Ordering::Relaxed) {
                server
                    .serve_once(Some(Duration::from_millis(10)))
                    .expect("serve");
            }
        }
    });

    let probe = env!("CARGO_BIN_EXE_vm-probe");
    let _guest = Killed(
        Command::new(probe)
            .args(["fake-guest", "--socket"])
            .arg(&socket)
            .spawn()
            .expect("fake guest"),
    );
    // A generous window: this test proves the path, not the timing of a shared CI machine.
    let host = Command::new(probe)
        .args([
            "host",
            "--frames",
            "128",
            "--rate",
            "48000",
            "--seconds",
            "2",
        ])
        .args(["--window-us", "200000", "--socket"])
        .arg(&socket)
        .stderr(Stdio::inherit())
        .output()
        .expect("host");
    stop.store(true, Ordering::Relaxed);
    serving.join().expect("server thread");
    let _ = std::fs::remove_file(&socket);

    assert!(host.status.success(), "host failed: {:?}", host.status);
    let report: serde_json::Value = serde_json::from_slice(&host.stdout).expect("JSON report");
    assert!(report["blocks"].as_u64().unwrap() > 500, "{report}");
    assert_eq!(report["misses"], 0);
    assert_eq!(report["lost"], 0);
    assert_eq!(report["ring_failures"], 0);
}
