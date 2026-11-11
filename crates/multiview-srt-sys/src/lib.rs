//! Minimal runtime-loaded **libsrt** binding: a dual-stack SRT listener.
//!
//! # Why this crate exists
//!
//! `FFmpeg`'s `libsrt` URL protocol (8.1.1, byte-identical to 8.1) cannot bind the
//! IPv6 wildcard: libsrt 1.5 rejects `srt_bind` on `[::]` unless
//! `SRTO_IPV6ONLY` was set explicitly (`api.cpp`: "when binding to :: (IPv6
//! wildcard), `SRTO_IPV6ONLY` option must be set explicitly to 0 or 1" →
//! `MJ_NOTSUP/MN_INVAL`, surfaced as "Operation not supported: Bad parameters"),
//! and `libsrt.c` exposes no option to set it. The repository rule is
//! dual-stack `[::]`, so the listener half of SRT (both an SRT *output* that
//! callers pull from and an SRT *input* that a remote encoder pushes to) is
//! owned here: one `AF_INET6` socket with `SRTO_IPV6ONLY = 0` accepts callers
//! over IPv6 **and** IPv4 (v4-mapped) on the same port.
//!
//! # Shape
//!
//! * [`SrtLib`] — the `dlopen`-resolved function table (`libsrt.so.1.5` or the
//!   Debian `libsrt-gnutls.so.1.5` / `libsrt-openssl.so.1.5` flavours). Nothing
//!   is linked at build time, so the default build never needs libsrt.
//! * [`SrtListener`] — bind + listen (backlog > 1) + an interruptible
//!   [`accept`](SrtListener::accept) that polls with a short epoll timeout and a
//!   caller-supplied stop predicate, so it never wedges a thread or `stop`.
//! * [`SrtSocket`] — one accepted connection: [`send`](SrtSocket::send) /
//!   [`recv`](SrtSocket::recv) in libsrt *message* (live) mode, each bounded by a
//!   timeout so a dead peer surfaces as an error instead of a hang.
//!
//! All FFI is in this crate (`unsafe_code = deny` with `// SAFETY:` notes); the
//! consumers stay `forbid(unsafe_code)`. No callback is registered with libsrt,
//! so no Rust code ever runs on a libsrt thread.

// reason: this leaf IS the raw libsrt FFI boundary; every block carries a SAFETY note.
#![allow(unsafe_code)]

use std::ffi::{c_char, c_int, c_void, CStr};
use std::net::{IpAddr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use libloading::Library;

mod sa;

/// libsrt socket handle (`SRTSOCKET`, an `int32_t`).
type SrtSock = i32;

/// `SRT_ERROR` / `SRT_INVALID_SOCK`.
const SRT_ERROR: c_int = -1;
/// `SRT_EPOLL_IN` / `SRT_EPOLL_ERR`.
const EPOLL_IN: c_int = 0x1;
const EPOLL_ERR: c_int = 0x8;
/// `SRT_SOCKOPT` values from libsrt 1.5.4 `srt.h` (stable ABI since 1.3/1.4).
const SRTO_SNDSYN: c_int = 1;
const SRTO_RCVSYN: c_int = 2;
const SRTO_SNDTIMEO: c_int = 13;
const SRTO_RCVTIMEO: c_int = 14;
const SRTO_REUSEADDR: c_int = 15;
const SRTO_MAXBW: c_int = 16;
const SRTO_SENDER: c_int = 21;
const SRTO_LATENCY: c_int = 23;
const SRTO_OHEADBW: c_int = 25;
const SRTO_PASSPHRASE: c_int = 26;
const SRTO_PBKEYLEN: c_int = 27;
const SRTO_TRANSTYPE: c_int = 50;
const SRTO_IPV6ONLY: c_int = 54;
/// `SRTT_LIVE`.
const SRTT_LIVE: c_int = 0;
/// libsrt's live-mode payload (`SRT_LIVE_DEF_PLSIZE` = 7 × 188 TS packets).
pub const LIVE_PAYLOAD: usize = 1316;
/// `SRT_EMN(MJ_AGAIN, 0)`: every code in `6000..7000` is a transient "again".
const SRT_AGAIN_MIN: c_int = 6000;
const SRT_AGAIN_MAX: c_int = 6999;
/// `srt_epoll_wait` slice used to poll the stop predicate.
const POLL_MS: i64 = 100;

/// Errors from the libsrt binding.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SrtError {
    /// libsrt could not be `dlopen`-resolved, or a symbol is missing.
    #[error("libsrt unavailable: {0}")]
    Unavailable(String),
    /// A libsrt call failed; carries the call name and libsrt's error text.
    #[error("{call} failed: {message}")]
    Call {
        /// The libsrt entry point.
        call: &'static str,
        /// `srt_getlasterror_str()`.
        message: String,
    },
    /// The operation was aborted by the caller's stop predicate.
    #[error("interrupted")]
    Interrupted,
    /// The operation did not complete within its timeout.
    #[error("timed out")]
    TimedOut,
    /// The peer closed the connection.
    #[error("connection closed by peer")]
    Closed,
    /// A listener URL carries an option this binding does not apply. Failing
    /// closed beats silently ignoring e.g. an encryption setting. Names the key
    /// only, never its value.
    #[error("unsupported SRT listener option {0:?}")]
    UnsupportedOption(String),
    /// An argument was invalid (e.g. a passphrase with an interior NUL).
    #[error("invalid argument: {0}")]
    Invalid(&'static str),
}

/// Result alias.
pub type Result<T> = std::result::Result<T, SrtError>;

/// One libsrt event record (`SRT_EPOLL_EVENT`).
#[repr(C)]
#[derive(Clone, Copy)]
struct EpollEvent {
    fd: SrtSock,
    events: c_int,
}

type FnVoid = unsafe extern "C" fn() -> c_int;
type FnCreate = unsafe extern "C" fn() -> SrtSock;
type FnBind = unsafe extern "C" fn(SrtSock, *const sa::Sockaddr, c_int) -> c_int;
type FnListen = unsafe extern "C" fn(SrtSock, c_int) -> c_int;
type FnAccept = unsafe extern "C" fn(SrtSock, *mut sa::Sockaddr, *mut c_int) -> SrtSock;
type FnClose = unsafe extern "C" fn(SrtSock) -> c_int;
type FnSetFlag = unsafe extern "C" fn(SrtSock, c_int, *const c_void, c_int) -> c_int;
type FnSend = unsafe extern "C" fn(SrtSock, *const c_char, c_int, c_int, c_int) -> c_int;
type FnRecv = unsafe extern "C" fn(SrtSock, *mut c_char, c_int) -> c_int;
type FnErrStr = unsafe extern "C" fn() -> *const c_char;
type FnErrCode = unsafe extern "C" fn(*mut c_int) -> c_int;
type FnEpollCreate = unsafe extern "C" fn() -> c_int;
type FnEpollAdd = unsafe extern "C" fn(c_int, SrtSock, *const c_int) -> c_int;
type FnEpollUwait = unsafe extern "C" fn(c_int, *mut EpollEvent, c_int, i64) -> c_int;
type FnEpollRelease = unsafe extern "C" fn(c_int) -> c_int;

/// The resolved libsrt function table. Holds the `Library` so every function
/// pointer stays valid for the table's lifetime.
pub struct SrtLib {
    _lib: Library,
    cleanup: FnVoid,
    create_socket: FnCreate,
    bind: FnBind,
    listen: FnListen,
    accept: FnAccept,
    close: FnClose,
    setsockflag: FnSetFlag,
    sendmsg: FnSend,
    recvmsg: FnRecv,
    getlasterror_str: FnErrStr,
    getlasterror: FnErrCode,
    epoll_create: FnEpollCreate,
    epoll_add_usock: FnEpollAdd,
    epoll_uwait: FnEpollUwait,
    epoll_release: FnEpollRelease,
}

impl std::fmt::Debug for SrtLib {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SrtLib")
    }
}

impl Drop for SrtLib {
    fn drop(&mut self) {
        // SAFETY: balances the one `srt_startup` in `load`; no socket created
        // through this table outlives it (every handle holds an `Arc<SrtLib>`).
        unsafe {
            (self.cleanup)();
        }
    }
}

/// Library names tried in order, per platform: Linux the upstream SONAME then
/// the Debian flavours; macOS the Homebrew/upstream install names; Windows the
/// vcpkg/upstream DLL.
#[cfg(not(any(target_os = "macos", windows)))]
const SONAMES: &[&str] = &[
    "libsrt.so.1.5",
    "libsrt-openssl.so.1.5",
    "libsrt-gnutls.so.1.5",
    "libsrt.so",
];
#[cfg(target_os = "macos")]
const SONAMES: &[&str] = &["libsrt.1.5.dylib", "libsrt.dylib"];
#[cfg(windows)]
const SONAMES: &[&str] = &["srt.dll", "libsrt.dll"];

static SHARED: OnceLock<std::result::Result<Arc<SrtLib>, String>> = OnceLock::new();

impl SrtLib {
    /// The process-wide libsrt table, `dlopen`ed once.
    ///
    /// # Errors
    /// [`SrtError::Unavailable`] when no libsrt SONAME resolves or a required
    /// symbol is missing.
    pub fn shared() -> Result<Arc<Self>> {
        SHARED
            .get_or_init(|| Self::load().map(Arc::new).map_err(|e| e.to_string()))
            .clone()
            .map_err(SrtError::Unavailable)
    }

    fn load() -> Result<Self> {
        Self::load_from(SONAMES)
    }

    /// `dlopen` the first SONAME in `names` that resolves. Factored out of
    /// [`load`](Self::load) so a test can inject a deliberately-missing list
    /// and exercise the "libsrt unavailable" path without depending on the
    /// host's real install (and without touching the process-wide
    /// [`shared`](Self::shared) cache).
    fn load_from(names: &[&str]) -> Result<Self> {
        let mut last = String::from("no SONAME tried");
        for &name in names {
            // SAFETY: `dlopen` of a well-known system library; its initialisers
            // are libsrt's own (no Rust code). A failure is just reported.
            match unsafe { Library::new(name) } {
                Ok(lib) => return Self::resolve(lib),
                Err(e) => last = format!("{name}: {e}"),
            }
        }
        Err(SrtError::Unavailable(last))
    }

    fn resolve(lib: Library) -> Result<Self> {
        macro_rules! sym {
            ($name:literal, $ty:ty) => {{
                // SAFETY: the symbol is looked up by its exact libsrt name and
                // typed with the C signature from libsrt 1.5.4 `srt.h`.
                let s: libloading::Symbol<'_, $ty> = unsafe { lib.get($name) }.map_err(|e| {
                    SrtError::Unavailable(format!(
                        "{}: {e}",
                        String::from_utf8_lossy($name.strip_suffix(b"\0").unwrap_or($name))
                    ))
                })?;
                *s
            }};
        }
        let startup = sym!(b"srt_startup\0", FnVoid);
        let table = Self {
            cleanup: sym!(b"srt_cleanup\0", FnVoid),
            create_socket: sym!(b"srt_create_socket\0", FnCreate),
            bind: sym!(b"srt_bind\0", FnBind),
            listen: sym!(b"srt_listen\0", FnListen),
            accept: sym!(b"srt_accept\0", FnAccept),
            close: sym!(b"srt_close\0", FnClose),
            setsockflag: sym!(b"srt_setsockflag\0", FnSetFlag),
            sendmsg: sym!(b"srt_sendmsg\0", FnSend),
            recvmsg: sym!(b"srt_recvmsg\0", FnRecv),
            getlasterror_str: sym!(b"srt_getlasterror_str\0", FnErrStr),
            getlasterror: sym!(b"srt_getlasterror\0", FnErrCode),
            epoll_create: sym!(b"srt_epoll_create\0", FnEpollCreate),
            epoll_add_usock: sym!(b"srt_epoll_add_usock\0", FnEpollAdd),
            epoll_uwait: sym!(b"srt_epoll_uwait\0", FnEpollUwait),
            epoll_release: sym!(b"srt_epoll_release\0", FnEpollRelease),
            _lib: lib,
        };
        // SAFETY: library-wide init, balanced by `srt_cleanup` in `Drop`.
        if unsafe { startup() } < 0 {
            return Err(table.call_error("srt_startup"));
        }
        Ok(table)
    }

    /// Whether the calling thread's last libsrt error is a transient "again"
    /// (`MJ_AGAIN`: send/recv buffer unavailable or `SNDTIMEO`/`RCVTIMEO` hit).
    fn last_error_is_again(&self) -> bool {
        // SAFETY: a null errno out-pointer is documented as allowed.
        let code = unsafe { (self.getlasterror)(std::ptr::null_mut()) };
        (SRT_AGAIN_MIN..=SRT_AGAIN_MAX).contains(&code)
    }

    fn call_error(&self, call: &'static str) -> SrtError {
        // SAFETY: returns a pointer to libsrt's thread-local error text (valid
        // until the next libsrt call on this thread); copied immediately.
        let message = unsafe {
            let p = (self.getlasterror_str)();
            if p.is_null() {
                String::from("unknown error")
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        };
        SrtError::Call { call, message }
    }

    fn set_int(&self, sock: SrtSock, opt: c_int, value: c_int, call: &'static str) -> Result<()> {
        let size = c_int::try_from(std::mem::size_of::<c_int>()).unwrap_or(4);
        // SAFETY: `value` lives across the call; libsrt copies `size` bytes.
        let r = unsafe { (self.setsockflag)(sock, opt, std::ptr::from_ref(&value).cast(), size) };
        if r == SRT_ERROR {
            return Err(self.call_error(call));
        }
        Ok(())
    }

    fn set_i64(&self, sock: SrtSock, opt: c_int, value: i64, call: &'static str) -> Result<()> {
        let size = c_int::try_from(std::mem::size_of::<i64>()).unwrap_or(8);
        // SAFETY: as `set_int`, for the 64-bit options (`SRTO_MAXBW`).
        let r = unsafe { (self.setsockflag)(sock, opt, std::ptr::from_ref(&value).cast(), size) };
        if r == SRT_ERROR {
            return Err(self.call_error(call));
        }
        Ok(())
    }
}

/// Socket options applied to the listener (and inherited by accepted sockets).
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ListenerOptions {
    /// Latency in milliseconds (`SRTO_LATENCY`); `None` keeps libsrt's 120 ms.
    pub latency_ms: Option<u32>,
    /// AES key length in bytes (16/24/32) with its passphrase.
    pub encryption: Option<(u32, String)>,
    /// `SRTO_OHEADBW` percent.
    pub oheadbw_pct: Option<u32>,
    /// `SRTO_MAXBW` bytes/second.
    pub maxbw: Option<i64>,
    /// `true` for a sender (output), `false` for a receiver (input).
    pub sender: bool,
}

impl ListenerOptions {
    /// Options for an SRT **output** listener (we send to callers).
    #[must_use]
    pub fn sender() -> Self {
        Self {
            sender: true,
            ..Self::default()
        }
    }

    /// Options for an SRT **input** listener (callers send to us).
    #[must_use]
    pub fn receiver() -> Self {
        Self::default()
    }

    /// Set the latency (ms).
    #[must_use]
    pub fn with_latency_ms(mut self, ms: Option<u32>) -> Self {
        self.latency_ms = ms;
        self
    }

    /// Set AES encryption (`key_len` bytes) with `passphrase`.
    #[must_use]
    pub fn with_encryption(mut self, key_len: u32, passphrase: String) -> Self {
        self.encryption = Some((key_len, passphrase));
        self
    }

    /// Set the overhead-bandwidth percent / max bandwidth.
    #[must_use]
    pub fn with_bandwidth(mut self, oheadbw_pct: Option<u32>, maxbw: Option<i64>) -> Self {
        self.oheadbw_pct = oheadbw_pct;
        self.maxbw = maxbw;
        self
    }
}

/// A listener's bind address and socket options, parsed from its URL.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ListenerSpec {
    /// Where to bind.
    pub addr: SocketAddr,
    /// The socket options.
    pub options: ListenerOptions,
}

/// Decode a query value the way libav's `ff_urldecode` does (`%XX`, `+` = space).
fn url_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        let hex = (b == b'%')
            .then(|| value.get(i + 1..i + 3))
            .flatten()
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        if let Some(decoded) = hex {
            out.push(decoded);
            i += 3;
        } else {
            out.push(if b == b'+' { b' ' } else { b });
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse an `srt://host:port?mode=listener&…` URL into a [`ListenerSpec`].
///
/// Query values keep the libav units the URL lowering emits (`latency` is in
/// **microseconds**). `sender` selects output vs input. An empty host binds the
/// dual-stack wildcard. Options with no effect on a listener are accepted; any
/// other key fails closed with [`SrtError::UnsupportedOption`].
///
/// # Errors
/// [`SrtError::Invalid`] for a malformed URL or value;
/// [`SrtError::UnsupportedOption`] as above.
pub fn parse_listener_url(url: &str, sender: bool) -> Result<ListenerSpec> {
    use std::net::ToSocketAddrs;

    let (authority, query) = url.split_once('?').unwrap_or((url, ""));
    let host_port = authority
        .strip_prefix("srt://")
        .ok_or(SrtError::Invalid("scheme is not srt"))?;
    let (host, port) = host_port
        .rsplit_once(':')
        .ok_or(SrtError::Invalid("port is missing"))?;
    let port = port
        .parse::<u16>()
        .map_err(|_| SrtError::Invalid("port is invalid"))?;
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    let bare = if bare.is_empty() { "::" } else { bare };
    let addr = (bare, port)
        .to_socket_addrs()
        .map_err(|_| SrtError::Invalid("host does not resolve"))?
        .next()
        .ok_or(SrtError::Invalid("host does not resolve"))?;

    let mut options = ListenerOptions {
        sender,
        ..ListenerOptions::default()
    };
    let mut passphrase = None;
    let mut key_len = None;
    for pair in query.split('&').filter(|part| !part.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        match key {
            "latency" => {
                let micros = value
                    .parse::<u64>()
                    .map_err(|_| SrtError::Invalid("latency"))?;
                let ms = u32::try_from(micros / 1000).map_err(|_| SrtError::Invalid("latency"))?;
                options.latency_ms = Some(ms);
            }
            "passphrase" => passphrase = Some(url_decode(value)),
            "pbkeylen" => {
                key_len = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| SrtError::Invalid("pbkeylen"))?,
                );
            }
            "oheadbw" => {
                options.oheadbw_pct = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| SrtError::Invalid("oheadbw"))?,
                );
            }
            "maxbw" => {
                options.maxbw = Some(
                    value
                        .parse::<i64>()
                        .map_err(|_| SrtError::Invalid("maxbw"))?,
                );
            }
            "transtype" if value == "live" => {}
            // No effect on a listener: the mode is implied, a stream id is the
            // caller's, and accept waits are owned by the stop predicate.
            "mode" | "streamid" | "connect_timeout" | "timeout" | "listen_timeout" | "pkt_size"
            | "payload_size" => {}
            other => return Err(SrtError::UnsupportedOption(other.to_owned())),
        }
    }
    if let Some(passphrase) = passphrase {
        options.encryption = Some((key_len.unwrap_or(16), passphrase));
    }
    Ok(ListenerSpec { addr, options })
}

/// A bound, listening SRT socket. Closed on drop.
#[derive(Debug)]
pub struct SrtListener {
    lib: Arc<SrtLib>,
    sock: SrtSock,
    eid: c_int,
    local: SocketAddr,
}

// SAFETY: libsrt socket ids and epoll ids are plain integers usable from any
// thread (libsrt's API is thread-safe per socket); the table is immutable.
unsafe impl Send for SrtListener {}
// SAFETY: as above; `accept` takes `&self` and libsrt serialises internally.
unsafe impl Sync for SrtListener {}

/// Encode `addr` as a `sockaddr_in6` (IPv4 addresses become v4-mapped), the
/// only family an `IPV6ONLY = 0` socket accepts for a dual-stack bind.
fn sockaddr_v6(addr: SocketAddr) -> sa::SockaddrIn6 {
    let (ip, port, scope) = match addr {
        SocketAddr::V6(v6) => (*v6.ip(), v6.port(), v6.scope_id()),
        SocketAddr::V4(v4) => (v4.ip().to_ipv6_mapped(), v4.port(), 0),
    };
    // SAFETY: `sockaddr_in6` is plain-old-data; all-zero is a valid value.
    let mut sa: sa::SockaddrIn6 = unsafe { std::mem::zeroed() };
    sa.sin6_family = sa::af_inet6_family();
    sa.sin6_port = port.to_be();
    sa.sin6_addr.s6_addr = ip.octets();
    sa.sin6_scope_id = scope;
    sa
}

/// Decode a peer address libsrt wrote into a `sockaddr_storage`.
fn decode_sockaddr(storage: &sa::SockaddrStorage) -> Option<SocketAddr> {
    match c_int::from(storage.ss_family) {
        sa::AF_INET6 => {
            // SAFETY: the family says the storage holds a `sockaddr_in6`, which
            // fits in `sockaddr_storage` (its whole purpose) and is POD.
            let sa: sa::SockaddrIn6 = unsafe { std::ptr::read(std::ptr::from_ref(storage).cast()) };
            let ip = Ipv6Addr::from(sa.sin6_addr.s6_addr);
            let port = u16::from_be(sa.sin6_port);
            Some(match ip.to_ipv4_mapped() {
                Some(v4) => SocketAddr::V4(SocketAddrV4::new(v4, port)),
                None => SocketAddr::V6(SocketAddrV6::new(ip, port, 0, sa.sin6_scope_id)),
            })
        }
        sa::AF_INET => {
            // SAFETY: as above for `sockaddr_in`.
            let sa: sa::SockaddrIn = unsafe { std::ptr::read(std::ptr::from_ref(storage).cast()) };
            Some(SocketAddr::new(
                IpAddr::from(u32::from_be(sa.sin_addr.s_addr).to_be_bytes()),
                u16::from_be(sa.sin_port),
            ))
        }
        _ => None,
    }
}

impl SrtListener {
    /// Bind `addr` and listen. An IPv6 address (including `[::]`) is bound with
    /// `SRTO_IPV6ONLY = 0`, so the wildcard accepts IPv4 callers too
    /// (dual-stack); an IPv4 address binds as a v4-mapped IPv6 address with the
    /// same option, i.e. IPv4 traffic for that address only.
    ///
    /// # Errors
    /// [`SrtError::Unavailable`] without libsrt; [`SrtError::Call`] when socket
    /// creation, an option, bind or listen fails (e.g. the port is in use);
    /// [`SrtError::Invalid`] for a bad passphrase/key length.
    pub fn bind(addr: SocketAddr, options: &ListenerOptions, backlog: u16) -> Result<Self> {
        let lib = SrtLib::shared()?;
        // SAFETY: plain socket creation; the id is closed on every error path
        // below via the guard, and on drop otherwise.
        let sock = unsafe { (lib.create_socket)() };
        if sock == SRT_ERROR {
            return Err(lib.call_error("srt_create_socket"));
        }
        let mut guard = SockGuard {
            lib: Arc::clone(&lib),
            sock,
            armed: true,
        };
        lib.set_int(sock, SRTO_TRANSTYPE, SRTT_LIVE, "SRTO_TRANSTYPE")?;
        lib.set_int(sock, SRTO_IPV6ONLY, 0, "SRTO_IPV6ONLY")?;
        lib.set_int(sock, SRTO_REUSEADDR, 1, "SRTO_REUSEADDR")?;
        // Non-blocking accept; accepted sockets are switched to blocking with
        // bounded SNDTIMEO/RCVTIMEO in `SrtSocket::configure`.
        lib.set_int(sock, SRTO_RCVSYN, 0, "SRTO_RCVSYN")?;
        lib.set_int(sock, SRTO_SNDSYN, 0, "SRTO_SNDSYN")?;
        if options.sender {
            lib.set_int(sock, SRTO_SENDER, 1, "SRTO_SENDER")?;
        }
        if let Some(ms) = options.latency_ms {
            let ms = c_int::try_from(ms).map_err(|_| SrtError::Invalid("latency"))?;
            lib.set_int(sock, SRTO_LATENCY, ms, "SRTO_LATENCY")?;
        }
        if let Some(pct) = options.oheadbw_pct {
            let pct = c_int::try_from(pct).map_err(|_| SrtError::Invalid("oheadbw"))?;
            lib.set_int(sock, SRTO_OHEADBW, pct, "SRTO_OHEADBW")?;
        }
        if let Some(maxbw) = options.maxbw {
            lib.set_i64(sock, SRTO_MAXBW, maxbw, "SRTO_MAXBW")?;
        }
        if let Some((key_len, passphrase)) = &options.encryption {
            let key_len = c_int::try_from(*key_len).map_err(|_| SrtError::Invalid("pbkeylen"))?;
            lib.set_int(sock, SRTO_PBKEYLEN, key_len, "SRTO_PBKEYLEN")?;
            if passphrase.as_bytes().contains(&0) {
                return Err(SrtError::Invalid("passphrase contains NUL"));
            }
            let len = c_int::try_from(passphrase.len())
                .map_err(|_| SrtError::Invalid("passphrase length"))?;
            // SAFETY: libsrt copies `len` bytes from the passphrase buffer,
            // which outlives the call.
            let r = unsafe {
                (lib.setsockflag)(sock, SRTO_PASSPHRASE, passphrase.as_ptr().cast(), len)
            };
            if r == SRT_ERROR {
                return Err(lib.call_error("SRTO_PASSPHRASE"));
            }
        }
        let sa = sockaddr_v6(addr);
        let sa_len = c_int::try_from(std::mem::size_of::<sa::SockaddrIn6>()).unwrap_or(28);
        // SAFETY: `sa` is a fully-initialised `sockaddr_in6` of `sa_len` bytes.
        if unsafe { (lib.bind)(sock, std::ptr::from_ref(&sa).cast(), sa_len) } == SRT_ERROR {
            return Err(lib.call_error("srt_bind"));
        }
        // SAFETY: plain call on our bound socket.
        if unsafe { (lib.listen)(sock, c_int::from(backlog.max(1))) } == SRT_ERROR {
            return Err(lib.call_error("srt_listen"));
        }
        // SAFETY: plain epoll creation, released in `Drop`.
        let eid = unsafe { (lib.epoll_create)() };
        if eid < 0 {
            return Err(lib.call_error("srt_epoll_create"));
        }
        let events = EPOLL_IN | EPOLL_ERR;
        // SAFETY: `events` outlives the call; `eid`/`sock` are ours.
        if unsafe { (lib.epoll_add_usock)(eid, sock, &raw const events) } == SRT_ERROR {
            let err = lib.call_error("srt_epoll_add_usock");
            // SAFETY: release the epoll we just created.
            unsafe { (lib.epoll_release)(eid) };
            return Err(err);
        }
        guard.armed = false;
        Ok(Self {
            lib,
            sock,
            eid,
            local: addr,
        })
    }

    /// The address this listener was bound to.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// Wait for and accept one caller. Polls in short slices and returns
    /// [`SrtError::Interrupted`] as soon as `should_stop()` is true, so it never
    /// wedges a thread.
    ///
    /// # Errors
    /// [`SrtError::Interrupted`] on stop; [`SrtError::Call`] on a libsrt failure.
    pub fn accept(
        &self,
        should_stop: &dyn Fn() -> bool,
    ) -> Result<(SrtSocket, Option<SocketAddr>)> {
        loop {
            if should_stop() {
                return Err(SrtError::Interrupted);
            }
            let mut ev = [EpollEvent { fd: -1, events: 0 }; 1];
            // SAFETY: `ev` has room for exactly 1 event as declared.
            let n = unsafe { (self.lib.epoll_uwait)(self.eid, ev.as_mut_ptr(), 1, POLL_MS) };
            if n < 0 {
                return Err(self.lib.call_error("srt_epoll_uwait"));
            }
            if n == 0 {
                continue;
            }
            // SAFETY: `sockaddr_storage` is POD; all-zero is valid.
            let mut storage: sa::SockaddrStorage = unsafe { std::mem::zeroed() };
            let mut len =
                c_int::try_from(std::mem::size_of::<sa::SockaddrStorage>()).unwrap_or(128);
            // SAFETY: `storage` has `len` bytes for libsrt to write the peer.
            let sock = unsafe {
                (self.lib.accept)(
                    self.sock,
                    std::ptr::from_mut(&mut storage).cast(),
                    &raw mut len,
                )
            };
            if sock == SRT_ERROR {
                // A spurious wake (no pending caller) is retried.
                continue;
            }
            let socket = SrtSocket::configure(Arc::clone(&self.lib), sock)?;
            return Ok((socket, decode_sockaddr(&storage)));
        }
    }
}

impl Drop for SrtListener {
    fn drop(&mut self) {
        // SAFETY: we own both ids; each is released exactly once here.
        unsafe {
            (self.lib.epoll_release)(self.eid);
            (self.lib.close)(self.sock);
        }
    }
}

/// Closes a socket on an early-return path.
struct SockGuard {
    lib: Arc<SrtLib>,
    sock: SrtSock,
    /// Cleared once the socket is handed to its owner.
    armed: bool,
}

impl Drop for SockGuard {
    fn drop(&mut self) {
        if self.armed {
            // SAFETY: closes the socket this guard still owns, exactly once.
            unsafe {
                (self.lib.close)(self.sock);
            }
        }
    }
}

/// One accepted SRT connection (live/message mode). Closed on drop.
#[derive(Debug)]
pub struct SrtSocket {
    lib: Arc<SrtLib>,
    sock: SrtSock,
}

// SAFETY: see `SrtListener` — an integer handle into libsrt's thread-safe API.
unsafe impl Send for SrtSocket {}

/// Default per-call I/O bound for an accepted socket.
const IO_TIMEOUT_MS: c_int = 500;

impl SrtSocket {
    fn configure(lib: Arc<SrtLib>, sock: SrtSock) -> Result<Self> {
        let socket = Self { lib, sock };
        // Blocking calls, each bounded so a dead peer cannot hang a thread.
        socket.lib.set_int(sock, SRTO_SNDSYN, 1, "SRTO_SNDSYN")?;
        socket.lib.set_int(sock, SRTO_RCVSYN, 1, "SRTO_RCVSYN")?;
        socket
            .lib
            .set_int(sock, SRTO_SNDTIMEO, IO_TIMEOUT_MS, "SRTO_SNDTIMEO")?;
        socket
            .lib
            .set_int(sock, SRTO_RCVTIMEO, IO_TIMEOUT_MS, "SRTO_RCVTIMEO")?;
        Ok(socket)
    }

    /// Send `data` as live-mode messages of at most [`LIVE_PAYLOAD`] bytes,
    /// all of it before the absolute `deadline`.
    ///
    /// `should_stop()` and `deadline` are checked before EVERY chunk and every
    /// retry, not only after a failed send, so a slow-but-steady peer can never
    /// stretch one call past `deadline` or ignore stop (review SRT-05). Each
    /// libsrt call is additionally bounded by the per-call socket timeout
    /// ([`IO_TIMEOUT_MS`]), so the call returns within `deadline` plus at most
    /// one such timeout.
    ///
    /// # Errors
    /// [`SrtError::TimedOut`] at `deadline`; [`SrtError::Interrupted`] on
    /// stop; [`SrtError::Call`] when the connection is broken.
    pub fn send(
        &self,
        data: &[u8],
        deadline: Instant,
        should_stop: &dyn Fn() -> bool,
    ) -> Result<()> {
        for chunk in data.chunks(LIVE_PAYLOAD) {
            let len = c_int::try_from(chunk.len()).map_err(|_| SrtError::Invalid("chunk"))?;
            loop {
                if should_stop() {
                    return Err(SrtError::Interrupted);
                }
                if Instant::now() >= deadline {
                    return Err(SrtError::TimedOut);
                }
                // SAFETY: `chunk` is `len` readable bytes for the call.
                let r = unsafe { (self.lib.sendmsg)(self.sock, chunk.as_ptr().cast(), len, -1, 1) };
                if r != SRT_ERROR {
                    break;
                }
                if !self.lib.last_error_is_again() {
                    return Err(self.lib.call_error("srt_sendmsg"));
                }
            }
        }
        Ok(())
    }

    /// Receive one live-mode message into `buf` (at least [`LIVE_PAYLOAD`]
    /// bytes). Returns `Ok(None)` on a receive timeout (no data yet).
    ///
    /// # Errors
    /// [`SrtError::Closed`] / [`SrtError::Call`] when the connection is broken.
    pub fn recv(&self, buf: &mut [u8]) -> Result<Option<usize>> {
        let len = c_int::try_from(buf.len()).unwrap_or(c_int::MAX);
        // SAFETY: `buf` is `len` writable bytes for the call.
        let r = unsafe { (self.lib.recvmsg)(self.sock, buf.as_mut_ptr().cast(), len) };
        if r == SRT_ERROR {
            if self.lib.last_error_is_again() {
                return Ok(None);
            }
            return Err(self.lib.call_error("srt_recvmsg"));
        }
        if r == 0 {
            return Err(SrtError::Closed);
        }
        Ok(Some(usize::try_from(r).unwrap_or(0)))
    }
}

impl Drop for SrtSocket {
    fn drop(&mut self) {
        // SAFETY: closes the accepted socket we own, exactly once.
        unsafe {
            (self.lib.close)(self.sock);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn listener_url_parses_units_secrets_and_fails_closed() {
        let spec = parse_listener_url(
            "srt://[::]:9000?mode=listener&latency=200000&pbkeylen=32&passphrase=a%26b+c&maxbw=7",
            true,
        )
        .unwrap();
        assert_eq!(spec.addr, "[::]:9000".parse().unwrap());
        assert_eq!(spec.options.latency_ms, Some(200));
        assert_eq!(spec.options.encryption, Some((32, "a&b c".to_owned())));
        assert_eq!(spec.options.maxbw, Some(7));
        assert!(spec.options.sender);
        // An empty host is the dual-stack wildcard.
        let spec = parse_listener_url("srt://:9000?mode=listener", false).unwrap();
        assert_eq!(spec.addr, "[::]:9000".parse().unwrap());
        // A specific address stays specific.
        let spec = parse_listener_url("srt://[2001:db8::10]:9000", false).unwrap();
        assert_eq!(spec.addr, "[2001:db8::10]:9000".parse().unwrap());
        // Unknown keys are refused by name, and the value is not echoed.
        let err = parse_listener_url("srt://[::]:9000?enforced_encryption=hunter2", false)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("enforced_encryption") && !err.contains("hunter2"),
            "{err}"
        );
    }

    #[test]
    fn v4_binds_as_mapped_v6() {
        let sa = sockaddr_v6("127.0.0.1:9000".parse().unwrap());
        assert_eq!(c_int::from(sa.sin6_family), sa::AF_INET6);
        assert_eq!(u16::from_be(sa.sin6_port), 9000);
        assert_eq!(
            Ipv6Addr::from(sa.sin6_addr.s6_addr).to_ipv4_mapped(),
            Some("127.0.0.1".parse().unwrap())
        );
    }

    /// Missing-library path (ADR-0039 §3): an injected SONAME list that cannot
    /// possibly resolve on any host proves `SrtLib::load_from` fails closed
    /// with a clear [`SrtError::Unavailable`] message — never a panic — and
    /// that [`SrtListener::bind`] (which calls through the SAME `dlopen` path
    /// via [`SrtLib::shared`]) surfaces that same clear error rather than
    /// crashing, so a caller's retry loop (the push listener / ingest bridge)
    /// keeps the rest of the program running.
    #[test]
    fn missing_library_fails_closed_with_a_clear_error_never_a_panic() {
        let err = SrtLib::load_from(&["libsrt-definitely-not-installed.so.999"]).unwrap_err();
        let SrtError::Unavailable(message) = &err else {
            panic!("expected Unavailable, got {err:?}");
        };
        assert!(
            message.contains("libsrt-definitely-not-installed.so.999"),
            "{message}"
        );
        // The same shape surfaces through the public error Display without
        // panicking (mirrors what a listener's retry loop logs).
        assert!(err.to_string().contains("libsrt unavailable"));
    }

    #[test]
    fn mapped_peer_decodes_as_v4() {
        let sa = sockaddr_v6("127.0.0.1:4000".parse().unwrap());
        // SAFETY (test): copy the sockaddr_in6 into a zeroed storage.
        let mut storage: sa::SockaddrStorage = unsafe { std::mem::zeroed() };
        unsafe {
            std::ptr::copy_nonoverlapping(
                std::ptr::from_ref(&sa).cast::<u8>(),
                std::ptr::from_mut(&mut storage).cast::<u8>(),
                std::mem::size_of::<sa::SockaddrIn6>(),
            );
        }
        assert_eq!(
            decode_sockaddr(&storage),
            Some("127.0.0.1:4000".parse().unwrap())
        );
    }
}
