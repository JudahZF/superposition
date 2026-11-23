//! The agent over Unix socket pairs that stand in for virtio-serial ports, with `cat` as the
//! worker: it echoes the host's control bytes back, as a relay must.

#![cfg(unix)]

use sp_vm_agent::{Agent, AgentConfig, Event, PortIo, Request};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
    sync::{Arc, Mutex},
    time::Duration,
};

struct Host {
    agent: UnixStream,
    events: BufReader<UnixStream>,
    workers: HashMap<u32, UnixStream>,
}

impl Host {
    fn send(&mut self, request: &Request) {
        let mut line = serde_json::to_vec(request).unwrap();
        line.push(b'\n');
        self.agent.write_all(&line).unwrap();
    }

    /// The next event that is not a heartbeat.
    fn event(&mut self) -> Event {
        loop {
            let mut line = String::new();
            assert!(
                self.events.read_line(&mut line).unwrap() > 0,
                "agent port closed"
            );
            let event: Event = serde_json::from_str(&line).unwrap();
            if !matches!(event, Event::Heartbeat { .. }) {
                return event;
            }
        }
    }
}

fn start_agent(ports: &[u32]) -> Host {
    start_agent_with(ports, "true".into(), std::env::temp_dir())
}

fn start_agent_with(ports: &[u32], scanner: std::path::PathBuf, vst3: std::path::PathBuf) -> Host {
    let (host_agent, agent_agent) = UnixStream::pair().unwrap();
    let mut agent_ports: HashMap<Option<u32>, UnixStream> = HashMap::new();
    agent_ports.insert(None, agent_agent);
    let mut workers = HashMap::new();
    for &port in ports {
        let (host, agent) = UnixStream::pair().unwrap();
        host.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        agent_ports.insert(Some(port), agent);
        workers.insert(port, host);
    }
    let agent_ports = Arc::new(Mutex::new(agent_ports));
    let config = AgentConfig {
        worker: "cat".into(),
        scanner,
        reports: vst3.join("reports"),
        uploads: vst3.join("uploads"),
        vst3,
        image_id: "test-image".into(),
        open_port: Box::new(move |port| {
            let stream = agent_ports
                .lock()
                .unwrap()
                .remove(&port)
                .ok_or_else(|| std::io::Error::other("port opened twice"))?;
            Ok(Box::new(stream) as Box<dyn PortIo>)
        }),
    };
    let agent = Agent::new(config).unwrap();
    std::thread::spawn(move || agent.run());
    host_agent
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    Host {
        events: BufReader::new(host_agent.try_clone().unwrap()),
        agent: host_agent,
        workers,
    }
}

fn echo(host: &mut Host, port: u32, bytes: &[u8]) -> Vec<u8> {
    let stream = host.workers.get_mut(&port).unwrap();
    stream.write_all(bytes).unwrap();
    let mut back = vec![0; bytes.len()];
    stream.read_exact(&mut back).unwrap();
    back
}

#[test]
fn a_started_worker_relays_control_bytes_and_stops_within_the_timeout() {
    let mut host = start_agent(&[0, 1]);
    host.send(&Request::Hello);
    assert_eq!(
        host.event(),
        Event::Hello {
            image_id: "test-image".into(),
            workers: vec![]
        }
    );

    host.send(&Request::Start {
        port: 1,
        args: vec![],
        audio_cpu: None,
        windows_cpus: None,
    });
    assert!(matches!(host.event(), Event::Started { port: 1, .. }));
    assert_eq!(echo(&mut host, 1, b"frame one"), b"frame one");

    host.send(&Request::Stop { port: 1 });
    assert_eq!(
        host.event(),
        Event::Stopped {
            port: 1,
            exited: true
        }
    );
    host.send(&Request::Hello);
    assert!(matches!(host.event(), Event::Hello { workers, .. } if workers.is_empty()));
}

#[test]
fn a_new_worker_on_a_port_replaces_the_old_one_and_gets_the_relay() {
    let mut host = start_agent(&[0]);
    host.send(&Request::Start {
        port: 0,
        args: vec![],
        audio_cpu: Some(2),
        windows_cpus: Some(0b1),
    });
    let Event::Started { pid: first, .. } = host.event() else {
        panic!("no first worker");
    };
    host.send(&Request::Start {
        port: 0,
        args: vec![],
        audio_cpu: Some(2),
        windows_cpus: Some(0b1),
    });
    let Event::Started { pid: second, .. } = host.event() else {
        panic!("no second worker");
    };
    assert_ne!(first, second);
    assert_eq!(
        echo(&mut host, 0, b"to the new worker"),
        b"to the new worker"
    );
}

#[test]
fn a_bad_request_is_an_error_event_and_the_agent_keeps_serving() {
    let mut host = start_agent(&[]);
    host.agent.write_all(b"{\"op\": \"reboot\"}\n").unwrap();
    assert!(matches!(host.event(), Event::Error { port: None, .. }));
    host.send(&Request::Hello);
    assert!(matches!(host.event(), Event::Hello { .. }));
}

#[test]
fn a_scan_reports_every_bundle_under_the_vst3_folder() {
    use std::os::unix::fs::PermissionsExt;

    let root = std::env::temp_dir().join(format!("sp-vm-agent-scan-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let vst3 = root.join("VST3");
    std::fs::create_dir_all(vst3.join("Vendor/Folder.vst3/Contents")).unwrap();
    std::fs::write(vst3.join("Single.vst3"), b"").unwrap();
    std::fs::write(vst3.join("readme.txt"), b"").unwrap();
    // Links back to the root, beside the bundles and in one: no walk follows them.
    std::os::unix::fs::symlink(&vst3, vst3.join("Vendor/back")).unwrap();
    std::os::unix::fs::symlink(&vst3, vst3.join("Vendor/Folder.vst3/Contents/back")).unwrap();
    // The scanner's stand-in writes a report naming its bundle (arguments 2 and 5).
    let scanner = root.join("scanner.sh");
    std::fs::write(
        &scanner,
        "#!/bin/sh\nprintf '{\"bundle\":\"%s\"}' \"$2\" > \"$5\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&scanner, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut host = start_agent_with(&[], scanner, vst3.clone());
    host.send(&Request::Scan);
    let mut bundles = Vec::new();
    loop {
        match host.event() {
            Event::Scanned {
                bundle,
                fingerprint,
                code,
                timed_out,
                report,
            } => {
                assert_eq!(code, Some(0));
                assert_eq!(fingerprint.len(), 64);
                assert!(!timed_out);
                assert!(report.unwrap().contains(&bundle));
                bundles.push(bundle);
            }
            Event::ScanDone { bundles: count } => {
                assert_eq!(count, 2);
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(
        bundles,
        [
            vst3.join("Single.vst3").display().to_string(),
            vst3.join("Vendor/Folder.vst3").display().to_string()
        ]
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_upload_in_parts_is_written_and_runs() {
    let root = std::env::temp_dir().join(format!("sp-vm-agent-upload-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let mut host = start_agent_with(&[], "true".into(), root.clone());

    // More than one part, so the offsets matter.
    let mut script = b"#!/bin/sh\nexit 7\n".to_vec();
    script.resize(sp_vm_agent::UPLOAD_CHUNK + 100, b'#');
    script.push(b'\n');
    for request in Request::uploads("setup.sh", &script) {
        host.send(&request);
        assert!(matches!(host.event(), Event::Uploaded { .. }));
    }
    assert_eq!(
        std::fs::read(root.join("uploads/setup.sh")).unwrap(),
        script
    );

    host.send(&Request::Run {
        name: "setup.sh".into(),
    });
    assert_eq!(
        host.event(),
        Event::Ran {
            name: "setup.sh".into(),
            code: Some(7)
        }
    );

    host.send(&Request::Run {
        name: "../escape".into(),
    });
    assert!(matches!(host.event(), Event::Error { .. }));
    let _ = std::fs::remove_dir_all(&root);
}
