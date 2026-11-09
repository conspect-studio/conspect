//! Authenticated local transport runtime. Control authorizes before `admit`.
use crate::{
    VideoAdapter,
    av::{AvAdapter, AvWindow},
};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{mpsc, watch};

/// Already authorized immutable binding. The factory owns all source-demand
/// resources until its returned guard is dropped; never creates encoders.
pub struct Feed {
    pub generation: u64,
    pub catalog: bytes::Bytes,
    pub audio_tracks: Vec<String>,
    pub windows: mpsc::Receiver<AvWindow>,
    pub demand: Box<dyn Send + Sync>,
}

/// Per-subscription synchronous producer fence. `try_offer` and `revoke` share
/// the same small critical section; no transport operation runs under it.
static RETAINED_BYTES: AtomicUsize = AtomicUsize::new(0);
const GLOBAL_BYTES: usize = 64 * 1024 * 1024;
const SUBSCRIPTION_BYTES: usize = 8 * 1024 * 1024;

// Bytes::from_owner carries the charge through every library clone/slice and
// refunds only when the final retained payload reference actually drops.
struct ChargedPayload {
    payload: bytes::Bytes,
    charge: usize,
    subscription: Arc<AtomicUsize>,
}
impl AsRef<[u8]> for ChargedPayload {
    fn as_ref(&self) -> &[u8] {
        &self.payload
    }
}
impl Drop for ChargedPayload {
    fn drop(&mut self) {
        self.subscription.fetch_sub(self.charge, Ordering::AcqRel);
        RETAINED_BYTES.fetch_sub(self.charge, Ordering::AcqRel);
    }
}

/// Reserve the complete serialized object before allocation. Its owner retains
/// the original admission charge as well as the extra framing allocation.
pub(crate) fn serialized_frame(packet: &crate::Packet) -> Result<bytes::Bytes, crate::Reject> {
    let timestamp = moq_net::Timestamp::new(packet.pts_us, moq_net::Timescale::MICRO)
        .map_err(|_| crate::Reject::Timestamp)?;
    let maximum = packet
        .payload
        .len()
        .checked_add(8)
        .ok_or(crate::Reject::Budget)?;
    RETAINED_BYTES
        .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
            used.checked_add(maximum).filter(|sum| *sum <= GLOBAL_BYTES)
        })
        .map_err(|_| crate::Reject::Budget)?;
    struct Serialized {
        data: Vec<u8>,
        _source: bytes::Bytes,
        charge: usize,
    }
    impl AsRef<[u8]> for Serialized {
        fn as_ref(&self) -> &[u8] {
            &self.data
        }
    }
    impl Drop for Serialized {
        fn drop(&mut self) {
            RETAINED_BYTES.fetch_sub(self.charge, Ordering::AcqRel);
        }
    }
    let mut owner = Serialized {
        data: Vec::with_capacity(maximum),
        _source: packet.payload.clone(),
        charge: maximum,
    };
    hang::container::Frame {
        timestamp,
        payload: packet.payload.clone(),
    }
    .encode(&mut owner.data)
    .map_err(|_| crate::Reject::Transport)?;
    Ok(bytes::Bytes::from_owner(owner))
}

pub struct Fence {
    charged: Arc<AtomicUsize>,
    sender: Mutex<Option<mpsc::Sender<AvWindow>>>,
    cancel: watch::Sender<bool>,
}
impl Fence {
    pub fn channel(capacity: usize) -> (Arc<Self>, mpsc::Receiver<AvWindow>) {
        let (sender, receiver) = mpsc::channel(capacity.clamp(1, 8));
        let (cancel, _) = watch::channel(false);
        (
            Arc::new(Self {
                charged: Arc::new(AtomicUsize::new(0)),
                sender: Mutex::new(Some(sender)),
                cancel,
            }),
            receiver,
        )
    }
    /// The authorized feed owner must use this instead of retaining a raw sender.
    pub fn try_offer(&self, mut window: AvWindow) -> Result<(), AvWindow> {
        if *self.cancel.borrow() {
            return Err(window);
        }
        // Bound referenced media before it can enter the queue. A full queue is
        // terminal for this subscription, avoiding delivery after a hidden gap.
        let packets = window
            .video
            .packets
            .iter()
            .chain(window.audio.values().flatten());
        let mut bytes = 0usize;
        let mut count = 0usize;
        for packet in packets {
            count += 1;
            let Some(total) = bytes.checked_add(packet.payload.len()) else {
                self.cancel.send_replace(true);
                return Err(window);
            };
            bytes = total;
            if bytes > VideoAdapter::MAX_BYTES || count > 512 {
                self.cancel.send_replace(true);
                return Err(window);
            }
        }
        let Ok(sender) = self.sender.try_lock() else {
            self.cancel.send_replace(true);
            return Err(window);
        };
        // Reserve before retention. The owner wrapper follows every Bytes clone
        // into queue, publisher cache, and transport; final drop refunds credit.
        if self.charged.load(Ordering::Acquire).saturating_add(bytes) > SUBSCRIPTION_BYTES
            || RETAINED_BYTES
                .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                    used.checked_add(bytes).filter(|sum| *sum <= GLOBAL_BYTES)
                })
                .is_err()
        {
            self.cancel.send_replace(true);
            return Err(window);
        }
        self.charged.fetch_add(bytes, Ordering::AcqRel);
        for packet in window
            .video
            .packets
            .iter_mut()
            .chain(window.audio.values_mut().flatten())
        {
            let payload = std::mem::take(&mut packet.payload);
            packet.payload = bytes::Bytes::from_owner(ChargedPayload {
                charge: payload.len(),
                payload,
                subscription: self.charged.clone(),
            });
        }
        match sender.as_ref() {
            Some(sender) => sender.try_send(window).map_err(|e| {
                self.cancel.send_replace(true);
                e.into_inner()
            }),
            None => Err(window),
        }
    }
    pub fn revoke(&self) {
        if let Ok(mut sender) = self.sender.lock() {
            sender.take();
        }
        self.cancel.send_replace(true);
    }
}

struct Entry {
    origin: moq_net::origin::Consumer,
    fence: Arc<Fence>,
    used: bool,
}
struct State {
    entries: Mutex<HashMap<String, Entry>>,
    ready: AtomicBool,
    shutdown: watch::Sender<bool>,
}
struct SubscriptionCleanup {
    state: Arc<State>,
    token: String,
    fence: Arc<Fence>,
}
impl Drop for SubscriptionCleanup {
    fn drop(&mut self) {
        self.fence.revoke();
        if let Ok(mut entries) = self.state.entries.lock() {
            entries.remove(&self.token);
        }
    }
}

/// Listener is ready only after a successful bind; no default synthetic feeds.
pub struct Runtime {
    state: Arc<State>,
    base_url: String,
    protocol: String,
}
pub struct Admission {
    pub url: String,
    pub broadcast: String,
    pub generation: u64,
    pub protocol: String,
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.state.ready.store(false, Ordering::Release);
        self.state.shutdown.send_replace(true);
    }
}

impl Runtime {
    pub async fn listen_files(
        bind: std::net::SocketAddr,
        endpoint: String,
        protocol: &str,
        cert: std::path::PathBuf,
        key: std::path::PathBuf,
    ) -> anyhow::Result<Arc<Self>> {
        anyhow::ensure!(bind.is_ipv6(), "IPv6-first listener required");
        let mut config = moq_tokio::listen::Config::default();
        config.bind = Some(moq_tokio::listen::Bind::Addr(bind));
        config.version = vec![protocol.parse().map_err(anyhow::Error::msg)?];
        config.tls.cert = vec![cert];
        config.tls.key = vec![key];
        Self::listen(config, endpoint, protocol).await
    }
    pub async fn listen(
        config: moq_tokio::listen::Config,
        base_url: String,
        protocol: &str,
    ) -> anyhow::Result<Arc<Self>> {
        anyhow::ensure!(
            matches!(protocol, "moq-transport-16" | "moq-transport-18"),
            "unsupported protocol"
        );
        anyhow::ensure!(
            base_url.starts_with("https://") && !base_url.contains('?') && !base_url.contains('#'),
            "invalid endpoint"
        );
        anyhow::ensure!(
            config.version.len() == 1 && config.version[0].to_string() == protocol,
            "listener version mismatch"
        );
        let mut listener = config.init(Default::default())?.listen().await?;
        let runtime = Arc::new(Self {
            state: Arc::new(State {
                entries: Mutex::new(HashMap::new()),
                ready: AtomicBool::new(true),
                shutdown: watch::channel(false).0,
            }),
            base_url: base_url.trim_end_matches('/').into(),
            protocol: protocol.into(),
        });
        let state = runtime.state.clone();
        tokio::spawn(async move {
            let mut sessions = tokio::task::JoinSet::new();
            let mut shutdown = state.shutdown.subscribe();
            loop {
                tokio::select! {
                    _ = async { let _ = shutdown.wait_for(|value| *value).await; } => break,
                    Some(_) = sessions.join_next(), if !sessions.is_empty() => {},
                    request = listener.accept() => {
                        let Some(request) = request else { break };
                        let token = request.url().and_then(|url| url.query_pairs().find(|(k, _)| k == "ticket").map(|(_, v)| v.into_owned()));
                        let admitted = token.and_then(|token| {
                            let mut entries = state.entries.lock().ok()?;
                            let entry = entries.get_mut(&token)?;
                            if entry.used || *entry.fence.cancel.borrow() { return None }
                            entry.used = true;
                            Some((entry.origin.clone(), entry.fence.clone()))
                        });
                        let Some((origin, fence)) = admitted else { let _ = request.reject(moq_tokio::server::Reject::Unauthorized).await; continue };
                        if sessions.len() >= 16 { fence.revoke(); let _ = request.reject(moq_tokio::server::Reject::App(429)).await; continue }
                        sessions.spawn(async move {
                            let mut cancel = fence.cancel.subscribe();
                            let accepted = tokio::select! {
                                value = request.with_publisher(origin).ok() => value.ok(),
                                _ = cancel.wait_for(|value| *value) => None,
                            };
                            if let Some(session) = accepted {
                                tokio::select! { _ = session.closed() => {}, _ = cancel.wait_for(|value| *value) => {} }
                                session.abort(moq_net::Error::Cancel);
                                let _ = tokio::time::timeout(Duration::from_secs(1), session.closed()).await;
                                drop(session);
                            }
                            fence.revoke();
                        });
                    }
                }
            }
            state.ready.store(false, Ordering::Release);
            if let Ok(entries) = state.entries.lock() {
                for entry in entries.values() {
                    entry.fence.revoke();
                }
            }
            sessions.abort_all();
        });
        Ok(runtime)
    }
    pub fn available(&self) -> bool {
        self.state.ready.load(Ordering::Acquire)
    }
    /// The same fence must own the factory sender; expiry independently stops
    /// enqueue, drops demand, and cancels an already-connected transport.
    pub fn admit(
        &self,
        mut feed: Feed,
        fence: Arc<Fence>,
        ttl: Duration,
    ) -> anyhow::Result<Admission> {
        anyhow::ensure!(self.available(), "listener unavailable");
        anyhow::ensure!(
            feed.catalog.len() <= 65536 && feed.audio_tracks.len() <= 16,
            "configuration exceeds budget"
        );
        let _: hang::catalog::Catalog = hang::catalog::Catalog::from_slice(&feed.catalog)?;
        let mut origin_config = moq_net::origin::Config::default();
        origin_config.cache_duration = Duration::from_secs(2);
        origin_config.pool = moq_net::cache::Pool::new(
            moq_net::cache::Config::default()
                .with_capacity(8 * 1024 * 1024u64)
                .with_expiry(Duration::from_secs(2)),
        );
        let origin = moq_tokio::origin::spawn_config(origin_config);
        let broadcast = origin.create_broadcast("media")?;
        broadcast.announce(Default::default())?;
        let mut catalog = broadcast.create_track(
            "catalog.json",
            hang::catalog::Catalog::<()>::default_track_info(),
        )?;
        catalog.write_frame(moq_net::Timestamp::from_millis(0)?, feed.catalog.clone())?;
        let video = broadcast.create_track("video", VideoAdapter::track_info())?;
        let mut audio = std::collections::BTreeMap::new();
        for name in &feed.audio_tracks {
            audio.insert(
                name.clone(),
                broadcast.create_track(
                    name.as_str(),
                    hang::container::track_info(hang::catalog::PRIORITY.audio),
                )?,
            );
        }
        let mut adapter = AvAdapter::new(VideoAdapter::new(video, feed.generation), audio);
        let token = uuid::Uuid::new_v4().simple().to_string();
        {
            let mut entries = self
                .state
                .entries
                .lock()
                .map_err(|_| anyhow::anyhow!("registry poisoned"))?;
            anyhow::ensure!(entries.len() < 16, "capacity");
            entries.insert(
                token.clone(),
                Entry {
                    origin: origin.consume(),
                    fence: fence.clone(),
                    used: false,
                },
            );
        }
        let result = Admission {
            url: format!("{}/?ticket={token}", self.base_url),
            broadcast: "media".into(),
            generation: feed.generation,
            protocol: self.protocol.clone(),
        };
        let cleanup = SubscriptionCleanup {
            state: self.state.clone(),
            token,
            fence: fence.clone(),
        };
        tokio::spawn(async move {
            let _cleanup = cleanup;
            let mut cancel = fence.cancel.subscribe();
            let expiry =
                tokio::time::sleep(ttl.clamp(Duration::from_secs(1), Duration::from_secs(60)));
            tokio::pin!(expiry);
            loop {
                tokio::select! {
                    biased;
                    _ = &mut expiry => break,
                    _ = cancel.wait_for(|value| *value) => break,
                    window = feed.windows.recv() => match window {
                        Some(window) => { if adapter.publish(&window).is_err() { break } },
                        None => break,
                    }
                }
            }
            fence.revoke();
            adapter.revoke();
            drop((feed, catalog, broadcast, origin));
        });
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn cancellation_and_unwind_remove_subscription_ticket() {
        for panic in [false, true] {
            let state = Arc::new(State {
                entries: Mutex::new(HashMap::new()),
                ready: AtomicBool::new(true),
                shutdown: watch::channel(false).0,
            });
            let (fence, _rx) = Fence::channel(1);
            let origin = moq_tokio::origin::spawn();
            state.entries.lock().unwrap().insert(
                "test".into(),
                Entry {
                    origin: origin.consume(),
                    fence: fence.clone(),
                    used: false,
                },
            );
            let cleanup = SubscriptionCleanup {
                state: state.clone(),
                token: "test".into(),
                fence: fence.clone(),
            };
            let (started, ready) = tokio::sync::oneshot::channel();
            let task = tokio::spawn(async move {
                let _cleanup = cleanup;
                let _ = started.send(());
                if panic {
                    panic!("intentional cleanup regression");
                }
                std::future::pending::<()>().await;
            });
            ready.await.unwrap();
            if !panic {
                task.abort();
            }
            assert!(task.await.is_err());
            assert!(state.entries.lock().unwrap().is_empty());
            assert!(*fence.cancel.borrow());
        }
    }

    #[tokio::test]
    async fn expiry_removes_ticket_and_releases_source_demand() {
        struct Guard(Arc<AtomicBool>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let released = Arc::new(AtomicBool::new(false));
        let state = Arc::new(State {
            entries: Mutex::new(HashMap::new()),
            ready: AtomicBool::new(true),
            shutdown: watch::channel(false).0,
        });
        let runtime = Runtime {
            state: state.clone(),
            base_url: "https://localhost:4443".into(),
            protocol: "moq-transport-16".into(),
        };
        let (fence, windows) = Fence::channel(1);
        let feed = Feed { generation: 9, catalog: bytes::Bytes::from_static(br#"{"video":{"renditions":{"video":{"codec":"vp8","codedWidth":320,"codedHeight":180}}},"audio":{"renditions":{}}}"#), audio_tracks: vec![], windows, demand: Box::new(Guard(released.clone())) };
        let admitted = runtime
            .admit(feed, fence.clone(), Duration::from_secs(1))
            .unwrap();
        assert!(admitted.url.contains("ticket="));
        assert_eq!(state.entries.lock().unwrap().len(), 1);
        tokio::time::timeout(Duration::from_secs(3), async {
            while !released.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(state.entries.lock().unwrap().is_empty());
        assert!(*fence.cancel.borrow());
    }
    #[tokio::test]
    async fn actual_audio_shared_frame_preserves_1500_microseconds_and_credit() {
        let (fence, mut rx) = Fence::channel(1);
        let packet = crate::Packet {
            payload: bytes::Bytes::from_static(&[4, 5, 6]),
            pts_us: 1500,
            verified_rap: false,
        };
        assert!(
            fence
                .try_offer(AvWindow {
                    video: crate::Window {
                        generation: 1,
                        packets: vec![]
                    },
                    audio: std::collections::BTreeMap::from([("audio".into(), vec![packet])])
                })
                .is_ok()
        );
        let window = rx.try_recv().unwrap();
        let serialized = serialized_frame(&window.audio["audio"][0]).unwrap();
        let expected_ptr = serialized.as_ptr();
        let broadcast = moq_net::broadcast::Producer::new(Default::default());
        let track = broadcast
            .create_track(
                "audio",
                hang::container::track_info(hang::catalog::PRIORITY.audio),
            )
            .unwrap();
        let mut group = track.append_group().unwrap();
        let mut consumer = group.consume();
        group
            .write_frame(moq_net::Timestamp::from_micros(1500).unwrap(), serialized)
            .unwrap();
        group.finish().unwrap();
        drop(window);
        let frame = consumer.read_frame().await.unwrap().unwrap();
        assert_eq!(
            frame.timestamp,
            moq_net::Timestamp::from_micros(1500).unwrap()
        );
        assert_eq!(frame.payload.as_ptr(), expected_ptr);
        drop((group, consumer, track, broadcast));
        assert_eq!(fence.charged.load(Ordering::Acquire), 3);
        let decoded = hang::container::Frame::decode(frame.payload.clone()).unwrap();
        assert_eq!(
            decoded.timestamp,
            moq_net::Timestamp::from_micros(1500).unwrap()
        );
        assert_eq!(decoded.payload.as_ref(), &[4, 5, 6]);
        drop(decoded);
        drop(frame);
        assert_eq!(fence.charged.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn actual_moq_frame_retains_serialized_owner_and_source_charge() {
        let (fence, mut rx) = Fence::channel(1);
        let window = AvWindow {
            video: crate::Window {
                generation: 1,
                packets: vec![crate::Packet {
                    payload: bytes::Bytes::from_static(&[1, 2, 3]),
                    pts_us: 123,
                    verified_rap: true,
                }],
            },
            audio: Default::default(),
        };
        assert!(fence.try_offer(window).is_ok());
        let window = rx.try_recv().unwrap();
        let serialized = serialized_frame(&window.video.packets[0]).unwrap();
        let expected_ptr = serialized.as_ptr();
        let broadcast = moq_net::broadcast::Producer::new(Default::default());
        let track = broadcast
            .create_track("video", crate::VideoAdapter::track_info())
            .unwrap();
        let mut group = track.append_group().unwrap();
        let mut consumer = group.consume();
        group
            .write_frame(moq_net::Timestamp::from_micros(123).unwrap(), serialized)
            .unwrap();
        group.finish().unwrap();
        drop(window);
        fence.revoke();
        let frame = consumer.read_frame().await.unwrap().unwrap();
        assert_eq!(
            frame.payload.as_ptr(),
            expected_ptr,
            "must use upstream Shared rather than Segment copy"
        );
        drop((group, consumer, track, broadcast));
        assert_eq!(fence.charged.load(Ordering::Acquire), 3);
        let decoded = hang::container::Frame::decode(frame.payload.clone()).unwrap();
        assert_eq!(decoded.payload.as_ref(), &[1, 2, 3]);
        drop(decoded);
        drop(frame);
        assert_eq!(fence.charged.load(Ordering::Acquire), 0);
    }

    #[test]
    fn retained_payload_credit_survives_queue_and_inflight_clones() {
        let (fence, mut rx) = Fence::channel(1);
        let window = AvWindow {
            video: crate::Window {
                generation: 1,
                packets: vec![crate::Packet {
                    payload: bytes::Bytes::from_static(&[1, 2, 3]),
                    pts_us: 0,
                    verified_rap: true,
                }],
            },
            audio: Default::default(),
        };
        assert!(fence.try_offer(window).is_ok());
        assert_eq!(fence.charged.load(Ordering::Acquire), 3);
        let received = rx.try_recv().unwrap();
        let retained = received.video.packets[0].payload.clone();
        drop(received);
        fence.revoke();
        assert_eq!(fence.charged.load(Ordering::Acquire), 3);
        let slice = retained.slice(1..2);
        drop(retained);
        assert_eq!(fence.charged.load(Ordering::Acquire), 3);
        drop(slice);
        assert_eq!(fence.charged.load(Ordering::Acquire), 0);
    }
    #[test]
    fn revoking_one_peer_preserves_other_peer_and_shared_payload_credits() {
        let payload = bytes::Bytes::from(vec![7u8; 1024]);
        let window = |pts_us| AvWindow {
            video: crate::Window {
                generation: 1,
                packets: vec![crate::Packet {
                    payload: payload.clone(),
                    pts_us,
                    verified_rap: true,
                }],
            },
            audio: Default::default(),
        };
        let (first, mut first_rx) = Fence::channel(2);
        let (second, mut second_rx) = Fence::channel(2);
        assert!(first.try_offer(window(0)).is_ok());
        assert!(second.try_offer(window(0)).is_ok());
        let first_window = first_rx.try_recv().unwrap();
        let retained_first = first_window.video.packets[0].payload.clone();
        let second_window = second_rx.try_recv().unwrap();
        assert_eq!(retained_first.as_ptr(), payload.as_ptr());
        assert_eq!(
            second_window.video.packets[0].payload.as_ptr(),
            payload.as_ptr()
        );
        first.revoke();
        assert!(first.try_offer(window(1)).is_err());
        assert!(!*second.cancel.borrow());
        let mut continuing_frames = 1;
        for pts in 1..10 {
            assert!(second.try_offer(window(pts)).is_ok());
            let received = second_rx.try_recv().unwrap();
            assert_eq!(received.video.packets[0].pts_us, pts);
            continuing_frames += received.video.packets.len();
        }
        assert_eq!(continuing_frames, 10);
        assert_eq!(first.charged.load(Ordering::Acquire), 1024);
        assert_eq!(second.charged.load(Ordering::Acquire), 1024);
        drop(first_window);
        assert_eq!(first.charged.load(Ordering::Acquire), 1024);
        drop(retained_first);
        assert_eq!(first.charged.load(Ordering::Acquire), 0);
        assert_eq!(second.charged.load(Ordering::Acquire), 1024);
        drop(second_window);
        assert_eq!(second.charged.load(Ordering::Acquire), 0);
        assert!(!*second.cancel.borrow());
    }

    #[test]
    fn fence_rejects_after_revoke_and_bounds_queue() {
        let (fence, _rx) = Fence::channel(1);
        let window = || AvWindow {
            video: crate::Window {
                generation: 1,
                packets: vec![],
            },
            audio: Default::default(),
        };
        assert!(fence.try_offer(window()).is_ok());
        assert!(fence.try_offer(window()).is_err());
        fence.revoke();
        fence.revoke();
        assert!(fence.try_offer(window()).is_err());
    }
}
