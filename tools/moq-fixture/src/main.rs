//! Development-only, shared pre-encoded VP8/H264 publisher. No runtime encoder.
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

fn h264_frames(data: &[u8]) -> anyhow::Result<Vec<Bytes>> {
    ensure!(data.starts_with(b"H264"), "expected H264 packet fixture");
    let mut offset = 4;
    let mut frames = Vec::new();
    while offset < data.len() {
        ensure!(offset + 4 <= data.len(), "truncated H264 length");
        let len = u32::from_le_bytes(data[offset..offset + 4].try_into()?) as usize;
        offset += 4;
        ensure!(
            len > 0 && len <= 1024 * 1024 && offset + len <= data.len(),
            "invalid H264 length"
        );
        let packet = &data[offset..offset + len];
        for nal in [7u8, 8, 5] {
            ensure!(
                packet
                    .windows(4)
                    .any(|w| w[..3] == [0, 0, 1] && w[3] & 31 == nal),
                "all-IDR fixture requires SPS/PPS/IDR in each access unit"
            );
        }
        frames.push(Bytes::copy_from_slice(packet));
        offset += len;
    }
    ensure!(!frames.is_empty(), "empty H264 fixture");
    Ok(frames)
}

#[tokio::main(worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    ensure!(
        args.len() == 5 || args.len() == 6,
        "usage: conspect-moq-fixture <16|18> <fixture.ivf> <cert.pem> <key.pem> [audio.packets]"
    );
    let protocol = match args[1].as_str() {
        "16" => "moq-transport-16",
        "18" => "moq-transport-18",
        _ => anyhow::bail!("only explicit drafts 16 and 18 supported"),
    };
    use std::io::Read;
    let mut data = Vec::new();
    std::fs::File::open(&args[2])?
        .take(16 * 1024 * 1024 + 1)
        .read_to_end(&mut data)?;
    ensure!(data.len() <= 16 * 1024 * 1024, "fixture exceeds 16MiB");
    let h264 = data.starts_with(b"H264");
    let frames = if h264 {
        h264_frames(&data)?
    } else {
        frames(&data)?
    };
    let mut audio_packets = Vec::new();
    if let Some(path) = args.get(5) {
        let bytes = std::fs::read(path)?;
        ensure!(bytes.len() <= 1024 * 1024, "audio fixture too large");
        let mut offset = 0;
        while offset < bytes.len() {
            ensure!(offset + 4 <= bytes.len(), "audio header truncated");
            let len = u32::from_le_bytes(bytes[offset..offset + 4].try_into()?) as usize;
            offset += 4;
            ensure!(
                len > 0 && len <= 65536 && offset + len <= bytes.len(),
                "audio packet invalid"
            );
            audio_packets.push(Bytes::copy_from_slice(&bytes[offset..offset + len]));
            offset += len;
        }
        ensure!(!audio_packets.is_empty(), "audio fixture empty");
    }
    let mut config = moq_tokio::listen::Config::default();
    config.bind = Some("[::]:4443".parse()?);
    config.version = vec![protocol.parse().map_err(anyhow::Error::msg)?];
    config.tls.cert = vec![args[3].clone().into()];
    config.tls.key = vec![args[4].clone().into()];
    let mut server = config.init(Default::default())?.listen().await?;
    let origin = moq_tokio::origin::spawn();
    let broadcast = origin.create_broadcast("fixture")?;
    broadcast.announce(Default::default())?;
    let mut catalog = broadcast.create_track(
        "catalog.json",
        hang::catalog::Catalog::<()>::default_track_info(),
    )?;
    let video_json = br#"{"video":{"renditions":{"video":{"codec":"vp8","codedWidth":320,"codedHeight":180,"framerate":30}}},"audio":{"renditions":{}}}"#;
    let av_json = br#"{"video":{"renditions":{"video":{"codec":"vp8","codedWidth":320,"codedHeight":180,"framerate":30}}},"audio":{"renditions":{"audio":{"codec":"opus","sampleRate":48000,"numberOfChannels":2}}}}"#;
    let json: &[u8] = if audio_packets.is_empty() {
        video_json
    } else {
        av_json
    };
    let json =
        String::from_utf8(json.to_vec())?.replace("vp8", if h264 { "avc1.42c01e" } else { "vp8" });
    let _: hang::catalog::Catalog = hang::catalog::Catalog::from_slice(json.as_bytes())?;
    catalog.write_frame(moq_net::Timestamp::from_millis(0)?, Bytes::from(json))?;
    let video = broadcast.create_track(
        "video",
        hang::container::track_info(hang::catalog::PRIORITY.video)
            .with_max_age(Duration::from_secs(2)),
    )?;
    let mut audio_tracks = std::collections::BTreeMap::new();
    if !audio_packets.is_empty() {
        audio_tracks.insert(
            "audio".to_string(),
            broadcast.create_track(
                "audio",
                hang::container::track_info(hang::catalog::PRIORITY.audio),
            )?,
        );
    }
    let producer = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_nanos(1_000_000_000 / 30));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        use multiview_moq::{Packet, VideoAdapter, Window};
        let mut adapter =
            multiview_moq::av::AvAdapter::new(VideoAdapter::new(video, 1), audio_tracks);
        let mut audio_index = 0u64;
        for index in 0u64.. {
            tick.tick().await;
            let payload = frames[index as usize % frames.len()].clone();
            let verified_rap = h264 || payload[0] & 1 == 0;
            let mut audio = std::collections::BTreeMap::new();
            if !audio_packets.is_empty() {
                let mut packets = Vec::new();
                while audio_index * 20000 < (index + 1) * 1_000_000 / 30 {
                    packets.push(Packet {
                        payload: audio_packets[audio_index as usize % audio_packets.len()].clone(),
                        pts_us: audio_index * 20000,
                        verified_rap: true,
                    });
                    audio_index += 1;
                }
                audio.insert("audio".to_string(), packets);
            }
            adapter
                .publish(&multiview_moq::av::AvWindow {
                    audio,
                    video: Window {
                        generation: 1,
                        packets: vec![Packet {
                            payload,
                            pts_us: index * 1_000_000 / 30,
                            verified_rap,
                        }],
                    },
                })
                .map_err(|error| anyhow::anyhow!("publication failed: {error:?}"))?;
        }
        Ok::<(), anyhow::Error>(())
    });
    eprintln!("fixture ready: https://localhost:4443 broadcast=fixture protocol={protocol}");
    let mut sessions = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            Some(_) = sessions.join_next(), if !sessions.is_empty() => {},
            request = server.accept() => {
                let Some(request) = request else { break };
                if sessions.len() >= 8 { request.reject(moq_tokio::server::Reject::App(429)).await?; continue; }
                let origin = origin.consume();
                sessions.spawn(async move {
                    let session = request.with_publisher(origin).ok().await?;
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    drop(session);
                    Ok::<(), anyhow::Error>(())
                });
            }
        }
    }
    producer.abort();
    sessions.abort_all();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_truncated_and_wrong_codec() {
        assert!(frames(b"DKIF").is_err());
        assert!(frames(&[0; 32]).is_err());
    }
    fn ivf(payload: &[u8]) -> Vec<u8> {
        let mut data = vec![0; 32];
        data[..4].copy_from_slice(b"DKIF");
        data[8..12].copy_from_slice(b"VP80");
        data[12..14].copy_from_slice(&320u16.to_le_bytes());
        data[14..16].copy_from_slice(&180u16.to_le_bytes());
        data.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        data.extend_from_slice(&[0; 8]);
        data.extend_from_slice(payload);
        data
    }
    #[test]
    fn parses_shared_keyframe_and_rejects_partial_or_delta_start() {
        assert_eq!(frames(&ivf(&[0, 2, 3])).unwrap()[0].as_ref(), &[0, 2, 3]);
        assert!(frames(&ivf(&[1, 2, 3])).is_err());
        let mut truncated = ivf(&[0, 2, 3]);
        truncated.pop();
        assert!(frames(&truncated).is_err());
        assert!(frames(&ivf(&[])).is_err());
    }
    #[test]
    fn h264_requires_complete_independent_access_units() {
        assert!(h264_frames(b"H264").is_err());
        assert!(h264_frames(b"H264\x01").is_err());
        let payload = [0, 0, 1, 0x67, 0, 0, 1, 0x68, 0, 0, 1, 0x65];
        let mut bytes = b"H264".to_vec();
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&payload);
        assert_eq!(h264_frames(&bytes).unwrap().len(), 1);
        bytes.pop();
        assert!(h264_frames(&bytes).is_err());
    }
    #[test]
    fn exact_drafts_parse_without_downgrade() {
        for name in ["moq-transport-16", "moq-transport-18"] {
            assert_eq!(name.parse::<moq_net::Version>().unwrap().to_string(), name);
        }
    }
}
