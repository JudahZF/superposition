//! The port server and its peers over a real Unix socket.

#![cfg(target_os = "linux")]

use sp_ivshmem::{PeerEvent, PortPeer, PortServer};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

struct Served {
    socket: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Served {
    fn start(name: &str, size: u64, vectors: u16) -> Self {
        let socket = std::env::temp_dir().join(format!("sp-ivshmem-{}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&socket);
        let mut server = PortServer::bind(&socket, size, vectors).expect("bind");
        let stop = Arc::new(AtomicBool::new(false));
        let thread = std::thread::spawn({
            let stop = Arc::clone(&stop);
            move || {
                while !stop.load(Ordering::Relaxed) {
                    server
                        .serve_once(Some(Duration::from_millis(10)))
                        .expect("serve");
                }
            }
        });
        Self {
            socket,
            stop,
            thread: Some(thread),
        }
    }

    fn path(&self) -> &Path {
        &self.socket
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

fn next_event(peer: &mut PortPeer) -> PeerEvent {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(event) = peer.poll_events().expect("events").first() {
            return *event;
        }
        assert!(Instant::now() < deadline, "no peer event");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn peers_share_memory_and_ring_each_other_by_vector() {
    let served = Served::start("ring", 3000, 2);
    let mut host = PortPeer::connect(served.path(), 2).expect("host");
    let guest = PortPeer::connect(served.path(), 2).expect("guest");

    assert_eq!(next_event(&mut host), PeerEvent::Joined(guest.id()));
    assert_eq!(host.only_other_peer(), Some(guest.id()));
    assert_eq!(guest.only_other_peer(), Some(host.id()));

    let host_memory = rustix::fs::fstat(host.memory()).expect("fstat");
    let guest_memory = rustix::fs::fstat(guest.memory()).expect("fstat");
    assert_eq!(host_memory.st_ino, guest_memory.st_ino);
    assert_eq!(host_memory.st_size, 4096, "the size is a power of two");

    let doorbell = host.doorbell(guest.id(), 1).expect("doorbell");
    assert!(doorbell.ring());
    assert!(doorbell.ring());
    assert_eq!(
        guest
            .interrupt(1)
            .unwrap()
            .wait(Duration::from_secs(1))
            .unwrap(),
        2
    );
    assert_eq!(
        guest.interrupt(0).unwrap().clear().unwrap(),
        0,
        "vector 0 is apart"
    );

    let reply = guest.doorbell(host.id(), 0).expect("reply doorbell");
    assert!(reply.ring());
    assert_eq!(
        host.interrupt(0)
            .unwrap()
            .wait(Duration::from_secs(1))
            .unwrap(),
        1
    );
}

#[test]
fn a_disconnect_is_announced_and_a_returning_peer_gets_a_new_id() {
    let served = Served::start("leave", 4096, 1);
    let mut host = PortPeer::connect(served.path(), 1).expect("host");
    let guest = PortPeer::connect(served.path(), 1).expect("guest");
    let guest_id = guest.id();
    assert_eq!(next_event(&mut host), PeerEvent::Joined(guest_id));

    // A kept doorbell stays a valid descriptor after its peer leaves.
    let kept = host.doorbell(guest_id, 0).expect("doorbell");
    drop(guest);
    assert_eq!(next_event(&mut host), PeerEvent::Left(guest_id));
    assert_eq!(host.only_other_peer(), None);
    assert!(kept.ring());

    // QEMU writes into freed memory if a departed peer's ID comes back (see PortServer).
    let again = PortPeer::connect(served.path(), 1).expect("guest again");
    assert_ne!(again.id(), guest_id);
    assert_eq!(next_event(&mut host), PeerEvent::Joined(again.id()));
}

#[test]
fn a_peer_with_fewer_vectors_closes_the_extra_ones() {
    let served = Served::start("vectors", 4096, 2);
    let mut host = PortPeer::connect(served.path(), 2).expect("host");
    let small = PortPeer::connect(served.path(), 1).expect("one-vector peer");
    assert_eq!(next_event(&mut host), PeerEvent::Joined(small.id()));

    assert!(small.interrupt(1).is_err());
    assert!(small.doorbell(host.id(), 1).is_err());
    assert!(small.doorbell(host.id(), 0).unwrap().ring());
    assert_eq!(
        host.interrupt(0)
            .unwrap()
            .wait(Duration::from_secs(1))
            .unwrap(),
        1
    );
}
