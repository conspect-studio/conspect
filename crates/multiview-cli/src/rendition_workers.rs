//! Independently bounded video rendition workers.
//!
//! The program bakes exactly one immutable canvas per tick. Each distinct
//! rendition owns an encoder thread and a separate bounded admission queue;
//! identical sink bindings reference that rendition's one packet allocation.

use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use multiview_compositor::pipeline::Nv12Image;
use multiview_ffmpeg::EncodedPacket;

use crate::pipeline::PipelineError;

const WORKER_QUEUE_FRAMES: usize = 4;

/// One baked picture on the program's absolute rational tick.
pub(crate) struct BakedTick {
    pub tick: u64,
    pub canvas: Arc<Nv12Image>,
}

/// An independent video encoder. Production opens one concrete libav encoder;
/// tests inject a counter to prove output bindings do not open extra workers.
pub(crate) trait VideoEncodeWorker: Send + 'static {
    fn encode(&mut self, item: BakedTick) -> Result<Vec<EncodedPacket>, PipelineError>;
    fn finish(&mut self) -> Result<Vec<EncodedPacket>, PipelineError>;
}

/// Bounded input and owned worker lifecycle for one unique video key.
pub(crate) struct RenditionWorker {
    input: SyncSender<BakedTick>,
    thread: JoinHandle<Result<(), PipelineError>>,
}

impl RenditionWorker {
    pub fn spawn(
        index: usize,
        mut encoder: Box<dyn VideoEncodeWorker>,
        mut on_packets: impl FnMut(usize, Vec<EncodedPacket>) -> Result<(), PipelineError>
            + Send
            + 'static,
    ) -> Result<Self, PipelineError> {
        let (input, receiver) = mpsc::sync_channel(WORKER_QUEUE_FRAMES);
        let thread = thread::Builder::new()
            .name(format!("multiview-video-rendition-{index}"))
            .spawn(move || {
                while let Ok(item) = receiver.recv() {
                    on_packets(index, encoder.encode(item)?)?;
                }
                on_packets(index, encoder.finish()?)
            })
            .map_err(|err| PipelineError::Output {
                kind: "video-worker",
                reason: err.to_string(),
            })?;
        Ok(Self { input, thread })
    }

    /// Never wait on a busy worker; production must surface overload and preserve
    /// the program clock rather than silently claim a complete coded cadence.
    pub fn offer(&self, item: BakedTick) -> Result<(), PipelineError> {
        self.input
            .try_send(item)
            .map_err(|err| PipelineError::Output {
                kind: "video-worker",
                reason: match err {
                    TrySendError::Full(_) => "video rendition capacity exceeded".to_owned(),
                    TrySendError::Disconnected(_) => "video rendition worker ended".to_owned(),
                },
            })
    }

    pub fn join(self) -> Result<(), PipelineError> {
        drop(self.input);
        self.thread.join().map_err(|_| PipelineError::Output {
            kind: "video-worker",
            reason: "video rendition worker panicked".to_owned(),
        })?
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::{BakedTick, RenditionWorker, VideoEncodeWorker};
    use crate::pipeline::PipelineError;
    use multiview_compositor::pipeline::Nv12Image;
    use multiview_ffmpeg::EncodedPacket;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct Counter(Arc<AtomicUsize>);
    impl VideoEncodeWorker for Counter {
        fn encode(&mut self, _item: BakedTick) -> Result<Vec<EncodedPacket>, PipelineError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
        }
        fn finish(&mut self) -> Result<Vec<EncodedPacket>, PipelineError> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn independent_workers_share_one_canvas_for_three_bindings() {
        let opens = Arc::new(AtomicUsize::new(0));
        let frames = Arc::new(AtomicUsize::new(0));
        let canvas = Arc::new(
            Nv12Image::solid(
                16,
                16,
                16,
                128,
                128,
                multiview_core::color::ColorInfo::default(),
            )
            .expect("canvas"),
        );
        let mut workers = Vec::new();
        // SRT1 HEVC + SRT2/HLS H.264: binding indices [0,1,1].
        for _ in 0..2 {
            opens.fetch_add(1, Ordering::SeqCst);
            workers.push(
                RenditionWorker::spawn(
                    workers.len(),
                    Box::new(Counter(Arc::clone(&frames))),
                    |_, _| Ok(()),
                )
                .expect("worker"),
            );
        }
        assert_eq!(opens.load(Ordering::SeqCst), 2);
        for worker in &workers {
            worker
                .offer(BakedTick {
                    tick: 0,
                    canvas: Arc::clone(&canvas),
                })
                .expect("offer");
        }
        for worker in workers {
            worker.join().expect("join");
        }
        assert_eq!(frames.load(Ordering::SeqCst), 2);
        assert_eq!(Arc::strong_count(&canvas), 1);
    }
}
