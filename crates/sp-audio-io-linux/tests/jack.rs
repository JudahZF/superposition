//! Endpoint lifecycle against a private JACK dummy server. Never uses the user's audio server.
#![cfg(target_os = "linux")]

use std::{
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use sp_audio_io::{AudioEndpoint, AudioFormat, AudioRouteConfig};
use sp_audio_io_linux::{LinuxAudioEndpoint, enumerate_devices};

const CHILD: &str = "SP_JACK_ENDPOINT_CHILD";

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            // JACK2 must run its shutdown cleanup to release its server-registry slot.
            let _ = Command::new("kill")
                .args(["-TERM", &self.0.id().to_string()])
                .status();
            let _ = self.0.wait();
        }
    }
}

#[test]
#[ignore = "requires jackd; starts a private named dummy server, never the user's server"]
fn endpoint_lifecycle_on_a_private_jack_server() {
    if std::env::var_os(CHILD).is_some() {
        endpoint_checks();
        return;
    }
    let name = format!("sp-endpoint-{}", std::process::id());
    let log = std::env::temp_dir().join(format!("{name}.log"));
    let log_file = std::fs::File::create(&log).unwrap();
    let server = Server(
        Command::new("jackd")
            .args(["--no-realtime", "-n", &name, "-d", "dummy"])
            .args(["-r", "48000", "-p", "64", "-C", "2", "-P", "2"])
            .stdout(Stdio::from(log_file.try_clone().unwrap()))
            .stderr(Stdio::from(log_file))
            .spawn()
            .expect("jackd must be in the dev shell"),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !std::fs::read_to_string(&log)
        .unwrap()
        .contains("JACK server starting")
    {
        assert!(Instant::now() < deadline, "private JACK server did not start");
        thread::sleep(Duration::from_millis(20));
    }
    // JACK selects its server from the environment, which a running test cannot change safely.
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "endpoint_lifecycle_on_a_private_jack_server"])
        .args(["--include-ignored", "--nocapture"])
        .env(CHILD, "1")
        .env("JACK_DEFAULT_SERVER", &name)
        .status()
        .unwrap();
    drop(server);
    assert!(
        status.success(),
        "endpoint checks failed; server log: {}",
        std::fs::read_to_string(&log).unwrap()
    );
    std::fs::remove_file(log).unwrap();
}

fn endpoint_checks() {
    // jackd logs "starting" before it accepts clients.
    let deadline = Instant::now() + Duration::from_secs(5);
    let devices = loop {
        match enumerate_devices() {
            Ok(devices) => break devices,
            Err(error) if Instant::now() > deadline => panic!("private server unavailable: {error}"),
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    };
    let [device] = devices.as_slice() else {
        panic!("expected one JACK server, found {devices:?}");
    };
    assert_eq!(device.supported_buffer_frames, [64]);
    assert_eq!(device.sample_rate_hz, 48_000);
    assert_eq!(device.capabilities.max_input_channels, 2);
    assert_eq!(device.capabilities.max_output_channels, 2);

    let route = |frames| AudioRouteConfig {
        input: Some(device.info.id.clone()),
        output: device.info.id.clone(),
        format: AudioFormat::product_stereo(frames).unwrap(),
    };
    let mut endpoint = LinuxAudioEndpoint::new();

    // A route at the wrong quantum fails before audio starts and keeps the renderer.
    let error = endpoint.start_route(route(128)).unwrap_err().to_string();
    assert!(error.contains("64-frame blocks"), "{error}");
    assert!(!endpoint.is_faulted());
    assert!(endpoint.service_stopped_recoveries().is_some());

    for _ in 0..2 {
        endpoint.start_route(route(64)).expect("route starts");
        assert!(endpoint.service_stopped_recoveries().is_none());
        let deadline = Instant::now() + Duration::from_secs(5);
        let telemetry = loop {
            let telemetry = endpoint.callback_telemetry().expect("running telemetry");
            if telemetry.rendered >= 50 || Instant::now() > deadline {
                break telemetry;
            }
            thread::sleep(Duration::from_millis(10));
        };
        assert!(telemetry.rendered >= 50, "{telemetry:?}");
        assert!(telemetry.is_coherent());
        assert_eq!(telemetry.invalid_frames, 0);
        assert_eq!(telemetry.frame_histogram_64, telemetry.rendered + telemetry.silenced);
        assert_eq!(endpoint.poll_event(), None);

        // Stopping retires the callback and returns the renderer for the next start.
        endpoint.stop().expect("stop");
        assert!(endpoint.active_format().is_none());
        assert!(endpoint.service_stopped_recoveries().is_some());
    }
}
