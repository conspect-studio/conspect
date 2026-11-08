//! Test only: issue one isolated ticket and prove connected TTL closure.
use multiview_moq::provider::{Feed, Fence, Runtime};
use std::{time::Duration, path::PathBuf};
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    anyhow::ensure!(args.len() == 4, "cert key private-output-json");
    let runtime = Runtime::listen_files("[::]:4444".parse()?, "https://localhost:4444".into(), "moq-transport-16", PathBuf::from(&args[1]), PathBuf::from(&args[2])).await?;
    let (fence, windows) = Fence::channel(1);
    let feed = Feed { generation: 1, catalog: bytes::Bytes::from_static(br#"{"video":{"renditions":{"video":{"codec":"vp8","codedWidth":320,"codedHeight":180}}},"audio":{"renditions":{}}}"#), audio_tracks: vec![], windows, demand: Box::new(()) };
    let admitted = runtime.admit(feed, fence, Duration::from_secs(8))?;
    use std::os::unix::fs::OpenOptionsExt;
    use std::io::Write;
    let mut output = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&args[3])?;
    write!(output, "{{\"url\":\"{}\",\"broadcast\":\"media\"}}", admitted.url)?;
    eprintln!("authorized test listener ready (ticket redacted), TTL8s");
    tokio::time::sleep(Duration::from_secs(15)).await;
    Ok(())
}
