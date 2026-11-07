//! Dual-stack SRT **listener input**.
//!
//! libav cannot bind `srt://[::]:port` (see `multiview_srt_sys`), and a source's
//! video and audio rails each open their own libav context, so two contexts
//! would contend for one listener port. One [`SrtIngestBridge`] per listener URL
//! therefore owns the native socket, and each libav context reads the caller's
//! MPEG-TS from its own [`SrtReader`]: one end of a private, already-connected
//! `socketpair(2)` whose other end the relay writes. libav opens it as
//! `pipe:<fd>` (it `dup`s the descriptor). There is **no listening socket**:
//! the decrypted payload exists only inside this process, on close-on-exec
//! descriptors nothing outside can connect to (review SRT-01, ADR-0039).
//!
//! The read end is non-blocking, so libav's `rw_timeout` and interrupt
//! callback bound every read exactly as for a network URL. When the caller goes
//! away the readers it fed are closed, so libav sees EOF and the ordinary
//! reconnect bracket runs while the tile rides `NO_SIGNAL`; with no caller a
//! reader times out on `rw_timeout` and retries. At most [`MAX_READERS`]
//! readers exist per bridge, all served by the one relay thread with no
//! per-reader thread; each has a bounded backlog that drops on overflow
//! (invariant #10, review SRT-02). One caller is served at a time; a later
//! caller waits in the backlog until it ends.

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};
use std::time::Duration;

use multiview_output::push_link::Backoff;
use multiview_srt_sys::{parse_listener_url, ListenerSpec, SrtError, SrtListener, SrtSocket};

/// Readers one bridge serves at once: the video and audio rails, plus one
/// overlapping reconnect each. A further [`SrtIngestBridge::open_reader`] fails.
pub const MAX_READERS: usize = 4;
/// Per-reader backlog bound in bytes, beyond what the socket buffer holds.
const READER_BACKLOG_BYTES: usize = 2 * 1024 * 1024;
/// libsrt handshakes queued while a caller is being served.
const LISTEN_BACKLOG: u16 = 1;
/// Largest SRT message read in one call.
const MESSAGE_MAX: usize = 1500;
/// Poll step of interruptible sleeps.
const POLL: Duration = Duration::from_millis(50);

/// The relay's end of one reader's socketpair.
struct Reader {
    id: u64,
    tx: UnixStream,
    /// Bytes the socket would not take yet, oldest first (bounded).
    backlog: VecDeque<Vec<u8>>,
    backlog_bytes: usize,
    /// Fed by the current caller (closed when that caller goes away).
    fed: bool,
}

impl Reader {
    /// Queue `message` behind any backlog and write as much as the socket
    /// takes, never blocking. `false` when the reader has hung up.
    fn push(&mut self, message: &[u8], dropped: &AtomicU64) -> bool {
        if self.backlog_bytes.saturating_add(message.len()) > READER_BACKLOG_BYTES {
            dropped.fetch_add(1, Ordering::Relaxed);
        } else {
            self.backlog_bytes += message.len();
            self.backlog.push_back(message.to_vec());
        }
        while let Some(front) = self.backlog.front_mut() {
            match self.tx.write(front) {
                Ok(n) if n == front.len() => {
                    self.backlog_bytes -= n;
                    self.backlog.pop_front();
                }
                Ok(n) => {
                    self.backlog_bytes -= n;
                    front.drain(..n);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return false,
            }
        }
        self.fed = true;
        true
    }
}

#[derive(Default)]
struct Readers {
    next_id: u64,
    list: Vec<Reader>,
}

type SharedReaders = Arc<Mutex<Readers>>;
type Registry = Mutex<HashMap<String, Weak<SrtIngestBridge>>>;

/// A running listener shared by every rail of one source. Stops when the last
/// holder drops it.
pub struct SrtIngestBridge {
    readers: SharedReaders,
    stop: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
}

impl std::fmt::Debug for SrtIngestBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SrtIngestBridge")
            .field("readers", &lock(&self.readers).list.len())
            .finish_non_exhaustive()
    }
}

/// One libav context's private stream from a bridge. Keep it alive while libav
/// reads [`SrtReader::url`]; dropping it frees the bridge's reader slot.
#[derive(Debug)]
pub struct SrtReader {
    id: u64,
    rx: UnixStream,
    url: String,
    readers: Weak<Mutex<Readers>>,
}

impl SrtReader {
    /// The `pipe:<fd>` URL libav opens (it `dup`s the descriptor).
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }
}

impl Drop for SrtReader {
    fn drop(&mut self) {
        if let Some(readers) = self.readers.upgrade() {
            lock(&readers).list.retain(|reader| reader.id != self.id);
        }
        // `rx` closes here; libav closes its own duplicate.
        let _ = &self.rx;
    }
}

impl SrtIngestBridge {
    /// A new private reader of this listener's caller stream.
    ///
    /// # Errors
    /// A reason string when [`MAX_READERS`] readers are already open or the
    /// socketpair cannot be created.
    pub fn open_reader(&self) -> Result<SrtReader, String> {
        let mut readers = lock(&self.readers);
        if readers.list.len() >= MAX_READERS {
            return Err(format!("srt input: all {MAX_READERS} reader slots in use"));
        }
        let (tx, rx) = UnixStream::pair().map_err(|e| format!("srt input socketpair: {e}"))?;
        // Both ends non-blocking: the relay never waits on a reader, and libav
        // bounds an empty read with `rw_timeout` and its interrupt callback.
        tx.set_nonblocking(true)
            .and_then(|()| rx.set_nonblocking(true))
            .and_then(|()| tx.shutdown(std::net::Shutdown::Read))
            .and_then(|()| rx.shutdown(std::net::Shutdown::Write))
            .map_err(|e| format!("srt input socketpair: {e}"))?;
        let id = readers.next_id;
        readers.next_id = readers.next_id.wrapping_add(1);
        readers.list.push(Reader {
            id,
            tx,
            backlog: VecDeque::new(),
            backlog_bytes: 0,
            fed: false,
        });
        Ok(SrtReader {
            id,
            url: format!("pipe:{}", rx.as_raw_fd()),
            rx,
            readers: Arc::downgrade(&self.readers),
        })
    }

    /// Messages shed because a reader's backlog was full.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl Drop for SrtIngestBridge {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The bridge for listener `url`, started on first use. Called on ingest
/// threads only, never on the output clock. An unusable URL fails closed; the
/// error names no URL, so it cannot leak a passphrase into a log.
///
/// # Errors
/// A reason string when the URL is invalid or the local relay cannot start.
pub fn acquire(url: &str) -> Result<Arc<SrtIngestBridge>, String> {
    let mut bridges = registry().lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(bridge) = bridges.get(url).and_then(Weak::upgrade) {
        return Ok(bridge);
    }
    bridges.retain(|_, bridge| bridge.strong_count() > 0);
    let spec = parse_listener_url(url, false).map_err(|e| e.to_string())?;
    let bridge = Arc::new(start(spec)?);
    bridges.insert(url.to_owned(), Arc::downgrade(&bridge));
    Ok(bridge)
}

/// How [`start_with`] spawns a named thread (injectable so a test can fail one).
type Spawn<'a> = dyn Fn(&str, Box<dyn FnOnce() + Send>) -> std::io::Result<()> + 'a;

fn spawn_thread(name: &str, body: Box<dyn FnOnce() + Send>) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(body)
        .map(drop)
}

/// Raises the stop flag on drop unless disarmed: startup rollback.
struct StopOnDrop(Option<Arc<AtomicBool>>);

impl StopOnDrop {
    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        if let Some(stop) = &self.0 {
            stop.store(true, Ordering::Release);
        }
    }
}

fn start(spec: ListenerSpec) -> Result<SrtIngestBridge, String> {
    start_with(spec, &spawn_thread)
}

fn start_with(spec: ListenerSpec, spawn: &Spawn<'_>) -> Result<SrtIngestBridge, String> {
    let stop = Arc::new(AtomicBool::new(false));
    // Until the bridge owns `stop`, any early return raises it, so every
    // thread already started exits and drops its socket (SRT-06).
    let rollback = StopOnDrop(Some(Arc::clone(&stop)));
    let dropped = Arc::new(AtomicU64::new(0));
    let readers = SharedReaders::default();
    let srt_stop = Arc::clone(&stop);
    let srt_dropped = Arc::clone(&dropped);
    let srt_readers = Arc::clone(&readers);
    spawn(
        "multiview-srt-listen",
        Box::new(move || listen(&spec, &srt_readers, &srt_dropped, &srt_stop)),
    )
    .map_err(|e| format!("srt listener thread: {e}"))?;
    rollback.disarm();
    Ok(SrtIngestBridge {
        readers,
        stop,
        dropped,
    })
}

fn sleep_unless_stopped(stop: &AtomicBool, total: Duration) {
    let mut left = total;
    while !stop.load(Ordering::Acquire) && !left.is_zero() {
        let slice = left.min(POLL);
        std::thread::sleep(slice);
        left = left.saturating_sub(slice);
    }
}

/// Bind, then serve one caller at a time until stopped.
fn listen(
    spec: &ListenerSpec,
    readers: &SharedReaders,
    dropped: &AtomicU64,
    stop: &Arc<AtomicBool>,
) {
    let mut backoff = Backoff::standard(u64::from(spec.addr.port()));
    while !stop.load(Ordering::Acquire) {
        let listener = match SrtListener::bind(spec.addr, &spec.options, LISTEN_BACKLOG) {
            Ok(listener) => listener,
            Err(e) => {
                tracing::warn!(addr = %spec.addr, error = %e, "srt input listener bind failed");
                sleep_unless_stopped(stop, backoff.next_delay());
                continue;
            }
        };
        backoff.reset();
        tracing::info!(addr = %spec.addr, "srt input listener ready");
        loop {
            let flag = Arc::clone(stop);
            match listener.accept(&move || flag.load(Ordering::Acquire)) {
                Ok((socket, peer)) => {
                    tracing::info!(peer = ?peer, "srt input caller connected");
                    relay(&socket, readers, dropped, stop);
                    // Closing the readers this caller fed gives libav the EOF
                    // that runs the normal reconnect bracket. A reader that
                    // joined while the caller was dying is kept for the next.
                    lock(readers).list.retain(|reader| !reader.fed);
                    tracing::info!(peer = ?peer, "srt input caller disconnected");
                }
                Err(SrtError::Interrupted) => return,
                Err(e) => {
                    tracing::warn!(error = %e, "srt input listener failed; rebinding");
                    break;
                }
            }
        }
    }
}

fn lock(readers: &Mutex<Readers>) -> std::sync::MutexGuard<'_, Readers> {
    readers.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Pump one caller's messages to the readers, never waiting on any of them.
fn relay(socket: &SrtSocket, readers: &SharedReaders, dropped: &AtomicU64, stop: &AtomicBool) {
    let mut buf = [0_u8; MESSAGE_MAX];
    while !stop.load(Ordering::Acquire) {
        let len = match socket.recv(&mut buf) {
            Ok(Some(len)) => len,
            Ok(None) => continue,
            Err(_) => return,
        };
        let Some(message) = buf.get(..len) else {
            return;
        };
        lock(readers)
            .list
            .retain_mut(|reader| reader.push(message, dropped));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::{acquire, lock, start_with, Spawn, MAX_READERS, READER_BACKLOG_BYTES};
    use multiview_srt_sys::parse_listener_url;
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    /// Inode numbers of every TCP/UDP-less listening socket this process holds:
    /// TCP and TCP6 in `LISTEN`, plus listening Unix sockets.
    fn listening_socket_inodes() -> Vec<String> {
        let mut owned = std::collections::HashSet::new();
        for entry in std::fs::read_dir("/proc/self/fd").unwrap().flatten() {
            if let Ok(target) = std::fs::read_link(entry.path()) {
                let target = target.to_string_lossy().into_owned();
                if let Some(inode) = target
                    .strip_prefix("socket:[")
                    .and_then(|t| t.strip_suffix(']'))
                {
                    owned.insert(inode.to_owned());
                }
            }
        }
        let mut listening = Vec::new();
        for table in ["/proc/self/net/tcp", "/proc/self/net/tcp6"] {
            for line in std::fs::read_to_string(table).unwrap().lines().skip(1) {
                let cols: Vec<&str> = line.split_whitespace().collect();
                if cols.get(3) == Some(&"0A") && cols.get(9).is_some_and(|i| owned.contains(*i)) {
                    listening.push(format!("{table} inode {}", cols[9]));
                }
            }
        }
        for line in std::fs::read_to_string("/proc/self/net/unix")
            .unwrap()
            .lines()
            .skip(1)
        {
            let cols: Vec<&str> = line.split_whitespace().collect();
            // Flags 00010000 = __SO_ACCEPTCON: a listening Unix socket.
            if cols.get(3) == Some(&"00010000") && cols.get(6).is_some_and(|i| owned.contains(*i)) {
                listening.push(format!("unix inode {}", cols[6]));
            }
        }
        listening
    }

    const ISOLATED: &str = "MULTIVIEW_SRT_INGEST_ISOLATED";

    /// SRT-01: decrypted input never reaches a socket another local process
    /// could connect to. Runs in its own process so no sibling test's sockets
    /// are counted: after a bridge for an ENCRYPTED listener is up, the process
    /// holds no listening TCP or Unix socket at all.
    #[test]
    fn decrypted_input_is_never_offered_on_a_connectable_socket() {
        if std::env::var_os(ISOLATED).is_none() {
            let status = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "srt_ingest::tests::decrypted_input_is_never_offered_on_a_connectable_socket",
                    "--test-threads=1",
                    "--nocapture",
                ])
                .env(ISOLATED, "1")
                .status()
                .unwrap();
            assert!(status.success(), "isolated run failed: {status}");
            return;
        }
        assert!(
            listening_socket_inodes().is_empty(),
            "test process starts clean"
        );
        let bridge =
            acquire("srt://[::1]:29874?mode=listener&pbkeylen=16&passphrase=abcdefghijkl").unwrap();
        let reader = bridge.open_reader().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let listening = listening_socket_inodes();
        assert!(
            listening.is_empty(),
            "plaintext relay is connectable: {listening:?}"
        );
        assert!(reader.url().starts_with("pipe:"), "{}", reader.url());
    }

    /// Every startup thread-spawn failure rolls back: each thread that DID start
    /// exits, so a failed start leaks no relay, listener or fd (SRT-06).
    #[test]
    fn a_failed_thread_spawn_leaks_nothing_already_started() {
        for fail_at in 1..=4_usize {
            let started: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::default();
            let calls = Mutex::new(0_usize);
            let spawn: &Spawn<'_> = &|name, body| {
                let mut n = calls.lock().unwrap();
                *n += 1;
                if *n == fail_at {
                    return Err(std::io::Error::other("injected spawn failure"));
                }
                let handle = std::thread::Builder::new()
                    .name(name.to_owned())
                    .spawn(body)?;
                started.lock().unwrap().push(handle);
                Ok(())
            };
            let spec = parse_listener_url("srt://[::1]:29873?mode=listener", false).unwrap();
            let result = start_with(spec, spawn);
            let spawned = *calls.lock().unwrap();
            if fail_at > spawned {
                // Every spawn already succeeded: nothing to inject here.
                assert!(result.is_ok());
                break;
            }
            assert!(
                result.is_err(),
                "spawn {fail_at} failed but start succeeded"
            );
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline
                && started.lock().unwrap().iter().any(|h| !h.is_finished())
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            for handle in started.lock().unwrap().drain(..) {
                assert!(
                    handle.is_finished(),
                    "spawn {fail_at} failed: a thread leaked"
                );
                handle.join().unwrap();
            }
        }
    }

    #[test]
    fn rails_share_one_bridge_and_a_bad_url_fails_closed_without_leaking() {
        let url = "srt://[::1]:29871?mode=listener";
        let (video, audio) = (acquire(url).unwrap(), acquire(url).unwrap());
        assert!(Arc::ptr_eq(&video, &audio), "one socket per listener URL");
        assert!(video.open_reader().unwrap().url().starts_with("pipe:"));
        let err = acquire("srt://[::1]:29872?mode=listener&fec=hunter2").unwrap_err();
        assert!(err.contains("fec") && !err.contains("hunter2"), "{err}");
    }

    /// SRT-02: a bridge serves at most `MAX_READERS` readers (no thread per
    /// reader), a dropped reader frees its slot, and a reader that never reads
    /// holds a bounded backlog instead of growing.
    #[test]
    fn readers_are_capped_and_an_idle_reader_stays_bounded() {
        let bridge = acquire("srt://[::1]:29875?mode=listener").unwrap();
        let mut held: Vec<_> = (0..MAX_READERS)
            .map(|_| bridge.open_reader().unwrap())
            .collect();
        let refused = bridge.open_reader().unwrap_err();
        assert!(refused.contains("reader slots"), "{refused}");
        held.pop();
        let again = bridge.open_reader().unwrap();
        let message = [0x47_u8; 1316];
        let dropped = std::sync::atomic::AtomicU64::new(0);
        {
            let mut readers = lock(&bridge.readers);
            let reader = readers.list.iter_mut().last().unwrap();
            // Nobody reads `again`: ~10 MB offered, far past socket + backlog.
            for _ in 0..8_000 {
                assert!(reader.push(&message, &dropped));
            }
            assert!(reader.backlog_bytes <= READER_BACKLOG_BYTES);
        }
        assert!(dropped.load(std::sync::atomic::Ordering::Relaxed) > 0);
        drop(again);
        assert_eq!(lock(&bridge.readers).list.len(), MAX_READERS - 1);
    }

    /// A real caller over each family reaches a real in-process libav reader
    /// on `[::]`, through the private reader exactly as the pipeline opens it.
    #[test]
    fn wildcard_input_decodes_from_ipv6_and_ipv4_callers() {
        ffmpeg_next::init().unwrap();
        let port = 22_000 + u16::try_from(std::process::id() % 20_000).unwrap();
        let bridge = acquire(&format!("srt://[::]:{port}?mode=listener")).unwrap();
        for host in ["[::1]", "127.0.0.1"] {
            let mut caller = Command::new("ffmpeg")
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-re",
                    "-f",
                    "lavfi",
                    "-i",
                ])
                .arg("testsrc=size=160x120:rate=30")
                .args(["-c:v", "mpeg2video", "-g", "10", "-f", "mpegts"])
                .arg(format!("srt://{host}:{port}?mode=caller"))
                .stdin(Stdio::null())
                .spawn()
                .expect("spawn caller");
            let reader = bridge.open_reader().unwrap();
            let mut opts = ffmpeg_next::Dictionary::new();
            opts.set("rw_timeout", "10000000");
            let video_packets = ffmpeg_next::format::input_with_dictionary(&reader.url(), opts)
                .map_or(0, |mut input| {
                    let index = input
                        .streams()
                        .best(ffmpeg_next::media::Type::Video)
                        .map(|s| s.index());
                    input
                        .packets()
                        .take(200)
                        .filter(|(stream, _)| Some(stream.index()) == index)
                        .count()
                });
            caller.kill().expect("kill caller");
            caller.wait().expect("reap caller");
            assert!(video_packets >= 15, "{video_packets} packets over {host}");
            drop(reader);
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
    }
}
