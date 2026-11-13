//! Settings and limits, all explicit and bounded.

use std::time::Duration;

/// h3 connection settings (design note section 5). h3 0.0.8's defaults are
/// all off or unbounded, so every value is set explicitly by
/// [`H3Settings::builder`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // reason: each bool is one independent h3 SETTINGS flag, mirroring h3's builder.
pub struct H3Settings {
    /// `SETTINGS_ENABLE_CONNECT_PROTOCOL` (RFC 9220 section 3).
    pub enable_extended_connect: bool,
    /// `H3_DATAGRAM` (RFC 9297 section 2.1.1).
    pub enable_datagram: bool,
    /// `SETTINGS_ENABLE_WEBTRANSPORT` (draft-ietf-webtrans-http3, draft02 ids).
    pub enable_webtransport: bool,
    /// `SETTINGS_WEBTRANSPORT_MAX_SESSIONS`: one session per connection.
    pub max_webtransport_sessions: u64,
    /// `SETTINGS_MAX_FIELD_SECTION_SIZE`; the default in h3 is `VarInt::MAX`
    /// and its QPACK decoder allocates each field before checking the total.
    pub max_field_section_size: u64,
    /// Send reserved (grease) settings, frames and streams.
    pub send_grease: bool,
}

impl Default for H3Settings {
    fn default() -> Self {
        Self {
            enable_extended_connect: true,
            enable_datagram: true,
            enable_webtransport: true,
            max_webtransport_sessions: 1,
            max_field_section_size: 16_384,
            send_grease: true,
        }
    }
}

impl H3Settings {
    /// The `h3::server::Builder` carrying exactly these settings.
    #[must_use]
    pub fn builder(&self) -> h3::server::Builder {
        let mut builder = h3::server::builder();
        builder
            .enable_extended_connect(self.enable_extended_connect)
            .enable_datagram(self.enable_datagram)
            .enable_webtransport(self.enable_webtransport)
            .max_webtransport_sessions(self.max_webtransport_sessions)
            .max_field_section_size(self.max_field_section_size)
            .send_grease(self.send_grease);
        builder
    }
}

/// QUIC transport limits for the endpoint. Every value is explicit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuicLimits {
    /// Peer-initiated bidirectional streams in flight (requests + WT bidi).
    pub max_concurrent_bidi_streams: u32,
    /// Peer-initiated unidirectional streams in flight (h3 control/QPACK +
    /// WT uni).
    pub max_concurrent_uni_streams: u32,
    /// Per-stream receive window (flow control credit per stream).
    pub stream_receive_window: u32,
    /// Connection-wide receive window.
    pub receive_window: u32,
    /// Connection-wide send buffer.
    pub send_window: u64,
    /// Datagram receive buffer (noq drops the oldest datagram when full).
    pub datagram_receive_buffer_size: usize,
    /// Datagram send buffer (noq drops the oldest queued datagram when full).
    pub datagram_send_buffer_size: usize,
    /// Idle timeout.
    pub max_idle_timeout: Duration,
    /// Keep-alive interval (below the idle timeout).
    pub keep_alive_interval: Duration,
}

impl Default for QuicLimits {
    fn default() -> Self {
        Self {
            max_concurrent_bidi_streams: 128,
            max_concurrent_uni_streams: 128,
            stream_receive_window: 1024 * 1024,
            receive_window: 16 * 1024 * 1024,
            send_window: 16 * 1024 * 1024,
            datagram_receive_buffer_size: 256 * 1024,
            datagram_send_buffer_size: 256 * 1024,
            max_idle_timeout: Duration::from_secs(30),
            keep_alive_interval: Duration::from_secs(10),
        }
    }
}

impl QuicLimits {
    /// The noq transport config for these limits.
    ///
    /// # Errors
    ///
    /// The idle timeout is out of range for QUIC.
    pub fn transport_config(&self) -> Result<noq::TransportConfig, crate::Error> {
        let mut transport = noq::TransportConfig::default();
        let idle = noq::IdleTimeout::try_from(self.max_idle_timeout)
            .map_err(|_| crate::Error::Config("max_idle_timeout out of range"))?;
        transport
            .max_concurrent_bidi_streams(noq::VarInt::from_u32(self.max_concurrent_bidi_streams))
            .max_concurrent_uni_streams(noq::VarInt::from_u32(self.max_concurrent_uni_streams))
            .stream_receive_window(noq::VarInt::from_u32(self.stream_receive_window))
            .receive_window(noq::VarInt::from_u32(self.receive_window))
            .send_window(self.send_window)
            .datagram_receive_buffer_size(Some(self.datagram_receive_buffer_size))
            .datagram_send_buffer_size(self.datagram_send_buffer_size)
            .max_idle_timeout(Some(idle))
            .keep_alive_interval(Some(self.keep_alive_interval));
        Ok(transport)
    }
}

/// Server limits. Every queue or allocation a peer can drive is listed in
/// the crate docs with the field here that bounds it.
#[derive(Clone, Debug)]
pub struct H3Limits {
    /// h3 settings.
    pub settings: H3Settings,
    /// QUIC transport limits.
    pub quic: QuicLimits,
    /// Requests in flight per connection (a semaphore; a request stream
    /// beyond it is reset with `H3_REQUEST_REJECTED`, never queued).
    pub max_concurrent_requests: usize,
    /// From stream accept until the request HEADERS frame is complete.
    pub header_deadline: Duration,
    /// Path prefix for WebTransport sessions (`/moq/`; exactly `/moq` too).
    pub wt_prefix: &'static str,
    /// Incoming WT bidirectional streams queued for the handler.
    pub wt_bidi_queue: usize,
    /// Incoming WT unidirectional streams queued for the handler.
    pub wt_uni_queue: usize,
    /// WT streams that arrived before their session was accepted.
    pub wt_early_streams: usize,
    /// Connections in flight on the endpoint.
    pub max_connections: usize,
    /// Connections in flight per source (IPv4 address or IPv6 /64).
    pub max_connections_per_source: usize,
    /// TLS/QUIC handshake deadline.
    pub handshake_timeout: Duration,
    /// How long `shutdown` waits for connections to drain.
    pub shutdown_timeout: Duration,
}

impl Default for H3Limits {
    fn default() -> Self {
        Self {
            settings: H3Settings::default(),
            quic: QuicLimits::default(),
            max_concurrent_requests: 128,
            header_deadline: Duration::from_secs(10),
            wt_prefix: "/moq/",
            wt_bidi_queue: 64,
            wt_uni_queue: 64,
            wt_early_streams: 16,
            max_connections: 1024,
            max_connections_per_source: 64,
            handshake_timeout: Duration::from_secs(10),
            shutdown_timeout: Duration::from_secs(2),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_defaults_are_the_design_values() {
        let s = H3Settings::default();
        assert!(s.enable_extended_connect);
        assert!(s.enable_datagram);
        assert!(s.enable_webtransport);
        assert_eq!(s.max_webtransport_sessions, 1);
        assert_eq!(s.max_field_section_size, 16_384);
        assert!(s.send_grease);
    }

    #[test]
    fn limits_defaults() {
        let l = H3Limits::default();
        assert_eq!(l.max_concurrent_requests, 128);
        assert_eq!(l.header_deadline, Duration::from_secs(10));
        assert_eq!(l.max_connections, 1024);
        assert_eq!(l.max_connections_per_source, 64);
        assert_eq!(l.wt_prefix, "/moq/");
        let q = l.quic;
        assert_eq!(q.max_concurrent_bidi_streams, 128);
        assert_eq!(q.max_concurrent_uni_streams, 128);
        assert_eq!(q.stream_receive_window, 1 << 20);
        assert_eq!(q.receive_window, 16 << 20);
        assert_eq!(q.send_window, 16 << 20);
        assert_eq!(q.datagram_receive_buffer_size, 256 << 10);
        assert_eq!(q.datagram_send_buffer_size, 256 << 10);
        assert_eq!(q.max_idle_timeout, Duration::from_secs(30));
        assert_eq!(q.keep_alive_interval, Duration::from_secs(10));
        assert!(q.transport_config().is_ok());
    }
}
