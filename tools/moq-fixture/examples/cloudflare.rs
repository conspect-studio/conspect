//! Development-only, shared pre-encoded VP8 publisher. No runtime encoder.
use anyhow::ensure;
use bytes::Bytes;
use std::time::Duration;

fn frames(data: &[u8]) -> anyhow::Result<Vec<Bytes>> {
    ensure!(
        data.len() >= 32 && &data[..4] == b"DKIF" && &data[8..12] == b"VP80",
        "expected VP8 IVF"
    );
    ensure!(
        u16::from_le_bytes(data[12..14].try_into()?) == 320
            && u16::from_le_bytes(data[14..16].try_into()?) == 180,
        "expected 320x180"
    );
    let mut offset = 32;
    let mut result = Vec::new();
    while offset < data.len() {
        ensure!(offset + 12 <= data.len(), "truncated IVF frame header");
        let len = u32::from_le_bytes(data[offset..offset + 4].try_into()?) as usize;
        offset += 12;
        ensure!(
            len > 0 && len <= 1024 * 1024 && offset + len <= data.len(),
            "invalid IVF frame size"
        );
        result.push(Bytes::copy_from_slice(&data[offset..offset + len]));
        offset += len;
    }
    ensure!(
        !result.is_empty() && result[0][0] & 1 == 0,
        "fixture must start at keyframe"
    );
    Ok(result)
}

#[tokio::main(worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    ensure!(args.len() == 3, "usage: cloudflare <private-handoff.json> <fixture.ivf>");
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::symlink_metadata(&args[1])?;
    ensure!(meta.is_file() && meta.permissions().mode() & 0o077 == 0, "private handoff required");
    let handoff: serde_json::Value = serde_json::from_slice(&std::fs::read(&args[1])?)?;
    let token = handoff["tokens"].as_array().and_then(|tokens| tokens.iter().find(|t|
        t["operations"].as_array().is_some_and(|ops| ops.iter().any(|op| op == "publish"))))
        .and_then(|t| t["secret"].as_str()).ok_or_else(|| anyhow::anyhow!("publish token missing"))?;
    let endpoint = format!("https://draft-16.cloudflare.mediaoverquic.com/{token}");
    let protocol = "moq-transport-16";
    use std::io::Read;
    let mut data = Vec::new();
    std::fs::File::open(&args[2])?
        .take(16 * 1024 * 1024 + 1)
        .read_to_end(&mut data)?;
    ensure!(data.len() <= 16 * 1024 * 1024, "fixture exceeds 16MiB");
    let frames = frames(&data)?;
    let mut audio_packets = Vec::new();
    if let Some(path) = args.get(5) {
        let bytes = std::fs::read(path)?;
        ensure!(bytes.len() <= 1024 * 1024, "audio fixture too large");
        let mut offset = 0;
        while offset < bytes.len() {
            ensure!(offset + 4 <= bytes.len(), "audio header truncated");
            let len = u32::from_le_bytes(bytes[offset..offset+4].try_into()?) as usize;
            offset += 4;
            ensure!(len > 0 && len <= 65536 && offset + len <= bytes.len(), "audio packet invalid");
            audio_packets.push(Bytes::copy_from_slice(&bytes[offset..offset+len]));
            offset += len;
        }
        ensure!(!audio_packets.is_empty(), "audio fixture empty");
    }
    let origin = moq_tokio::origin::spawn();
    let broadcast = origin.create_broadcast("fixture")?;
    broadcast.announce(Default::default())?;
    let mut catalog = broadcast.create_track(
        "catalog.json",
        hang::catalog::Catalog::<()>::default_track_info(),
    )?;
    let video_json = br#"{"video":{"renditions":{"video":{"codec":"vp8","codedWidth":320,"codedHeight":180,"framerate":30}}},"audio":{"renditions":{}}}"#;
    let av_json = br#"{"video":{"renditions":{"video":{"codec":"vp8","codedWidth":320,"codedHeight":180,"framerate":30}}},"audio":{"renditions":{"audio":{"codec":"opus","sampleRate":48000,"numberOfChannels":2}}}}"#;
    let json: &[u8] = if audio_packets.is_empty() { video_json } else { av_json };
    let _: hang::catalog::Catalog = hang::catalog::Catalog::from_slice(json)?;
    catalog.write_frame(
        moq_net::Timestamp::from_millis(0)?,
        Bytes::from_static(json),
    )?;
    let video = broadcast.create_track(
        "video",
        hang::container::track_info(hang::catalog::PRIORITY.video)
            .with_max_age(Duration::from_secs(2)),
    )?;
    let mut audio_tracks = std::collections::BTreeMap::new();
    if !audio_packets.is_empty() {
        audio_tracks.insert("audio".to_string(), broadcast.create_track("audio",
            hang::container::track_info(hang::catalog::PRIORITY.audio))?);
    }
    let producer = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_nanos(1_000_000_000 / 30));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        use multiview_moq::{Packet, VideoAdapter, Window};
        let mut adapter = multiview_moq::av::AvAdapter::new(VideoAdapter::new(video, 1), audio_tracks);
        let mut audio_index = 0u64;
        for index in 0u64.. {
            tick.tick().await;
            let payload = frames[index as usize % frames.len()].clone();
            let verified_rap = payload[0] & 1 == 0;
            let mut audio = std::collections::BTreeMap::new();
            if !audio_packets.is_empty() {
                let mut packets = Vec::new();
                while audio_index * 20000 < (index + 1) * 1_000_000 / 30 {
                    packets.push(Packet {payload: audio_packets[audio_index as usize % audio_packets.len()].clone(), pts_us: audio_index * 20000, verified_rap:true});
                    audio_index += 1;
                }
                audio.insert("audio".to_string(), packets);
            }
            adapter
                .publish(&multiview_moq::av::AvWindow { audio, video: Window {
                    generation: 1,
                    packets: vec![Packet {
                        payload,
                        pts_us: index * 1_000_000 / 30,
                        verified_rap,
                    }],
                }})
                .map_err(|error| anyhow::anyhow!("publication failed: {error:?}"))?;
        }
        Ok::<(), anyhow::Error>(())
    });
    let mut config = moq_tokio::connect::Config::default();
    config.version = vec![protocol.parse().map_err(anyhow::Error::msg)?];
    config.once = Some(true);
    let client = config.init(Default::default()).map_err(|_| anyhow::anyhow!("client configuration failed"))?.with_publisher(origin.consume());
    let url: url::Url = endpoint.parse().map_err(|_: _| anyhow::anyhow!("endpoint invalid"))?;
    let connection = match tokio::time::timeout(Duration::from_secs(40), client.connect(url).established()).await {
        Ok(Ok(connection)) => connection,
        Ok(Err(_)) => { producer.abort(); anyhow::bail!("CF_AUTHENTICATED_CONNECT_FAILED"); },
        Err(_) => { producer.abort(); anyhow::bail!("CF_AUTHENTICATED_CONNECT_TIMEOUT"); }
    };
    ensure!(connection.version().map(|v|v.to_string()).as_deref() == Some(protocol), "CF_PROTOCOL_MISMATCH");
    println!("CF_PUBLISH_CONNECTED protocol={protocol} broadcast=fixture");
    tokio::select! { _ = tokio::time::sleep(Duration::from_secs(300)) => {}, _ = connection.closed() => { println!("CF_PUBLISH_CLOSED"); } }
    producer.abort();
    drop((connection, catalog, broadcast, origin));
    Ok(())
}
