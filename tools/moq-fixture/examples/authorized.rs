//! Test only: shared fixture packets and observable server-enforced TTL closure.
use multiview_moq::{
    Packet, Window,
    av::AvWindow,
    provider::{Feed, Fence, Runtime},
};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
struct Demand(Arc<AtomicBool>);
impl Drop for Demand {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    anyhow::ensure!(args.len() == 5, "cert key private-output-json fixture.ivf");
    let data = std::fs::read(&args[4])?;
    anyhow::ensure!(
        data.len() >= 32
            && data.len() < 16 * 1024 * 1024
            && &data[..4] == b"DKIF"
            && &data[8..12] == b"VP80",
        "invalid VP8 fixture"
    );
    let mut offset = 32;
    let mut frames = Vec::new();
    while offset < data.len() {
        anyhow::ensure!(offset + 12 <= data.len(), "truncated frame header");
        let len = u32::from_le_bytes(data[offset..offset + 4].try_into()?) as usize;
        offset += 12;
        anyhow::ensure!(
            len > 0 && offset + len <= data.len(),
            "invalid frame length"
        );
        frames.push(bytes::Bytes::copy_from_slice(&data[offset..offset + len]));
        offset += len;
    }
    anyhow::ensure!(
        !frames.is_empty() && frames[0][0] & 1 == 0,
        "missing keyframe"
    );
    let runtime = Runtime::listen_files(
        "[::]:4444".parse()?,
        "https://localhost:4444".into(),
        "moq-transport-16",
        PathBuf::from(&args[1]),
        PathBuf::from(&args[2]),
    )
    .await?;
    let (fence, windows) = Fence::channel(4);
    let released = Arc::new(AtomicBool::new(false));
    let feed = Feed { generation: 1, catalog: bytes::Bytes::from_static(br#"{"video":{"renditions":{"video":{"codec":"vp8","codedWidth":320,"codedHeight":180,"framerate":30}}},"audio":{"renditions":{}}}"#), audio_tracks: vec![], windows, demand: Box::new(Demand(released.clone())) };
    let producer = fence.clone();
    let admitted = runtime.admit(feed, fence, Duration::from_secs(8))?;
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&args[3])?;
    write!(
        output,
        "{{\"url\":\"{}\",\"broadcast\":\"media\"}}",
        admitted.url
    )?;
    drop(output);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_nanos(1_000_000_000 / 30));
        for index in 0u64.. {
            tick.tick().await;
            let payload = frames[index as usize % frames.len()].clone();
            let verified_rap = payload[0] & 1 == 0;
            if producer
                .try_offer(AvWindow {
                    video: Window {
                        generation: 1,
                        packets: vec![Packet {
                            payload,
                            pts_us: index * 1_000_000 / 30,
                            verified_rap,
                        }],
                    },
                    audio: Default::default(),
                })
                .is_err()
            {
                break;
            }
        }
    });
    eprintln!("authorized fixture ready (ticket redacted), TTL8s");
    tokio::time::sleep(Duration::from_secs(11)).await;
    anyhow::ensure!(
        released.load(Ordering::Acquire),
        "demand remained after TTL"
    );
    eprintln!("server evidence: demand_released=true after TTL8s");
    Ok(())
}
