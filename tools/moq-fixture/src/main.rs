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
    ensure!(
        args.len() == 5,
        "usage: conspect-moq-fixture <16|18> <fixture.ivf> <cert.pem> <key.pem>"
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
    let frames = frames(&data)?;
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
    let json = br#"{"video":{"renditions":{"video":{"codec":"vp8","codedWidth":320,"codedHeight":180,"framerate":30}}},"audio":{"renditions":{}}}"#;
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
    let producer = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_nanos(1_000_000_000 / 30));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        use conspect_moq_fixture::{Packet, VideoAdapter, Window};
        let mut adapter = VideoAdapter::new(video, 1);
        for index in 0u64.. {
            tick.tick().await;
            let payload = frames[index as usize % frames.len()].clone();
            let verified_rap = payload[0] & 1 == 0;
            adapter
                .publish(&Window {
                    generation: 1,
                    packets: vec![Packet {
                        payload,
                        pts_us: index * 1_000_000 / 30,
                        verified_rap,
                    }],
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
    fn exact_drafts_parse_without_downgrade() {
        for name in ["moq-transport-16", "moq-transport-18"] {
            assert_eq!(name.parse::<moq_net::Version>().unwrap().to_string(), name);
        }
    }
}
