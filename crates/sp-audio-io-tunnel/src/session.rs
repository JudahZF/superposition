//! A session's render thread.

use std::io;
use std::sync::Arc;
use std::sync::atomic::Ordering::Acquire;
use std::sync::atomic::Ordering::Release;
use std::sync::mpsc::{SendError, SyncSender, sync_channel};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use sp_audio_io::ProductRenderer;
use sp_shared_memory_os::{AudioThreadPolicy, SchedulingMode};

use crate::bridge::{self, NetworkSide, RenderStage, SessionFormat, SessionTelemetry};

/// How long [`Session::stop`] waits for the render thread. It only waits on workers inside one
/// block, so this is far more than it needs.
const STOP_TIMEOUT: Duration = Duration::from_secs(2);

/// The render thread of a running session. The network side runs on the Tunnel thread.
pub(crate) struct Session {
    thread: JoinHandle<Option<RenderStage>>,
    telemetry: SessionTelemetry,
    format: SessionFormat,
    scheduling: SchedulingMode,
}

/// The render thread did not start.
pub(crate) struct StartFailed {
    /// The renderer, unless the thread panicked with it.
    pub(crate) renderer: Option<Box<ProductRenderer>>,
    pub(crate) error: io::Error,
}

/// The render thread was not retired: the renderer is lost.
#[derive(Debug, thiserror::Error)]
#[error("the Tunnel render thread did not stop; the renderer is lost")]
pub(crate) struct Unretired;

impl Session {
    /// Starts the render thread with real-time scheduling for `format` and returns it with the
    /// network side for the Tunnel thread.
    ///
    /// # Errors
    ///
    /// The thread cannot start, or real-time scheduling failed in a way that
    /// `sp-shared-memory-os` does not accept. The renderer comes back unless the thread
    /// panicked.
    pub(crate) fn start(
        renderer: Box<ProductRenderer>,
        format: SessionFormat,
        stereo: bool,
    ) -> Result<(Self, NetworkSide), StartFailed> {
        let (mut network, stage) = bridge::session(renderer, format, stereo);
        let telemetry = stage.telemetry();
        // The stage moves to the thread only after it started, so a failed spawn keeps it.
        let (stage_tx, stage_rx) = sync_channel::<RenderStage>(1);
        let (ready_tx, ready_rx) = sync_channel(1);
        let thread = match thread::Builder::new()
            .name("sp-tunnel-render".into())
            .spawn(move || stage_rx.recv().ok().map(|stage| render(stage, &ready_tx)))
        {
            Ok(thread) => thread,
            Err(error) => {
                return Err(StartFailed {
                    renderer: Some(stage.into_renderer()),
                    error,
                });
            }
        };
        network.set_render_thread(thread.thread().clone());
        if let Err(SendError(stage)) = stage_tx.send(stage) {
            return Err(StartFailed {
                renderer: Some(stage.into_renderer()),
                error: io::Error::other("the Tunnel render thread exited before it started"),
            });
        }
        let error = match ready_rx.recv() {
            Ok(Ok(scheduling)) => {
                let session = Self {
                    thread,
                    telemetry,
                    format,
                    scheduling,
                };
                return Ok((session, network));
            }
            Ok(Err(error)) => error,
            Err(_) => io::Error::other("the Tunnel render thread panicked"),
        };
        let renderer = thread.join().ok().flatten().map(RenderStage::into_renderer);
        Err(StartFailed { renderer, error })
    }

    pub(crate) fn telemetry(&self) -> &SessionTelemetry {
        &self.telemetry
    }

    pub(crate) const fn format(&self) -> SessionFormat {
        self.format
    }

    pub(crate) const fn scheduling(&self) -> SchedulingMode {
        self.scheduling
    }

    /// Stops the render thread and gives the renderer back.
    ///
    /// # Errors
    ///
    /// The thread did not finish within [`STOP_TIMEOUT`], or it panicked.
    pub(crate) fn stop(self) -> Result<Box<ProductRenderer>, Unretired> {
        self.telemetry.shared().stop.store(true, Release);
        self.thread.thread().unpark();
        let deadline = Instant::now() + STOP_TIMEOUT;
        while !self.thread.is_finished() {
            if Instant::now() >= deadline {
                // Dropping the handle detaches the thread; it keeps the renderer.
                return Err(Unretired);
            }
            thread::sleep(Duration::from_millis(1));
        }
        match self.thread.join() {
            Ok(Some(stage)) => Ok(stage.into_renderer()),
            _ => Err(Unretired),
        }
    }
}

/// The render thread: applies the scheduling, reports it, then renders until stopped.
fn render(mut stage: RenderStage, ready: &SyncSender<io::Result<SchedulingMode>>) -> RenderStage {
    let format = stage.format();
    let mut policy = match AudioThreadPolicy::new().and_then(|mut policy| {
        policy.enter(format.block.as_u32(), format.sample_rate_hz)?;
        Ok(policy)
    }) {
        Ok(policy) => policy,
        Err(error) => {
            let _ = ready.send(Err(error));
            return stage;
        }
    };
    let _ = ready.send(Ok(policy.mode()));
    let shared = Arc::clone(&stage.shared);
    while !shared.stop.load(Acquire) {
        if !stage.run() {
            // The network side unparks after each complete block and each resync request;
            // an unpark before this park makes it return at once.
            thread::park();
        }
    }
    let _ = policy.leave();
    stage
}

#[cfg(test)]
mod tests {
    use sp_audio_io::BlockFrames;
    use sp_engine::PreparedGraph;
    use tunnel_host::audio::{AudioBuffers, AudioLayout};
    use tunnel_types::MediaTime;

    use super::*;
    use crate::{Endpoints, TunnelChannels};

    /// The render thread renders the blocks that the network side completes, and stopping it
    /// gives back the very renderer that started it.
    #[test]
    fn stop_returns_the_original_renderer() {
        let renderer = Box::new(ProductRenderer::new(PreparedGraph::empty()));
        let original = std::ptr::from_ref(renderer.as_ref());
        let format = SessionFormat {
            block: BlockFrames::Frames64,
            sample_rate_hz: 96_000,
            inputs: 2,
            outputs: 2,
        };
        let Ok((session, mut network)) = Session::start(renderer, format, false) else {
            panic!("the render thread did not start");
        };
        let channels = TunnelChannels {
            inputs: 2,
            outputs: 2,
        };
        let layout = AudioLayout::new(&Endpoints::generate().decls(channels));
        let mut buffers = AudioBuffers::new(&layout);
        let deadline = Instant::now() + STOP_TIMEOUT;
        while session.telemetry().counters().rendered < 4 {
            assert!(Instant::now() < deadline, "the render thread renders");
            network.process(&mut buffers.block(&layout, MediaTime(0), 24));
            thread::sleep(Duration::from_micros(250));
        }
        let Ok(renderer) = session.stop() else {
            panic!("the render thread stops");
        };
        assert_eq!(std::ptr::from_ref(renderer.as_ref()), original);
    }
}
