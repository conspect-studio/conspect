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
        let state = self.state.clone();
        tokio::spawn(async move {
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
            if let Ok(mut entries) = state.entries.lock() {
                entries.remove(&token);
            }
            drop((feed, catalog, broadcast, origin));
        });
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        drop(retained);
        assert_eq!(fence.charged.load(Ordering::Acquire), 0);
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
