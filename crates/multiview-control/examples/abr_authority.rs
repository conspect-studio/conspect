//! Explicit test fixture: real control router, registry fences and shared MoQ tracks.
//! No encoder is created. All subscribers consume the same immutable IVF packets.
#[path = "../tests/support/mod.rs"]
mod support;
use multiview_control::{
    moq::{MoqRegistry, MoqReject, MoqVideo, MoqVideoAlternative},
    moq_provider::{
        AuthorizedFeed, AuthorizedFeedFactory, AuthorizedSharedFeed, WebTransportProvider,
    },
    preview::WhepScope,
    Principal,
};
use multiview_moq::{
    ladder::{RenditionSpec, SharedLadder},
    provider::{Fence, LadderFeed, Runtime},
    publication::LadderWindow,
    Packet, Window,
};
use std::{
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

struct Fixture {
    ladder: SharedLadder,
    frames: Vec<Vec<bytes::Bytes>>,
    upstream_clock_unknown: bool,
    sinks: Mutex<Vec<(Weak<Fence>, bool, [u64; 5])>>,
}

const BRIDGE_PACKET_MAX: usize = 256 * 1024;
fn bridge_pts(value: u128) -> Option<u64> {
    u64::try_from(value).ok()
}
fn vp8_rap(payload: &[u8], width: u16, height: u16) -> bool {
    if payload.len() < 10 || payload[0] & 1 != 0 || payload[3..6] != [0x9d, 0x01, 0x2a] {
        return false;
    }
    let partition = u32::from_le_bytes([payload[0], payload[1], payload[2], 0]) >> 5;
    partition as usize <= payload.len() - 10
        && partition > 0
        && u16::from_le_bytes([payload[6], payload[7]]) & 0x3fff == width
        && u16::from_le_bytes([payload[8], payload[9]]) & 0x3fff == height
}

#[derive(Default)]
struct BridgeMerge {
    pending: std::collections::BTreeMap<u64, Vec<Option<Packet>>>,
    last: Option<u64>,
    gap: bool,
}
impl BridgeMerge {
    fn offer(&mut self, level: usize, packet: Packet) -> Option<LadderWindow> {
        let pts = packet.pts_us;
        if level >= 3 || self.last.is_some_and(|last| pts <= last) {
            self.gap = true;
            return None;
        }
        let rungs = self
            .pending
            .entry(pts)
            .or_insert_with(|| vec![None, None, None]);
        if rungs[level].is_some() {
            self.gap = true;
            return None;
        }
        rungs[level] = Some(packet);
        if rungs.iter().all(Option::is_some) {
            let rungs = self.pending.remove(&pts).unwrap();
            if self.pending.keys().any(|time| *time < pts) {
                self.gap = true;
            }
            self.pending.retain(|time, _| *time > pts);
            self.last = Some(pts);
            let mut window = LadderWindow::new(
                1,
                rungs
                    .into_iter()
                    .map(|p| Window {
                        generation: 1,
                        packets: vec![p.unwrap()],
                    })
                    .collect(),
            );
            if self.gap {
                window.discontinuities = 0b111;
                // Never forward a delta after known loss, even before the
                // production publication's independent discontinuity gate.
                for rung in &mut window.renditions {
                    if !rung.packets[0].verified_rap {
                        rung.packets.clear();
                    }
                }
                if window.renditions.iter().all(|r| !r.packets.is_empty()) {
                    self.gap = false;
                }
            }
            return Some(window);
        }
        while self.pending.len() > 12 {
            self.pending.pop_first();
            self.gap = true;
        }
        None
    }
}

// One incoming relay session and one family fan out immutable packets. This is
// a relayed gateway proof, never browser-direct Cloudflare authorization.
fn private_handoff(path: &str) -> Result<serde_json::Value, &'static str> {
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;
    let fd = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|_| "upstream handoff open failed")?;
    let mut file = std::fs::File::from(fd);
    let metadata = file
        .metadata()
        .map_err(|_| "upstream handoff metadata failed")?;
    if !metadata.is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
        || metadata.len() > 16384
    {
        return Err("private upstream handoff required");
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(16385)
        .read_to_end(&mut bytes)
        .map_err(|_| "upstream handoff read failed")?;
    if bytes.len() > 16384 {
        return Err("upstream handoff too large");
    }
    serde_json::from_slice(&bytes).map_err(|_| "upstream handoff JSON invalid")
}
async fn relay_windows(
    path: &str,
) -> Result<tokio::sync::mpsc::Receiver<LadderWindow>, Box<dyn std::error::Error>> {
    let handoff = private_handoff(path)?;
    let endpoint = handoff["subscribe_url"]
        .as_str()
        .ok_or("missing upstream endpoint")?;
    let origin = moq_tokio::origin::spawn();
    let mut config = moq_tokio::connect::Config::default();
    config.version = vec!["moq-transport-16".parse().map_err(|_| "invalid draft")?];
    config.once = Some(true);
    if let Some(root) = handoff["tls_root"].as_str() {
        config.tls.root = vec![root.into()];
    }
    let client = config
        .init(Default::default())
        .map_err(|_| "upstream configuration failed")?
        .with_subscriber(origin.clone());
    let session = client
        .connect(
            endpoint
                .parse::<url::Url>()
                .map_err(|_| "invalid upstream endpoint")?,
        )
        .established()
        .await
        .map_err(|_| "upstream connection failed")?;
    let broadcast = tokio::time::timeout(
        Duration::from_secs(15),
        origin.consume().routed_broadcast("fixture"),
    )
    .await
    .map_err(|_| "upstream family deadline")?
    .map_err(|_| "upstream family missing")?;
    // 12 * 256KiB packet channel + at most 12 pending triples: <13MiB
    // bridge-retained payload. Pinned transport separately caps a received frame
    // before allocation at group::MAX_CACHE_BYTES (32MiB), not unlimited.
    let (packets_tx, mut packets_rx) = tokio::sync::mpsc::channel::<(usize, Packet)>(12);
    let mut readers = tokio::task::JoinSet::new();
    for (level, name) in ["video-low", "video-mid", "video-high"]
        .into_iter()
        .enumerate()
    {
        let mut track = broadcast
            .track(name)
            .map_err(|_| "upstream rung missing")?
            .subscribe(moq_net::track::Subscription::default())
            .await
            .map_err(|_| "upstream subscribe failed")?;
        let tx = packets_tx.clone();
        readers.spawn(async move {
            while let Ok(Some(mut group)) = track.recv_group().await {
                while let Ok(Some(frame)) = group.read_frame().await {
                    let Ok(decoded) = hang::container::Frame::decode(frame.payload) else {
                        return;
                    };
                    let payload = decoded.payload;
                    if payload.is_empty() || payload.len() > BRIDGE_PACKET_MAX {
                        return;
                    }
                    let Some(pts_us) = bridge_pts(decoded.timestamp.as_micros()) else {
                        return;
                    };
                    let (width, height) = [(160, 90), (320, 180), (640, 360)][level];
                    let packet = Packet {
                        verified_rap: vp8_rap(&payload, width, height),
                        payload,
                        pts_us,
                    };
                    if tx.send((level, packet)).await.is_err() {
                        return;
                    }
                }
            }
        });
    }
    drop(packets_tx);
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tokio::spawn(async move {
        let _session = session;
        let _origin = origin;
        let mut merge = BridgeMerge::default();
        loop {
            let next =
                tokio::select! { _ = tx.closed() => break, value = packets_rx.recv() => value };
            let Some((level, packet)) = next else {
                break;
            };
            if let Some(window) = merge.offer(level, packet) {
                if tx.send(window).await.is_err() {
                    break;
                }
            }
        }
        readers.abort_all();
        while readers.join_next().await.is_some() {}
        _session.abort(moq_net::Error::Cancel);
        let _ = tokio::time::timeout(Duration::from_secs(1), _session.closed()).await;
    });
    Ok(rx)
}
impl AuthorizedFeedFactory for Fixture {
    fn available(&self) -> bool {
        true
    }
    fn attach(
        &self,
        _: &Principal,
        _: &WhepScope,
        _: Arc<Fence>,
    ) -> Result<AuthorizedFeed, MoqReject> {
        Err(MoqReject::Unavailable)
    }
    fn attach_shared(
        &self,
        principal: &Principal,
        scope: &WhepScope,
        sink: Arc<Fence>,
    ) -> Result<Option<AuthorizedSharedFeed>, MoqReject> {
        if !principal.is_global()
            || !principal.role.can(multiview_control::auth::Action::Write)
            || !matches!(scope, WhepScope::Program)
        {
            return Err(MoqReject::Unavailable);
        }
        self.sinks
            .lock()
            .unwrap()
            .push((Arc::downgrade(&sink), false, [0; 5]));
        let (_, windows) = Fence::ladder_channel(4);
        let alternatives = self
            .ladder
            .renditions()
            .iter()
            .map(|r| MoqVideoAlternative {
                track: r.name().into(),
                codec: r.codec().into(),
                coded_width: r.coded_width(),
                coded_height: r.coded_height(),
                cadence_numerator: 30,
                cadence_denominator: 1,
                bitrate_bps: r.bitrate_bps(),
                description: None,
            })
            .collect();
        Ok(Some(AuthorizedSharedFeed {
            feed: LadderFeed {
                ladder: self.ladder.clone(),
                capacity_bps: Some(20_000_000),
                config: Default::default(),
                windows,
                demand: Box::new(()),
            },
            video: MoqVideo {
                codec: "vp8".into(),
                coded_width: 160,
                coded_height: 90,
                description: None,
            },
            audio: vec![],
            alternatives,
            clock_wall_micros: (!self.upstream_clock_unknown).then(|| self.ladder.clock_wall_us()),
        }))
    }
}
fn ivf(path: &str) -> Result<(u32, u32, Vec<bytes::Bytes>), Box<dyn std::error::Error>> {
    let data = std::fs::read(path)?;
    if data.len() < 32 || &data[..4] != b"DKIF" || &data[8..12] != b"VP80" {
        return Err("invalid VP8 IVF".into());
    }
    let width = u16::from_le_bytes(data[12..14].try_into()?) as u32;
    let height = u16::from_le_bytes(data[14..16].try_into()?) as u32;
    let mut offset = 32;
    let mut frames = vec![];
    while offset < data.len() {
        if offset + 12 > data.len() {
            return Err("truncated IVF header".into());
        }
        let size = u32::from_le_bytes(data[offset..offset + 4].try_into()?) as usize;
        offset += 12;
        if size == 0 || offset + size > data.len() {
            return Err("invalid IVF payload".into());
        }
        frames.push(bytes::Bytes::copy_from_slice(&data[offset..offset + size]));
        offset += size;
    }
    if frames.is_empty() || frames[0][0] & 1 != 0 {
        return Err("missing initial RAP".into());
    }
    Ok((width, height, frames))
}
#[cfg(test)]
mod bridge_tests {
    use super::*;
    #[test]
    fn private_handoff_uses_regular_owned_private_fd_and_rejects_symlink() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private.json");
        std::fs::write(&path, b"{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(private_handoff(path.to_str().unwrap()).is_ok());
        let link = dir.path().join("link");
        symlink(&path, &link).unwrap();
        assert!(private_handoff(link.to_str().unwrap()).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(private_handoff(path.to_str().unwrap()).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(&path, vec![b' '; 16385]).unwrap();
        assert!(private_handoff(path.to_str().unwrap()).is_err());
    }
    fn packet(pts: u64, rap: bool) -> Packet {
        Packet {
            payload: bytes::Bytes::from_static(b"vp8"),
            pts_us: pts,
            verified_rap: rap,
        }
    }
    fn complete(merge: &mut BridgeMerge, pts: u64, rap: bool) -> LadderWindow {
        assert!(merge.offer(0, packet(pts, rap)).is_none());
        assert!(merge.offer(1, packet(pts, rap)).is_none());
        merge.offer(2, packet(pts, rap)).unwrap()
    }
    #[test]
    fn late_packet_marks_partial_next_complete_and_gates_deltas_until_rap() {
        let mut m = BridgeMerge::default();
        complete(&mut m, 2, true);
        m.offer(0, packet(3, false));
        m.offer(1, packet(3, false));
        m.offer(0, packet(2, false));
        let next = m.offer(2, packet(3, false)).unwrap();
        assert_eq!(next.discontinuities, 7);
        assert!(next.renditions.iter().all(|r| r.packets.is_empty()));
        let rap = complete(&mut m, 4, true);
        assert_eq!(rap.discontinuities, 7);
        assert!(rap.renditions.iter().all(|r| r.packets[0].verified_rap));
        assert_eq!(complete(&mut m, 5, false).discontinuities, 0);
    }
    #[test]
    fn older_partial_discard_duplicate_and_eviction_each_mark_gap() {
        let mut m = BridgeMerge::default();
        m.offer(0, packet(2, false));
        assert_eq!(complete(&mut m, 3, true).discontinuities, 7);
        m.offer(0, packet(4, false));
        m.offer(0, packet(4, false));
        m.offer(1, packet(4, true));
        assert_eq!(m.offer(2, packet(4, true)).unwrap().discontinuities, 7);
        let mut m = BridgeMerge::default();
        for pts in 0..13 {
            m.offer(0, packet(pts, false));
        }
        assert!(m.pending.len() <= 12);
        assert_eq!(complete(&mut m, 20, true).discontinuities, 7);
    }
    #[test]
    fn timestamp_narrowing_is_checked() {
        assert_eq!(bridge_pts(u64::MAX as u128), Some(u64::MAX));
        assert_eq!(bridge_pts(u64::MAX as u128 + 1), None);
    }
    #[test]
    fn rap_requires_partition_and_exact_granted_dimensions() {
        let mut p = vec![0; 11];
        p[0] = 0x20;
        p[3..6].copy_from_slice(&[0x9d, 1, 0x2a]);
        p[6..8].copy_from_slice(&160u16.to_le_bytes());
        p[8..10].copy_from_slice(&90u16.to_le_bytes());
        assert!(vp8_rap(&p, 160, 90));
        assert!(!vp8_rap(&p, 320, 180));
        p[0] = 0xe0;
        assert!(!vp8_rap(&p, 160, 90));
        p[0] = 0x21;
        assert!(!vp8_rap(&p, 160, 90));
    }
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 6 && args.len() != 7 {
        return Err("cert key low.ivf mid.ivf high.ivf".into());
    }
    let mut specs = vec![];
    let mut frames = vec![];
    for (index, path) in args[3..6].iter().enumerate() {
        let (width, height, rung) = ivf(path)?;
        let bitrate =
            (rung.iter().map(|f| f.len() as u64).sum::<u64>() * 8 * 30) / (rung.len() as u64);
        specs.push(RenditionSpec::new(
            ["video-low", "video-mid", "video-high"][index],
            "vp8",
            width,
            height,
            30,
            1,
            bitrate,
        )?);
        frames.push(rung);
    }
    let wall = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_micros() as u64;
    let fixture = Arc::new(Fixture {
        ladder: SharedLadder::admit(1, specs, vec![], wall)?,
        frames,
        upstream_clock_unknown: args.len() == 7,
        sinks: Mutex::new(vec![]),
    });
    let runtime = Runtime::listen_files(
        "[::]:4443".parse()?,
        "https://localhost:4443".into(),
        "moq-transport-16",
        args[1].clone().into(),
        args[2].clone().into(),
    )
    .await?;
    let registry = Arc::new(MoqRegistry::new(
        Arc::new(WebTransportProvider::new(runtime, fixture.clone())),
        Duration::from_secs(50),
        8,
    ));
    let ladder = fixture
        .ladder
        .renditions()
        .iter()
        .map(|r| {
            format!(
                "{{\"track\":\"{}\",\"bitrate\":{}}}",
                r.name(),
                r.bitrate_bps()
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let mut upstream = if let Some(path) = args.get(6) {
        match relay_windows(path).await {
            Ok(receiver) => Some(receiver),
            Err(error) => {
                // Every bridge error is a fixed local label, never a transport URL.
                eprintln!("ABR_UPSTREAM_SETUP_FAILED label={error}");
                return Err("upstream bridge setup failed".into());
            }
        }
    } else {
        None
    };
    println!(
        "ABR_FIXTURE_READY url=https://localhost:4443 broadcast=media protocol=moq-transport-16 ticks=90 ladder=[{ladder}]"
    );
    let harness = support::harness_with(|state| state.with_moq(registry));
    let producer = fixture.clone();
    tokio::spawn(async move {
        let mut clock = tokio::time::interval(Duration::from_nanos(1_000_000_000 / 30));
        for tick in 0u64.. {
            clock.tick().await;
            let renditions = producer
                .frames
                .iter()
                .map(|frames| {
                    let payload = frames[tick as usize % frames.len()].clone();
                    Window {
                        generation: 1,
                        packets: vec![Packet {
                            verified_rap: payload.len() >= 10
                                && payload[0] & 1 == 0
                                && payload[3..6] == [0x9d, 0x01, 0x2a],
                            payload,
                            pts_us: tick * 1_000_000 / 30,
                        }],
                    }
                })
                .collect();
            if tick % 30 == 0 {
                let t = tick / 30;
                let fields = producer
                    .frames
                    .iter()
                    .enumerate()
                    .map(|(i, frames)| {
                        let cycles = tick / frames.len() as u64;
                        let rest = tick as usize % frames.len();
                        let bytes = cycles * frames.iter().map(|f| f.len() as u64).sum::<u64>()
                            + frames[..rest].iter().map(|f| f.len() as u64).sum::<u64>();
                        format!(
                            "{}={}/{}",
                            ["video-low", "video-mid", "video-high"][i],
                            tick,
                            bytes
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                println!("ABR_PUBLISH_STATS t={t} {fields}");
            }
            let window = if let Some(receiver) = upstream.as_mut() {
                let Some(window) = receiver.recv().await else {
                    eprintln!("ABR_UPSTREAM_ENDED");
                    break;
                };
                window
            } else {
                LadderWindow::new(1, renditions)
            };
            producer
                .sinks
                .lock()
                .unwrap()
                .retain_mut(|(weak, started, counts)| {
                    if let Some(sink) = weak.upgrade() {
                        // New subscriptions join only at one common verified VP8 RAP.
                        if !*started && !window.renditions.iter().all(|r| r.packets.first().is_some_and(|packet| packet.verified_rap))
                        {
                            return true;
                        }
                        *started = true;
                        use multiview_moq::provider::OfferRefusal;
                        let (index, terminal) = match sink.offer_ladder(window.clone()) {
                            Ok(()) => (0, false),
                            Err(OfferRefusal::Full(_)) => (1, false),
                            Err(OfferRefusal::Revoked(_)) => (2, true),
                            Err(OfferRefusal::Shape(_)) => (3, true),
                            Err(OfferRefusal::Hard(_)) => (4, true),
                        };
                        counts[index] += 1;
                        if tick % 30 == 0 || terminal {
                            eprintln!("ABR_OFFER_COUNTS tick={tick} accepted={} full={} revoked={} shape={} hard={} terminal={:?}", counts[0], counts[1], counts[2], counts[3], counts[4], sink.terminal_event());
                        }
                        !terminal
                    } else {
                        false
                    }
                });
        }
    });
    let listener = tokio::net::TcpListener::bind("[::1]:4445").await?;
    eprintln!("ABR_AUTHORITY_READY http_port=4445 quic_port=4443 rungs=3 fixture_only=true");
    axum::serve(listener, harness.router)
        .with_graceful_shutdown(async { tokio::time::sleep(Duration::from_secs(240)).await })
        .await?;
    Ok(())
}
