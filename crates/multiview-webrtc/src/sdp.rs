//! Honest, IPv6-first SDP helpers (ADR-0048 §10, ADR-0042) and ICE candidate
//! priority ordering.
//!
//! On the native path the answer returned to a WHIP/WHEP client is **str0m's own
//! complete answer SDP** (correct BUNDLE / mid / rtcp-mux / fmtp). This module
//! owns the *fallback / fake-path* answer used by the pure (str0m-free) build and
//! by tests: it is structurally faithful — `c=IN IP6 ::` (never the
//! `c=IN IP4 0.0.0.0` placeholder the old scaffold emitted), with `a=mid` and
//! `a=rtcp-mux` — so the fake path emits the same dialect a browser sees.
//!
//! It also owns the **IPv6-first candidate priority ordering**: across families
//! IPv6 leads IPv4 at every tier; within a family host > server-reflexive > relay.
//! TURN relay candidates (the operator's NAT-traversal last resort) are *ordered*
//! lowest — never omitted.

/// The parameters needed to render an honest answer SDP for the fallback path.
#[derive(Debug, Clone)]
pub struct AnswerParams {
    /// ICE username fragment (`a=ice-ufrag`).
    pub ice_ufrag: String,
    /// ICE password (`a=ice-pwd`).
    pub ice_pwd: String,
    /// DTLS fingerprint algorithm (lower-case, e.g. `sha-256`).
    pub fingerprint_algorithm: String,
    /// DTLS fingerprint colon-hex value.
    pub fingerprint_value: String,
    /// The negotiated video payload type.
    pub video_payload_type: u8,
    /// The negotiated video codec encoding name (e.g. `H264`, `VP8`).
    pub video_codec: String,
    /// The negotiated audio payload type, if audio is carried.
    pub audio_payload_type: Option<u8>,
    /// The negotiated audio codec encoding name, if audio is carried.
    pub audio_codec: Option<String>,
}

impl AnswerParams {
    /// Render the answer SDP. Always IPv6-first (`c=IN IP6 ::`), BUNDLE,
    /// `a=mid`/`a=rtcp-mux`, `a=setup:passive` (the server is the DTLS server).
    #[must_use]
    pub fn build_sdp(&self) -> String {
        use std::fmt::Write as _;

        let mut sdp = String::new();
        // Session-level. IPv6-first per ADR-0042: origin + connection are IPv6.
        sdp.push_str("v=0\r\n");
        sdp.push_str("o=- 0 0 IN IP6 ::\r\n");
        sdp.push_str("s=-\r\n");
        sdp.push_str("t=0 0\r\n");
        // BUNDLE both media on one transport (mid 0 = video, 1 = audio).
        if self.audio_payload_type.is_some() {
            sdp.push_str("a=group:BUNDLE 0 1\r\n");
        } else {
            sdp.push_str("a=group:BUNDLE 0\r\n");
        }
        sdp.push_str("a=msid-semantic: WMS *\r\n");

        // Video m-line (mid 0). `write!` to a String is infallible.
        let _ = write!(
            sdp,
            "m=video 9 UDP/TLS/RTP/SAVPF {}\r\n",
            self.video_payload_type
        );
        sdp.push_str("c=IN IP6 ::\r\n");
        sdp.push_str("a=rtcp-mux\r\n");
        sdp.push_str("a=mid:0\r\n");
        sdp.push_str("a=sendonly\r\n");
        self.push_ice_dtls(&mut sdp);
        let _ = write!(
            sdp,
            "a=rtpmap:{} {}/90000\r\n",
            self.video_payload_type, self.video_codec
        );

        // Audio m-line (mid 1), if carried.
        if let (Some(pt), Some(codec)) = (self.audio_payload_type, self.audio_codec.as_ref()) {
            let _ = write!(sdp, "m=audio 9 UDP/TLS/RTP/SAVPF {pt}\r\n");
            sdp.push_str("c=IN IP6 ::\r\n");
            sdp.push_str("a=rtcp-mux\r\n");
            sdp.push_str("a=mid:1\r\n");
            sdp.push_str("a=sendonly\r\n");
            self.push_ice_dtls(&mut sdp);
            let _ = write!(sdp, "a=rtpmap:{pt} {codec}/48000/2\r\n");
        }
        sdp
    }

    fn push_ice_dtls(&self, sdp: &mut String) {
        use std::fmt::Write as _;

        let _ = write!(sdp, "a=ice-ufrag:{}\r\n", self.ice_ufrag);
        let _ = write!(sdp, "a=ice-pwd:{}\r\n", self.ice_pwd);
        let _ = write!(
            sdp,
            "a=fingerprint:{} {}\r\n",
            self.fingerprint_algorithm, self.fingerprint_value
        );
        // The answerer is the DTLS server (the client offered actpass / active).
        sdp.push_str("a=setup:passive\r\n");
    }
}

/// Rewrite an answer SDP's connection (`c=`) and origin (`o=`) address family to
/// match the family of the gathered ICE candidates — IPv6-first (ADR-0042).
///
/// str0m's own answer hardcodes the dummy `c=IN IP4 0.0.0.0` and
/// `o=… IN IP4 0.0.0.0` (RFC 8839 §4.3.2 makes the `c=` line a placeholder when
/// ICE is in use — the real addresses ride `a=candidate`). That is valid SDP, but
/// box-validation found IPv6-only peers reject the family mismatch, and our
/// IPv6-first posture wants the served answer honest. This transform inspects the
/// `a=candidate` lines: if **any** is IPv6 (the IPv6-first case), every `c=`/`o=`
/// connection address becomes `IN IP6 ::`; if the candidates are IPv4-only
/// (legacy), the IPv4 dummy is kept (never claim an IPv6 connection we cannot
/// offer). Only the network-type/address-type/address triple of `c=`/`o=` lines
/// is touched; candidate, mid, fingerprint, and every other line is preserved
/// verbatim. The dummy address stays unspecified (`::` / `0.0.0.0`) per RFC 8839.
#[must_use]
pub fn align_connection_family(sdp: &str) -> String {
    let any_ipv6_candidate = sdp.lines().any(is_ipv6_candidate_line);
    if !any_ipv6_candidate {
        // IPv4-only (or no) candidates: leave str0m's IPv4 dummy untouched.
        return sdp.to_owned();
    }
    // Preserve the original line terminators (str0m uses CRLF; tests may too).
    let mut out = String::with_capacity(sdp.len());
    for segment in sdp.split_inclusive('\n') {
        // Split the trailing CR/LF so we rewrite only the line body.
        let trimmed_len = segment.trim_end_matches(['\r', '\n']).len();
        let (body, terminator) = segment.split_at(trimmed_len);
        out.push_str(&rewrite_connection_line(body));
        out.push_str(terminator);
    }
    out
}

/// Rewrite one `c=`/`o=` line's address family to IPv6 (`IN IP6 ::`), leaving any
/// other line unchanged. `o=` keeps its first three tokens
/// (`o=<user> <sess-id> <sess-version>`) and rewrites the `<nettype> <addrtype>
/// <unicast-address>` triple; `c=` rewrites the whole `IN <addrtype> <addr>`.
fn rewrite_connection_line(line: &str) -> String {
    if line.starts_with("c=") {
        return "c=IN IP6 ::".to_owned();
    }
    if let Some(rest) = line.strip_prefix("o=") {
        // o=<username> <sess-id> <sess-version> <nettype> <addrtype> <addr>
        let tokens: Vec<&str> = rest.split(' ').collect();
        if let [user, sess_id, sess_version, _nettype, _addrtype, _addr, ..] = tokens.as_slice() {
            return format!("o={user} {sess_id} {sess_version} IN IP6 ::");
        }
    }
    line.to_owned()
}

/// Whether an SDP line is an `a=candidate` line whose connection address is IPv6.
/// The candidate connection-address is the 5th whitespace token after the
/// `candidate:` prefix: `candidate:<foundation> <component> <transport>
/// <priority> <addr> <port> typ …`.
fn is_ipv6_candidate_line(line: &str) -> bool {
    let trimmed = line.trim();
    let Some(rest) = trimmed.strip_prefix("a=candidate:") else {
        return false;
    };
    rest.split_whitespace()
        .nth(4)
        .and_then(|addr| addr.parse::<std::net::IpAddr>().ok())
        .is_some_and(|ip| ip.is_ipv6())
}

/// A class of ICE candidate, used for IPv6-first priority ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CandidateClass {
    /// IPv6 host candidate.
    HostV6,
    /// IPv4 host candidate.
    HostV4,
    /// IPv6 server-reflexive (STUN-discovered) candidate.
    ServerReflexiveV6,
    /// IPv4 server-reflexive candidate.
    ServerReflexiveV4,
    /// IPv6 TURN relay candidate.
    RelayV6,
    /// IPv4 TURN relay candidate.
    RelayV4,
}

impl CandidateClass {
    /// A sort key: lower sorts first (higher ICE priority). Across families IPv6
    /// leads IPv4 at each tier; within a tier host > srflx > relay.
    const fn rank(self) -> u8 {
        match self {
            Self::HostV6 => 0,
            Self::HostV4 => 1,
            Self::ServerReflexiveV6 => 2,
            Self::ServerReflexiveV4 => 3,
            Self::RelayV6 => 4,
            Self::RelayV4 => 5,
        }
    }
}

/// Order candidate classes IPv6-first (ADR-0042 / ADR-0048 §5). Every input class
/// is preserved — TURN relay candidates are ordered lowest, never dropped.
#[must_use]
pub fn candidate_priority_order(classes: &[CandidateClass]) -> Vec<CandidateClass> {
    let mut out = classes.to_vec();
    out.sort_by_key(|c| c.rank());
    out
}
