//! RFC 5424 syslog message formatting (pure) plus an optional UDP/TCP sender.
//!
//! Broadcast monitoring deployments expect alarms to fan out to a site syslog
//! collector. This module owns the **pure** RFC 5424 wire format — PRI
//! arithmetic, the structured-data grammar with its mandatory escaping, and the
//! `NILVALUE` handling — so it can be golden-vector tested with no I/O. Building
//! a [`SyslogMessage`] and calling [`SyslogMessage::to_rfc5424`] is allocation-
//! local and infallible.
//!
//! The actual datagram/stream delivery is **off by default**: enable the
//! `syslog` Cargo feature to pull in `UdpSender`/`TcpSender` (named as plain
//! code spans here as they are absent from the default doc build), which use
//! `std::net` only (no native deps, `unsafe_code` stays forbidden). Sending is
//! best-effort and must never back-pressure the engine (invariant #10): senders
//! are owned by the control/telemetry plane, never the hot path.
//!
//! ## Mapping from the alarm vocabulary
//!
//! [`Severity::from_alarm`] projects the X.733 [`PerceivedSeverity`] scale onto
//! the RFC 5424 severity scale. Note the two scales run in **opposite numeric
//! directions** (X.733 `Critical` is the *largest* value; syslog `Emergency` is
//! `0`), so the mapping is deliberately monotonic-but-inverting and is property
//! tested.
//!
//! References: RFC 5424 §6 (the syslog message), §6.2.1 (PRI), §6.3 (structured
//! data), §6.5 (worked examples).
use std::fmt::Write as _;

use multiview_core::alarm::PerceivedSeverity;

/// The RFC 5424 `NILVALUE` (a single hyphen) used for any absent field.
const NILVALUE: &str = "-";

/// RFC 5424 severity (`§6.2.1`, "Table 2 — Severity").
///
/// The discriminants are the on-wire numerical codes (`0..=7`, lower = more
/// urgent). `#[non_exhaustive]` is intentionally **not** applied: RFC 5424 fixes
/// exactly these eight severities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    /// System is unusable (code `0`).
    Emergency,
    /// Action must be taken immediately (code `1`).
    Alert,
    /// Critical conditions (code `2`).
    Critical,
    /// Error conditions (code `3`).
    Error,
    /// Warning conditions (code `4`).
    Warning,
    /// Normal but significant condition (code `5`).
    Notice,
    /// Informational messages (code `6`).
    Informational,
    /// Debug-level messages (code `7`).
    Debug,
}

impl Severity {
    /// The RFC 5424 numerical severity code (`0..=7`).
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Emergency => 0,
            Self::Alert => 1,
            Self::Critical => 2,
            Self::Error => 3,
            Self::Warning => 4,
            Self::Notice => 5,
            Self::Informational => 6,
            Self::Debug => 7,
        }
    }

    /// Project an X.733 [`PerceivedSeverity`] onto the syslog severity scale.
    ///
    /// The mapping is monotonic in urgency despite the inverted numeric scales:
    ///
    /// | X.733 `PerceivedSeverity` | RFC 5424 `Severity` |
    /// |---------------------------|---------------------|
    /// | `Critical`                | `Critical` (2)      |
    /// | `Major`                   | `Error` (3)         |
    /// | `Minor`                   | `Warning` (4)       |
    /// | `Warning`                 | `Notice` (5)        |
    /// | `Indeterminate`           | `Informational` (6) |
    /// | `Cleared`                 | `Informational` (6) |
    ///
    /// A `Cleared` condition is an informational *clear*, not an active fault,
    /// so it is never escalated above `Informational`.
    #[must_use]
    pub const fn from_alarm(severity: PerceivedSeverity) -> Self {
        match severity {
            PerceivedSeverity::Critical => Self::Critical,
            PerceivedSeverity::Major => Self::Error,
            PerceivedSeverity::Minor => Self::Warning,
            PerceivedSeverity::Warning => Self::Notice,
            // `Indeterminate`, `Cleared`, and (because `PerceivedSeverity` is
            // `#[non_exhaustive]`) any future severity map to the least-urgent
            // `Informational`, so a new variant can never be silently escalated
            // to a paging fault.
            _ => Self::Informational,
        }
    }
}

/// RFC 5424 facility (`§6.2.1`, "Table 1 — Facility").
///
/// Only the facilities Multiview emits under are modelled; the discriminants are
/// the on-wire facility codes. `#[non_exhaustive]` so further facilities can be
/// added without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Facility {
    /// Kernel messages (code `0`).
    Kernel,
    /// System daemons (code `3`).
    Daemon,
    /// Local use 0 (code `16`).
    Local0,
    /// Local use 1 (code `17`).
    Local1,
    /// Local use 2 (code `18`).
    Local2,
    /// Local use 3 (code `19`).
    Local3,
    /// Local use 4 (code `20`).
    Local4,
    /// Local use 5 (code `21`).
    Local5,
    /// Local use 6 (code `22`).
    Local6,
    /// Local use 7 (code `23`).
    Local7,
}

impl Facility {
    /// The RFC 5424 numerical facility code.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Kernel => 0,
            Self::Daemon => 3,
            Self::Local0 => 16,
            Self::Local1 => 17,
            Self::Local2 => 18,
            Self::Local3 => 19,
            Self::Local4 => 20,
            Self::Local5 => 21,
            Self::Local6 => 22,
            Self::Local7 => 23,
        }
    }

    /// Compute the RFC 5424 `PRIVAL` for this facility and a severity.
    ///
    /// `PRIVAL = facility * 8 + severity` (`§6.2.1`), always in `0..=191`.
    #[must_use]
    pub fn prival(self, severity: Severity) -> u16 {
        // facility <= 23, severity <= 7 => PRIVAL <= 191; widen via `From`
        // (no lossy `as` conversion).
        u16::from(self.code()) * 8 + u16::from(severity.code())
    }
}

/// One RFC 5424 `SD-ELEMENT`: an `SD-ID` plus ordered `SD-PARAM`s (`§6.3`).
///
/// Construct with [`SdElement::new`] and chain [`SdElement::param`]. Parameter
/// values are escaped per `§6.3.3` at format time, so callers pass raw strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SdElement {
    id: String,
    params: Vec<(String, String)>,
}

impl SdElement {
    /// Begin an `SD-ELEMENT` with the given `SD-ID` (e.g. `"exampleSDID@32473"`).
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            params: Vec::new(),
        }
    }

    /// Append an `SD-PARAM` (`PARAM-NAME="PARAM-VALUE"`). Values are escaped at
    /// format time; pass the raw, un-escaped value here.
    #[must_use]
    pub fn param(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.params.push((name.into(), value.into()));
        self
    }

    /// Render this element (including its enclosing brackets) into `out`.
    fn render(&self, out: &mut String) {
        out.push('[');
        out.push_str(&self.id);
        for (name, value) in &self.params {
            out.push(' ');
            out.push_str(name);
            out.push_str("=\"");
            escape_param_value(value, out);
            out.push('"');
        }
        out.push(']');
    }
}

/// Escape a structured-data `PARAM-VALUE` per RFC 5424 §6.3.3.
///
/// Inside a value, the characters `"`, `\`, and `]` MUST be escaped with a
/// preceding backslash; all other characters pass through unchanged.
fn escape_param_value(value: &str, out: &mut String) {
    for ch in value.chars() {
        if matches!(ch, '"' | '\\' | ']') {
            out.push('\\');
        }
        out.push(ch);
    }
}

/// A fully-formed RFC 5424 syslog message.
///
/// Built fluently from a [`Facility`] + [`Severity`]; every header field beyond
/// the PRI/VERSION is optional and renders as `NILVALUE` (`-`) when unset (or
/// set to the empty string). [`SyslogMessage::to_rfc5424`] produces the exact
/// wire string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyslogMessage {
    facility: Facility,
    severity: Severity,
    timestamp: Option<String>,
    hostname: Option<String>,
    app_name: Option<String>,
    proc_id: Option<String>,
    msg_id: Option<String>,
    structured_data: Vec<SdElement>,
    message: Option<String>,
}

impl SyslogMessage {
    /// Start a message with the given facility and severity. All other fields
    /// default to `NILVALUE`.
    #[must_use]
    pub const fn new(facility: Facility, severity: Severity) -> Self {
        Self {
            facility,
            severity,
            timestamp: None,
            hostname: None,
            app_name: None,
            proc_id: None,
            msg_id: None,
            structured_data: Vec::new(),
            message: None,
        }
    }

    /// Set the `TIMESTAMP` (RFC 3339 / `§6.2.3`). Empty => `NILVALUE`.
    #[must_use]
    pub fn timestamp(mut self, timestamp: impl Into<String>) -> Self {
        self.timestamp = Some(timestamp.into());
        self
    }

    /// Set the `HOSTNAME` (`§6.2.4`). Empty => `NILVALUE`.
    #[must_use]
    pub fn hostname(mut self, hostname: impl Into<String>) -> Self {
        self.hostname = Some(hostname.into());
        self
    }

    /// Set the `APP-NAME` (`§6.2.5`). Empty => `NILVALUE`.
    #[must_use]
    pub fn app_name(mut self, app_name: impl Into<String>) -> Self {
        self.app_name = Some(app_name.into());
        self
    }

    /// Set the `PROCID` (`§6.2.6`). Empty => `NILVALUE`.
    #[must_use]
    pub fn proc_id(mut self, proc_id: impl Into<String>) -> Self {
        self.proc_id = Some(proc_id.into());
        self
    }

    /// Set the `MSGID` (`§6.2.7`). Empty => `NILVALUE`.
    #[must_use]
    pub fn msg_id(mut self, msg_id: impl Into<String>) -> Self {
        self.msg_id = Some(msg_id.into());
        self
    }

    /// Set the `STRUCTURED-DATA` elements (`§6.3`). Empty => `NILVALUE`.
    #[must_use]
    pub fn structured_data(mut self, elements: Vec<SdElement>) -> Self {
        self.structured_data = elements;
        self
    }

    /// Set the free-form `MSG` (`§6.4`). When unset, no message is appended.
    #[must_use]
    pub fn message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    /// The computed RFC 5424 `PRIVAL` for this message's facility + severity.
    #[must_use]
    pub fn prival(&self) -> u16 {
        self.facility.prival(self.severity)
    }

    /// Render the complete RFC 5424 wire string.
    ///
    /// Layout: `<PRI>1 TIMESTAMP HOSTNAME APP-NAME PROCID MSGID SD [SP MSG]`,
    /// with every absent header field rendered as `NILVALUE` (`-`) and absent
    /// structured data rendered as a single `-`.
    #[must_use]
    pub fn to_rfc5424(&self) -> String {
        let mut out = String::new();
        // HEADER: PRI VERSION SP TIMESTAMP SP HOSTNAME SP APP-NAME SP PROCID SP MSGID
        // `write!` to a String is infallible, but avoid `unwrap`/`expect`; on
        // the impossible error path we simply leave the buffer as-is.
        let _ = write!(out, "<{}>1", self.prival());
        push_field(&mut out, self.timestamp.as_deref());
        push_field(&mut out, self.hostname.as_deref());
        push_field(&mut out, self.app_name.as_deref());
        push_field(&mut out, self.proc_id.as_deref());
        push_field(&mut out, self.msg_id.as_deref());

        // STRUCTURED-DATA: NILVALUE or one-or-more concatenated SD-ELEMENTs.
        out.push(' ');
        if self.structured_data.is_empty() {
            out.push_str(NILVALUE);
        } else {
            for element in &self.structured_data {
                element.render(&mut out);
            }
        }

        // MSG: optional; only emitted (with a leading space) when present.
        if let Some(message) = &self.message {
            out.push(' ');
            out.push_str(message);
        }
        out
    }
}

/// Append a space then either the field value or `NILVALUE` for an absent/empty
/// optional header field.
fn push_field(out: &mut String, field: Option<&str>) {
    out.push(' ');
    match field {
        Some(value) if !value.is_empty() => out.push_str(value),
        _ => out.push_str(NILVALUE),
    }
}

#[cfg(feature = "syslog")]
mod transport {
    //! Best-effort UDP/TCP delivery of formatted syslog messages.
    //!
    //! Gated behind the `syslog` Cargo feature. Uses `std::net` only — no native
    //! deps, `unsafe_code` stays forbidden. Sending is best-effort: errors are
    //! returned to the caller (the telemetry/control plane), never propagated
    //! onto the engine's data plane.
    use std::io::Write as _;
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};

    use super::SyslogMessage;
    use crate::error::{Result, TelemetryError};

    /// A UDP syslog sender (RFC 5426 transport).
    ///
    /// Each [`UdpSender::send`] emits one datagram. UDP is connectionless and
    /// lossy by design — appropriate for best-effort telemetry that must never
    /// back-pressure the engine.
    #[derive(Debug)]
    pub struct UdpSender {
        socket: UdpSocket,
    }

    impl UdpSender {
        /// Bind an ephemeral local UDP socket and connect it to `collector`.
        ///
        /// IPv6-first (operator directive): the ephemeral local socket is bound
        /// in the address family of the resolved `collector` — `[::]:0` for an
        /// IPv6 collector, `0.0.0.0:0` only when the collector resolves to IPv4
        /// — so an IPv6 syslog collector is reachable. A user-supplied IPv4
        /// collector still works; we never *default* to IPv4.
        ///
        /// # Errors
        ///
        /// Returns [`TelemetryError::Transport`] if the collector address cannot
        /// be resolved, the socket cannot be bound, or the connect fails.
        pub fn connect(collector: impl ToSocketAddrs) -> Result<Self> {
            // Resolve first so the ephemeral local bind matches the collector's
            // family (IPv6-first); `connect` is then a no-op family check.
            let target = collector
                .to_socket_addrs()
                .map_err(|e| TelemetryError::Transport(e.to_string()))?
                .next()
                .ok_or_else(|| {
                    TelemetryError::Transport("collector resolved to no address".to_owned())
                })?;
            let local: SocketAddr = if target.is_ipv6() {
                (Ipv6Addr::UNSPECIFIED, 0).into()
            } else {
                (Ipv4Addr::UNSPECIFIED, 0).into()
            };
            let socket =
                UdpSocket::bind(local).map_err(|e| TelemetryError::Transport(e.to_string()))?;
            socket
                .connect(target)
                .map_err(|e| TelemetryError::Transport(e.to_string()))?;
            Ok(Self { socket })
        }

        /// Send one syslog message as a single UDP datagram.
        ///
        /// # Errors
        ///
        /// Returns [`TelemetryError::Transport`] if the datagram cannot be sent.
        pub fn send(&self, message: &SyslogMessage) -> Result<()> {
            let wire = message.to_rfc5424();
            self.socket
                .send(wire.as_bytes())
                .map(|_| ())
                .map_err(|e| TelemetryError::Transport(e.to_string()))
        }
    }

    /// A TCP syslog sender using RFC 6587 octet-counting framing.
    ///
    /// Each message is prefixed with its byte length and a space
    /// (`MSG-LEN SP SYSLOG-MSG`), which is the unambiguous framing for stream
    /// transports.
    #[derive(Debug)]
    pub struct TcpSender {
        stream: TcpStream,
    }

    impl TcpSender {
        /// Open a TCP connection to `collector`.
        ///
        /// # Errors
        ///
        /// Returns [`TelemetryError::Transport`] if the connection fails.
        pub fn connect(collector: impl ToSocketAddrs) -> Result<Self> {
            let stream = TcpStream::connect(collector)
                .map_err(|e| TelemetryError::Transport(e.to_string()))?;
            Ok(Self { stream })
        }

        /// Send one octet-counted syslog frame.
        ///
        /// # Errors
        ///
        /// Returns [`TelemetryError::Transport`] if the frame cannot be written.
        pub fn send(&mut self, message: &SyslogMessage) -> Result<()> {
            let wire = message.to_rfc5424();
            let bytes = wire.as_bytes();
            let frame = format!("{} ", bytes.len());
            self.stream
                .write_all(frame.as_bytes())
                .and_then(|()| self.stream.write_all(bytes))
                .map_err(|e| TelemetryError::Transport(e.to_string()))
        }
    }
}

#[cfg(feature = "syslog")]
pub use transport::{TcpSender, UdpSender};
