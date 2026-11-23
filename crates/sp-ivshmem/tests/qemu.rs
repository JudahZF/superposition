//! QEMU's `ivshmem-doorbell` device against this crate's server.
//!
//! Needs QEMU: `SP_TEST_QEMU=/path/to/qemu-system-x86_64 cargo test -p sp-ivshmem --test qemu
//! -- --ignored`. QEMU runs paused with no guest, no display and no network.

#![cfg(target_os = "linux")]

use sp_ivshmem::{PeerEvent, PortPeer, PortServer};
use std::{
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[test]
#[ignore = "needs QEMU"]
fn qemu_joins_a_port_and_leaves_when_it_stops() {
    let qemu = std::env::var("SP_TEST_QEMU").unwrap_or_else(|_| "qemu-system-x86_64".into());
    let socket = std::env::temp_dir().join(format!("sp-ivshmem-qemu-{}", std::process::id()));
    let _ = std::fs::remove_file(&socket);
    let mut server = PortServer::bind(&socket, 1 << 20, 1).expect("bind");
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
    let mut host = PortPeer::connect(&socket, 1).expect("host peer");

    let mut child = Command::new(qemu)
        .args([
            "-machine",
            "q35,accel=tcg",
            "-nodefaults",
            "-display",
            "none",
        ])
        .args(["-S", "-monitor", "none", "-serial", "none", "-net", "none"])
        .arg("-chardev")
        .arg(format!("socket,path={},id=port0", socket.display()))
        .args(["-device", "ivshmem-doorbell,chardev=port0,vectors=1"])
        .stdin(Stdio::null())
        .spawn()
        .expect("start QEMU");

    let joined = wait_event(&mut host);
    let PeerEvent::Joined(qemu_id) = joined else {
        panic!("expected QEMU to join, got {joined:?}");
    };
    assert!(host.doorbell(qemu_id, 0).expect("doorbell").ring());

    child.kill().expect("stop QEMU");
    child.wait().expect("reap QEMU");
    assert_eq!(wait_event(&mut host), PeerEvent::Left(qemu_id));

    stop.store(true, Ordering::Relaxed);
    serving.join().expect("server thread");
    let _ = std::fs::remove_file(&socket);
}

fn wait_event(peer: &mut PortPeer) -> PeerEvent {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(event) = peer.poll_events().expect("events").first() {
            return *event;
        }
        assert!(Instant::now() < deadline, "no peer event");
        std::thread::sleep(Duration::from_millis(5));
    }
}
