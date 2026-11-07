use super::*;
use crate::iptv::fetch::{FetchFuture, FetchOutcome, FetchRequest, Fetcher};
use std::io::{Seek, Write};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

#[derive(Clone)]
enum Reply {
    Body(Vec<u8>, &'static str),
    NotModified,
    Err(&'static str),
}

/// Scripted transport: replies come from a queue (the last one repeats).
struct Fake {
    script: Mutex<VecDeque<Reply>>,
    calls: AtomicUsize,
    conditional: Mutex<Vec<Option<String>>>,
    delay: Duration,
    gate: Option<Arc<tokio::sync::Semaphore>>,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
    host_in_flight: Mutex<HashMap<String, usize>>,
    host_max: Mutex<HashMap<String, usize>>,
}

impl Fake {
    fn new(script: Vec<Reply>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script.into()),
            calls: AtomicUsize::new(0),
            conditional: Mutex::new(Vec::new()),
            delay: Duration::ZERO,
            gate: None,
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
            host_in_flight: Mutex::new(HashMap::new()),
            host_max: Mutex::new(HashMap::new()),
        })
    }
    fn gated(reply: Reply, gate: &Arc<tokio::sync::Semaphore>) -> Arc<Self> {
        let mut f = Arc::try_unwrap(Self::new(vec![reply])).ok().unwrap();
        f.gate = Some(Arc::clone(gate));
        Arc::new(f)
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Fetcher for Fake {
    fn fetch(&self, request: FetchRequest, _: Arc<DialPolicy>) -> FetchFuture<'_> {
        assert_eq!(request.max_bytes, 512 * 1024);
        self.calls.fetch_add(1, Ordering::SeqCst);
        lock(&self.conditional).push(request.etag.clone().or(request.last_modified.clone()));
        let host = host_of(&request.url);
        let reply = {
            let mut s = lock(&self.script);
            if s.len() > 1 {
                s.pop_front()
            } else {
                s.front().cloned()
            }
            .unwrap()
        };
        Box::pin(async move {
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(now, Ordering::SeqCst);
            {
                let mut m = lock(&self.host_in_flight);
                let c = m.entry(host.clone()).or_default();
                *c += 1;
                let c = *c;
                let mut mx = lock(&self.host_max);
                let e = mx.entry(host.clone()).or_default();
                *e = (*e).max(c);
            }
            if let Some(g) = &self.gate {
                let _p = g.acquire().await.unwrap();
            }
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            *lock(&self.host_in_flight).get_mut(&host).unwrap() -= 1;
            match reply {
                Reply::Body(b, ct) => {
                    let mut file = tempfile::tempfile().unwrap();
                    file.write_all(&b).unwrap();
                    file.rewind().unwrap();
                    Ok(FetchOutcome::Body {
                        file,
                        len: b.len() as u64,
                        etag: Some("\"up1\"".into()),
                        last_modified: None,
                        content_type: Some(ct.to_owned()),
                    })
                }
                Reply::NotModified => Ok(FetchOutcome::NotModified),
                Reply::Err(e) => Err(e.to_owned()),
            }
        })
    }
}

struct Rig {
    dir: tempfile::TempDir,
    clock: Arc<AtomicU64>,
}

impl Rig {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
            clock: Arc::new(AtomicU64::new(1_000_000)),
        }
    }
    fn open(&self, f: Arc<Fake>) -> Arc<LogoCache> {
        let c = Arc::clone(&self.clock);
        LogoCache::with_options(
            self.dir.path().join("logos"),
            f,
            Arc::new(DialPolicy::allow_lan()),
            Arc::new(move || c.load(Ordering::SeqCst)),
        )
    }
    fn advance(&self, secs: u64) {
        self.clock.fetch_add(secs, Ordering::SeqCst);
    }
}

const URL: &str = "https://h.example/a.png";

fn png() -> Reply {
    Reply::Body(b"PNGDATA".to_vec(), "image/png")
}

fn is_image(l: &Logo) -> bool {
    matches!(l, Logo::Image(i) if i.bytes == b"PNGDATA" && i.content_type == "image/png")
}

async fn settle() {
    for _ in 0..50 {
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

#[tokio::test]
async fn image_types_only_and_cached() {
    let rig = Rig::new();
    let f = Fake::new(vec![Reply::Body(b"<html>".to_vec(), "text/html")]);
    let c = rig.open(f.clone());
    assert!(matches!(c.get(URL).await, Logo::Missing));
    let f2 = Fake::new(vec![png()]);
    let c2 = rig.open(f2.clone());
    assert!(matches!(
        c2.get("https://h.example/b.png").await,
        Logo::Image(_)
    ));
    assert!(is_image(&c2.get("https://h.example/b.png").await));
    assert_eq!(f2.calls(), 1);
}

#[tokio::test]
async fn oversized_logo_is_missing() {
    let rig = Rig::new();
    let f = Fake::new(vec![Reply::Body(vec![0; 512 * 1024 + 1], "image/png")]);
    let c = rig.open(f.clone());
    assert!(matches!(c.get(URL).await, Logo::Missing));
    assert!(matches!(c.get(URL).await, Logo::Missing));
    assert_eq!(f.calls(), 1, "oversized is negative cached");
}

#[tokio::test]
async fn cache_survives_a_restart() {
    let rig = Rig::new();
    let f = Fake::new(vec![png()]);
    assert!(is_image(&rig.open(f.clone()).get(URL).await));
    let g = Fake::new(vec![Reply::Err("HTTP 500")]);
    assert!(is_image(&rig.open(g.clone()).get(URL).await));
    assert_eq!(g.calls(), 0, "served from disk");
}

#[tokio::test]
async fn ttl_expiry_revalidates_and_304_keeps_bytes() {
    let rig = Rig::new();
    let f = Fake::new(vec![png(), Reply::NotModified]);
    let c = rig.open(f.clone());
    assert!(is_image(&c.get(URL).await));
    rig.advance(FRESH_SECS - 10);
    assert!(is_image(&c.get(URL).await));
    settle().await;
    assert_eq!(f.calls(), 1, "still fresh");
    rig.advance(20);
    assert!(is_image(&c.get(URL).await), "stale copy served meanwhile");
    settle().await;
    assert_eq!(f.calls(), 2);
    assert_eq!(lock(&f.conditional)[1].as_deref(), Some("\"up1\""));
    // Revalidation succeeded: fresh again, bytes intact, also after restart.
    assert!(is_image(&c.get(URL).await));
    settle().await;
    assert_eq!(f.calls(), 2);
    let h = Fake::new(vec![Reply::Err("HTTP 500")]);
    assert!(is_image(&rig.open(h.clone()).get(URL).await));
    assert_eq!(h.calls(), 0);
}

#[tokio::test]
async fn stale_is_served_while_revalidating() {
    let rig = Rig::new();
    let gate = Arc::new(tokio::sync::Semaphore::new(1));
    let f = Fake::gated(png(), &gate);
    let c = rig.open(f.clone());
    assert!(is_image(&c.get(URL).await));
    rig.advance(FRESH_SECS + 1);
    let _held = gate.acquire().await.unwrap(); // block the upstream
    let got = tokio::time::timeout(Duration::from_secs(2), c.get(URL))
        .await
        .expect("must not wait for the upstream");
    assert!(is_image(&got));
    settle().await;
    assert_eq!(f.calls(), 2, "background revalidation started");
}

#[tokio::test]
async fn upstream_failure_never_evicts_a_good_copy() {
    let rig = Rig::new();
    let f = Fake::new(vec![png(), Reply::Err("HTTP 404")]);
    let c = rig.open(f.clone());
    assert!(is_image(&c.get(URL).await));
    rig.advance(FRESH_SECS + 1);
    assert!(is_image(&c.get(URL).await));
    settle().await;
    assert_eq!(f.calls(), 2);
    assert!(is_image(&c.get(URL).await), "good copy survives a 404");
    assert!(is_image(&rig.open(Fake::new(vec![png()])).get(URL).await));
}

#[tokio::test]
async fn not_found_is_negative_cached_for_seven_days() {
    let rig = Rig::new();
    let f = Fake::new(vec![Reply::Err(
        "https://h.example/a.png: HTTP 404 Not Found",
    )]);
    let c = rig.open(f.clone());
    assert!(matches!(c.get(URL).await, Logo::Missing));
    rig.advance(PERMANENT_SECS - 60);
    assert!(matches!(c.get(URL).await, Logo::Missing));
    // Also across a restart.
    assert!(matches!(rig.open(f.clone()).get(URL).await, Logo::Missing));
    assert_eq!(f.calls(), 1);
    rig.advance(120);
    assert!(matches!(c.get(URL).await, Logo::Missing));
    assert_eq!(f.calls(), 2, "retried after 7 days");
}

#[tokio::test]
async fn dns_failure_is_permanent_class() {
    assert!(classify("h: error sending request: dns error: no such host").0 == FailKind::Permanent);
    assert!(classify("h: HTTP 429 Too Many Requests").0 == FailKind::Transient);
    assert!(classify("h: HTTP 503 x").0 == FailKind::Transient);
    assert!(classify("h: operation timed out").0 == FailKind::Transient);
    assert!(classify("h: HTTP 403 Forbidden").0 == FailKind::Permanent);
}

#[tokio::test]
async fn too_many_requests_retries_after_short_window_and_backs_off() {
    let rig = Rig::new();
    let f = Fake::new(vec![Reply::Err("HTTP 429 Too Many Requests")]);
    let c = rig.open(f.clone());
    assert!(matches!(c.get(URL).await, Logo::Missing));
    rig.advance(TRANSIENT_BASE_SECS - 1);
    assert!(matches!(c.get(URL).await, Logo::Missing));
    assert_eq!(f.calls(), 1, "inside the 15 minute window");
    rig.advance(2);
    assert!(matches!(c.get(URL).await, Logo::Missing));
    assert_eq!(f.calls(), 2, "retried after 15 minutes");
    rig.advance(TRANSIENT_BASE_SECS + 1);
    assert!(matches!(c.get(URL).await, Logo::Missing));
    assert_eq!(
        f.calls(),
        2,
        "second failure doubled the window to 30 minutes"
    );
    rig.advance(TRANSIENT_BASE_SECS);
    assert!(matches!(c.get(URL).await, Logo::Missing));
    assert_eq!(f.calls(), 3);
    assert_eq!(transient_delay(0), 900);
    assert_eq!(transient_delay(1), 1800);
    assert_eq!(transient_delay(30), TRANSIENT_MAX_SECS);
}

#[tokio::test]
async fn recovery_after_transient_failure_serves_the_image() {
    let rig = Rig::new();
    let f = Fake::new(vec![Reply::Err("HTTP 429 x"), png()]);
    let c = rig.open(f.clone());
    assert!(matches!(c.get(URL).await, Logo::Missing));
    rig.advance(TRANSIENT_BASE_SECS + 1);
    assert!(is_image(&c.get(URL).await));
}

#[tokio::test]
async fn eviction_keeps_the_store_within_both_bounds() {
    let rig = Rig::new();
    let f = Fake::new(vec![Reply::Body(vec![7; 1000], "image/png")]);
    // File bound: 10 files = 5 entries (bin + json).
    let c = rig.open(f.clone()).with_limits(SLOT_WAIT, u64::MAX, 10);
    for i in 0..12 {
        let _ = c.get(&format!("https://h{i}.example/x.png")).await;
    }
    let (n, bytes) = dir_usage(&rig.dir.path().join("logos"));
    assert!(n <= 10, "files {n}");
    assert!(n >= 8, "evicts only what it must: {n}");
    let st = lock(&c.state);
    assert!(st.total_files <= 10);
    assert_eq!(st.total_bytes, bytes, "accounting matches the disk");
    drop(st);
    // Byte bound.
    let rig2 = Rig::new();
    let c2 = rig2.open(f.clone()).with_limits(SLOT_WAIT, 5_000, u64::MAX);
    for i in 0..12 {
        let _ = c2.get(&format!("https://h{i}.example/x.png")).await;
    }
    let (_, bytes2) = dir_usage(&rig2.dir.path().join("logos"));
    assert!(bytes2 <= 5_000, "bytes {bytes2}");
    // LRU: the most recent entry survives, the oldest is gone.
    let recent = key_hash("https://h11.example/x.png");
    assert!(rig2
        .dir
        .path()
        .join(format!("logos/{recent}.json"))
        .exists());
    let oldest = key_hash("https://h0.example/x.png");
    assert!(!rig2
        .dir
        .path()
        .join(format!("logos/{oldest}.json"))
        .exists());
    // Startup scan re-applies the bounds.
    let c3 = rig2.open(f.clone()).with_limits(SLOT_WAIT, 2_500, u64::MAX);
    let (_, bytes3) = dir_usage(&rig2.dir.path().join("logos"));
    assert!(bytes3 <= 2_500, "bytes {bytes3}");
    drop(c3);
}

fn dir_usage(d: &Path) -> (u64, u64) {
    let mut n = 0;
    let mut b = 0;
    for e in std::fs::read_dir(d).unwrap().flatten() {
        n += 1;
        b += e.metadata().unwrap().len();
    }
    (n, b)
}

#[tokio::test]
async fn per_host_and_global_caps_hold() {
    let rig = Rig::new();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let f = Fake::gated(png(), &gate);
    let c = rig.open(f.clone());
    let mut tasks = Vec::new();
    for i in 0..6 {
        let c = Arc::clone(&c);
        tasks.push(tokio::spawn(async move {
            c.get(&format!("https://same.example/{i}.png")).await
        }));
    }
    settle().await;
    assert_eq!(f.calls(), 2, "only two fetches reach one host");
    gate.add_permits(100);
    for t in tasks {
        tokio::time::timeout(Duration::from_secs(10), t)
            .await
            .expect("no hang")
            .unwrap();
    }
    assert_eq!(*lock(&f.host_max).get("same.example").unwrap(), 2);

    // Global cap across many hosts.
    let rig = Rig::new();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let f = Fake::gated(png(), &gate);
    let c = rig.open(f.clone());
    let mut tasks = Vec::new();
    for i in 0..20 {
        let c = Arc::clone(&c);
        tasks.push(tokio::spawn(async move {
            c.get(&format!("https://host{i}.example/a.png")).await
        }));
    }
    settle().await;
    assert_eq!(f.calls(), 8, "eight global slots");
    gate.add_permits(100);
    for t in tasks {
        tokio::time::timeout(Duration::from_secs(10), t)
            .await
            .expect("no hang")
            .unwrap();
    }
    assert!(f.max_in_flight.load(Ordering::SeqCst) <= 8);
}

#[tokio::test]
async fn waiting_past_the_slot_deadline_is_busy_not_cached() {
    let rig = Rig::new();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let f = Fake::gated(png(), &gate);
    let c =
        rig.open(f.clone())
            .with_limits(Duration::from_millis(100), MAX_DISK_BYTES, MAX_DISK_FILES);
    let mut tasks = Vec::new();
    for i in 0..2 {
        let c = Arc::clone(&c);
        tasks.push(tokio::spawn(async move {
            c.get(&format!("https://slow.example/{i}.png")).await
        }));
    }
    settle().await;
    let third = tokio::time::timeout(Duration::from_secs(5), c.get("https://slow.example/3.png"))
        .await
        .expect("bounded wait");
    assert!(matches!(third, Logo::Busy));
    gate.add_permits(100);
    for t in tasks {
        t.await.unwrap();
    }
    // Not negative cached: the next attempt fetches.
    assert!(is_image(&c.get("https://slow.example/3.png").await));
}
