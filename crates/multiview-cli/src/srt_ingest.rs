//! Dual-stack SRT **listener input**.
//!
//! libav cannot bind `srt://[::]:port` (see `multiview_srt_sys`), and a source's
//! video and audio rails each open their own libav context, so two contexts
//! would contend for one listener port. One [`SrtIngestBridge`] per listener URL
//! therefore owns the native socket and relays the caller's MPEG-TS to any number
//! of local readers over `tcp://[::1]:<ephemeral>`, the loopback shape
//! `hls_fetch` already uses. Everything downstream (demux, normalise, supervised
//! reconnect, last-good store) is unchanged.
//!
//! When the caller goes away the local connections close, so libav sees EOF and
//! the ordinary reconnect bracket runs while the tile rides `NO_SIGNAL`. With no
//! caller a local reader simply times out on `rw_timeout` and retries. Each
//! reader has a bounded queue that drops on overflow (invariant #10). One caller
//! is served at a time; a later caller waits in the backlog until it ends.
//!
//! The relay is plaintext on the loopback interface after SRT decryption, so a
//! local process could read an encrypted source through it.

use std::collections::HashMap;
use std::io::Write;
use std::net::{Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};
use std::time::Duration;

use multiview_output::push_link::Backoff;
use multiview_srt_sys::{parse_listener_url, ListenerSpec, SrtError, SrtListener, SrtSocket};

/// Per-reader queue depth in SRT messages (~2.7 MB of 1316-byte payloads).
const READER_QUEUE_CAP: usize = 2048;
/// libsrt handshakes queued while a caller is being served.
const LISTEN_BACKLOG: u16 = 1;
/// Largest SRT message read in one call.
const MESSAGE_MAX: usize = 1500;
/// How often an idle relay checks that its reader is still connected.
const READER_IDLE_CHECK: Duration = Duration::from_secs(1);
/// A reader that cannot take bytes for this long is dropped.
const READER_WRITE_TIMEOUT: Duration = Duration::from_secs(2);
/// Poll step of the local accept loop and of interruptible sleeps.
const POLL: Duration = Duration::from_millis(50);

/// A local reader's queue, plus whether it has been fed by the current caller.
struct Reader {
    tx: SyncSender<Vec<u8>>,
    fed: bool,
}

type Readers = Arc<Mutex<Vec<Reader>>>;
type Registry = Mutex<HashMap<String, Weak<SrtIngestBridge>>>;

/// A running listener shared by every rail of one source. Stops when the last
/// holder drops it.
#[derive(Debug)]
pub struct SrtIngestBridge {
    local_url: String,
    stop: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
}

impl SrtIngestBridge {
    /// The loopback URL libav opens instead of the listener URL.
    #[must_use]
    pub fn local_url(&self) -> &str {
        &self.local_url
    }

    /// Messages shed because a local reader's queue was full.
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

fn start(spec: ListenerSpec) -> Result<SrtIngestBridge, String> {
    let local = TcpListener::bind(SocketAddr::from((Ipv6Addr::LOCALHOST, 0)))
        .map_err(|e| format!("srt relay bind: {e}"))?;
    local
        .set_nonblocking(true)
        .map_err(|e| format!("srt relay: {e}"))?;
    let port = local
        .local_addr()
        .map_err(|e| format!("srt relay: {e}"))?
        .port();
    let stop = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicU64::new(0));
    let readers: Readers = Arc::new(Mutex::new(Vec::new()));
    let relay_stop = Arc::clone(&stop);
    let relay_readers = Arc::clone(&readers);
    std::thread::Builder::new()
        .name("multiview-srt-relay".to_owned())
        .spawn(move || accept_readers(&local, &relay_readers, &relay_stop))
        .map_err(|e| format!("srt relay thread: {e}"))?;
    let srt_stop = Arc::clone(&stop);
    let srt_dropped = Arc::clone(&dropped);
    std::thread::Builder::new()
        .name("multiview-srt-listen".to_owned())
        .spawn(move || listen(&spec, &readers, &srt_dropped, &srt_stop))
        .map_err(|e| format!("srt listener thread: {e}"))?;
    Ok(SrtIngestBridge {
        local_url: format!("tcp://[::1]:{port}"),
        stop,
        dropped,
    })
}

fn sleep_unless_stopped(stop: &AtomicBool, total: Duration) {
    let mut left = total;
    while !stop.load(Ordering::Acquire) && !left.is_zero() {
        let step = left.min(POLL);
        std::thread::sleep(step);
        left = left.saturating_sub(step);
    }
}

/// Bind, then serve one caller at a time until stopped.
fn listen(spec: &ListenerSpec, readers: &Readers, dropped: &AtomicU64, stop: &Arc<AtomicBool>) {
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
                    lock(readers).retain(|reader| !reader.fed);
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

fn lock(readers: &Readers) -> std::sync::MutexGuard<'_, Vec<Reader>> {
    readers.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Pump one caller's messages to the readers, never waiting on any of them.
fn relay(socket: &SrtSocket, readers: &Readers, dropped: &AtomicU64, stop: &AtomicBool) {
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
        lock(readers).retain_mut(|reader| match reader.tx.try_send(message.to_vec()) {
            Ok(()) => {
                reader.fed = true;
                true
            }
            Err(TrySendError::Full(_)) => {
                dropped.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(TrySendError::Disconnected(_)) => false,
        });
    }
}

fn accept_readers(local: &TcpListener, readers: &Readers, stop: &AtomicBool) {
    while !stop.load(Ordering::Acquire) {
        match local.accept() {
            Ok((stream, _)) => {
                let (tx, rx) = sync_channel::<Vec<u8>>(READER_QUEUE_CAP);
                let spawned = std::thread::Builder::new()
                    .name("multiview-srt-reader".to_owned())
                    .spawn(move || feed_reader(stream, &rx));
                if spawned.is_ok() {
                    lock(readers).push(Reader { tx, fed: false });
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(POLL),
            Err(e) => {
                tracing::warn!(error = %e, "srt relay accept failed");
                std::thread::sleep(POLL);
            }
        }
    }
    lock(readers).clear();
}

/// Whether the local reader has hung up. Without this check an idle relay would
/// outlive every `rw_timeout` retry made while no caller is attached.
fn reader_gone(stream: &TcpStream) -> bool {
    let mut probe = [0_u8; 1];
    match stream.peek(&mut probe) {
        Ok(0) => true,
        Ok(_) => false,
        Err(e) => !matches!(
            e.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ),
    }
}

fn feed_reader(mut stream: TcpStream, rx: &Receiver<Vec<u8>>) {
    if stream.set_nonblocking(false).is_err()
        || stream.set_nodelay(true).is_err()
        || stream
            .set_write_timeout(Some(READER_WRITE_TIMEOUT))
            .is_err()
        || stream
            .set_read_timeout(Some(Duration::from_millis(1)))
            .is_err()
    {
        return;
    }
    loop {
        match rx.recv_timeout(READER_IDLE_CHECK) {
            Ok(message) => {
                if stream.write_all(&message).is_err() {
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if reader_gone(&stream) {
                    return;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::acquire;
    use std::process::{Command, Stdio};
    use std::sync::Arc;

    #[test]
    fn rails_share_one_bridge_and_a_bad_url_fails_closed_without_leaking() {
        let url = "srt://[::1]:29871?mode=listener";
        let (video, audio) = (acquire(url).unwrap(), acquire(url).unwrap());
        assert!(Arc::ptr_eq(&video, &audio), "one socket per listener URL");
        assert!(video.local_url().starts_with("tcp://[::1]:"));
        let err = acquire("srt://[::1]:29872?mode=listener&fec=hunter2").unwrap_err();
        assert!(err.contains("fec") && !err.contains("hunter2"), "{err}");
    }

    /// A real caller over each family reaches a real libav reader on `[::]`.
    #[test]
    fn wildcard_input_decodes_from_ipv6_and_ipv4_callers() {
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
            let status = Command::new("timeout")
                .args(["20", "ffmpeg", "-hide_banner", "-loglevel", "error", "-i"])
                .arg(bridge.local_url())
                .args(["-frames:v", "15", "-f", "null", "-"])
                .stdin(Stdio::null())
                .status()
                .expect("spawn reader");
            caller.kill().expect("kill caller");
            caller.wait().expect("reap caller");
            assert!(
                status.success(),
                "no frames from a caller over {host}: {status}"
            );
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
    }
}
