//! AMWA **NMOS IS-05** ("Device Connection Management") model.
//!
//! IS-05 is how a controller *connects* an NMOS sender to a receiver: it `PATCH`es
//! a **staged** endpoint with the transport parameters and an **activation**, then
//! the device promotes staged → **active** (broadcast-multiviewer brief §6/§8).
//! Multiview exposes this for its receivers (inputs) so a facility controller can
//! steer a 2110 flow into a tile, and for its senders (program/preview egress).
//!
//! This module is the **pure model** of the staged/active state and the
//! transport-parameter block, plus a minimal **SDP transport-file** parse/emit
//! for the ST 2110 fields a receiver needs to bind (multicast group, port,
//! source-filter). The live receiver bind (joining the multicast group on a real
//! NIC) is behind the off-by-default `nmos` feature; the model below is always
//! compiled and tested.
//!
//! ## Output-clock invariant (#1)
//!
//! Connecting a 2110 receiver only changes which essence Multiview *samples*; it
//! never paces the output. PTP (when present) disciplines a separate reference
//! clock — `out_pts = f(tick)` still holds. This module carries no timing; it
//! only describes the transport binding.
use serde::{Deserialize, Serialize};

/// A TAI instant as the NMOS `<seconds>:<nanoseconds>` pair, carried as integers.
///
/// IS-05 schedules activations against the **TAI** timeline (the same epoch PTP
/// disciplines). This model only *compares* instants — it never paces output —
/// so it needs no calendar/leap-second logic, just an ordered integer pair. The
/// nanosecond field is always `< 1_000_000_000`; arithmetic is checked so a
/// caller can never construct an out-of-range or overflowing instant silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaiTime {
    seconds: u64,
    nanoseconds: u32,
}

impl TaiTime {
    /// The number of nanoseconds in one whole second.
    const NANOS_PER_SEC: u32 = 1_000_000_000;

    /// Construct a [`TaiTime`] from whole seconds and a sub-second nanosecond
    /// remainder.
    ///
    /// A `nanoseconds` value at or beyond one whole second is **normalised** by
    /// carrying the whole-second part into `seconds` (saturating at [`u64::MAX`]
    /// rather than wrapping), so the stored remainder always stays `< 1e9`.
    #[must_use]
    pub fn new(seconds: u64, nanoseconds: u32) -> Self {
        let carry = u64::from(nanoseconds / Self::NANOS_PER_SEC);
        let rem = nanoseconds % Self::NANOS_PER_SEC;
        Self {
            seconds: seconds.saturating_add(carry),
            nanoseconds: rem,
        }
    }

    /// The whole-second component.
    #[must_use]
    pub const fn seconds(self) -> u64 {
        self.seconds
    }

    /// The sub-second nanosecond component (always `< 1_000_000_000`).
    #[must_use]
    pub const fn nanoseconds(self) -> u32 {
        self.nanoseconds
    }

    /// Add an offset instant to this one, returning [`None`] on overflow.
    ///
    /// Used to resolve an `ActivateScheduledRelative` offset against the instant
    /// the change was staged. Both the nanosecond carry and the second sum are
    /// checked, so the result is [`None`] rather than a wrapped instant.
    #[must_use]
    pub fn checked_add(self, offset: Self) -> Option<Self> {
        let nanos = self.nanoseconds + offset.nanoseconds;
        let carry = u64::from(nanos / Self::NANOS_PER_SEC);
        let rem = nanos % Self::NANOS_PER_SEC;
        let seconds = self
            .seconds
            .checked_add(offset.seconds)
            .and_then(|s| s.checked_add(carry))?;
        Some(Self {
            seconds,
            nanoseconds: rem,
        })
    }
}

impl PartialOrd for TaiTime {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TaiTime {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.seconds
            .cmp(&other.seconds)
            .then(self.nanoseconds.cmp(&other.nanoseconds))
    }
}

/// Parse a TAI instant in the NMOS `<seconds>[:<nanoseconds>]` form.
///
/// This is a focused, **total** parser (no panic, no full RFC clock dependency),
/// mirroring [`parse_sdp_transport`]'s discipline:
/// * a bare `<seconds>` is accepted with a zero nanosecond field;
/// * `<seconds>:<nanoseconds>` requires both fields to be present and non-empty;
/// * the nanosecond field must be `< 1_000_000_000` (a sub-second remainder);
/// * anything else — empty input, a third field, a sign, a decimal point, a
///   non-digit — yields [`None`].
#[must_use]
pub fn parse_tai(text: &str) -> Option<TaiTime> {
    let mut parts = text.split(':');
    let seconds_str = parts.next()?;
    let nanos_str = parts.next();
    // Reject a third colon-separated field (e.g. "1:2:3").
    if parts.next().is_some() {
        return None;
    }
    let seconds = seconds_str.parse::<u64>().ok()?;
    let nanoseconds = match nanos_str {
        None => 0,
        Some(nanos) => nanos.parse::<u32>().ok()?,
    };
    if nanoseconds >= TaiTime::NANOS_PER_SEC {
        return None;
    }
    Some(TaiTime {
        seconds,
        nanoseconds,
    })
}

/// The activation mode of an IS-05 connection change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ActivationMode {
    /// Apply the staged change immediately.
    ActivateImmediate,
    /// Apply at a scheduled absolute TAI time.
    ActivateScheduledAbsolute,
    /// Apply at a scheduled time relative to the request.
    ActivateScheduledRelative,
}

/// An IS-05 **activation** block: when a staged change takes effect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Activation {
    /// The activation mode, or [`None`] to stage without activating.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<ActivationMode>,
    /// The requested activation time (TAI `<seconds>:<nanoseconds>`), for a
    /// scheduled mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_time: Option<String>,
}

impl Activation {
    /// An immediate activation.
    #[must_use]
    pub fn immediate() -> Self {
        Self {
            mode: Some(ActivationMode::ActivateImmediate),
            requested_time: None,
        }
    }

    /// A staging-only request (no activation).
    #[must_use]
    pub fn stage_only() -> Self {
        Self {
            mode: None,
            requested_time: None,
        }
    }
}

/// One leg's RTP transport parameters (IS-05 supports up to two legs for
/// ST 2022-7 redundancy; this models one).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct TransportParams {
    /// The destination multicast group address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_ip: Option<String>,
    /// The destination UDP port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_port: Option<u16>,
    /// The source IP for an `IGMPv3` source-specific-multicast filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ip: Option<String>,
    /// Whether RTP reception is enabled on this leg.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtp_enabled: Option<bool>,
}

/// An IS-05 connection request `PATCH`ed to a `/staged` endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ConnectionRequest {
    /// Whether the sender/receiver is enabled once active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub master_enable: Option<bool>,
    /// The activation directive.
    pub activation: Activation,
    /// One transport-param block per leg (1 for single-path, 2 for ST 2022-7).
    #[serde(default)]
    pub transport_params: Vec<TransportParams>,
    /// For a receiver: the sender id to subscribe to (the connection target).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_id: Option<String>,
    /// For a receiver: the SDP transport file describing the sender's stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport_file: Option<String>,
}

/// The IS-05 connection state of one endpoint: its current active params and any
/// pending staged change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ConnectionState {
    /// The currently-active transport params (empty until first activated).
    #[serde(default)]
    pub active: Vec<TransportParams>,
    /// A staged-but-not-yet-active change, if one is pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged: Option<ConnectionRequest>,
    /// The TAI instant (`<seconds>:<nanoseconds>`) the pending change was staged
    /// at, captured so an `ActivateScheduledRelative` offset has a base to
    /// resolve against. [`None`] when nothing is staged, or when the stage was
    /// recorded without a clock stamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged_at: Option<String>,
    /// Whether the endpoint is master-enabled.
    #[serde(default)]
    pub master_enable: bool,
}

impl ConnectionState {
    /// Stage a connection request (does not activate it).
    ///
    /// No clock stamp is recorded, so a relative-scheduled request staged this
    /// way has no base to resolve its offset against (and [`activate_due`] will
    /// hold it pending). Use [`stage_at`] when a TAI stamp is available.
    ///
    /// [`activate_due`]: ConnectionState::activate_due
    /// [`stage_at`]: ConnectionState::stage_at
    pub fn stage(&mut self, request: ConnectionRequest) {
        self.staged = Some(request);
        self.staged_at = None;
    }

    /// Stage a connection request, recording the TAI instant it was staged at so
    /// an `ActivateScheduledRelative` offset can be resolved against it.
    pub fn stage_at(&mut self, request: ConnectionRequest, now: TaiTime) {
        self.staged = Some(request);
        self.staged_at = Some(format!("{}:{}", now.seconds(), now.nanoseconds()));
    }

    /// Resolve the absolute TAI instant the staged request becomes due, if any.
    ///
    /// * `ActivateImmediate` is due at the epoch (`0:0`) — i.e. always.
    /// * `ActivateScheduledAbsolute` is due at its parsed `requested_time`.
    /// * `ActivateScheduledRelative` is due at `staged_at + requested_time`,
    ///   and is unresolvable (returns [`None`]) without a recorded `staged_at`.
    /// * A staging-only request (no mode) is never due ([`None`]).
    fn due_at(&self) -> Option<TaiTime> {
        let request = self.staged.as_ref()?;
        match request.activation.mode? {
            ActivationMode::ActivateImmediate => Some(TaiTime::new(0, 0)),
            ActivationMode::ActivateScheduledAbsolute => {
                parse_tai(request.activation.requested_time.as_deref()?)
            }
            ActivationMode::ActivateScheduledRelative => {
                let offset = parse_tai(request.activation.requested_time.as_deref()?)?;
                let base = parse_tai(self.staged_at.as_deref()?)?;
                base.checked_add(offset)
            }
        }
    }

    /// Promote the staged request to active **iff** its activation is due at the
    /// given clock instant `now`, applying its transport params and
    /// master-enable. Returns whether a change was activated.
    ///
    /// `now` is supplied by the control plane's clock seam — never an input PTS
    /// (invariant #1): activating a receiver only changes which essence is
    /// sampled, it never paces output. A request that is not yet due, is
    /// staging-only, or whose schedule cannot be resolved is left pending.
    pub fn activate_due(&mut self, now: TaiTime) -> bool {
        let Some(due) = self.due_at() else {
            return false;
        };
        if now < due {
            return false;
        }
        let Some(request) = self.staged.take() else {
            return false;
        };
        self.staged_at = None;
        if let Some(enable) = request.master_enable {
            self.master_enable = enable;
        }
        self.active = request.transport_params;
        true
    }

    /// Promote the staged request to active **iff** it carries an immediate
    /// activation, applying its transport params and master-enable. Returns
    /// whether a change was activated.
    ///
    /// A thin wrapper over [`activate_due`](ConnectionState::activate_due): an
    /// immediate activation is due at the epoch, so any clock instant promotes
    /// it, while a scheduled or staging-only request never matches the epoch and
    /// is left pending.
    pub fn activate_if_immediate(&mut self) -> bool {
        let immediate = matches!(
            self.staged.as_ref().and_then(|r| r.activation.mode),
            Some(ActivationMode::ActivateImmediate)
        );
        if !immediate {
            return false;
        }
        self.activate_due(TaiTime::new(0, 0))
    }
}

/// A minimal **SDP transport file** parse for the ST 2110 fields a receiver
/// needs to bind: the `c=` connection (multicast group), the `m=video` port, and
/// the `a=source-filter` source IP.
///
/// This is intentionally a focused, total parser over the lines IS-05/2110 care
/// about — not a full RFC 8866 SDP implementation. Unknown lines are ignored.
#[must_use]
pub fn parse_sdp_transport(sdp: &str) -> TransportParams {
    let mut params = TransportParams {
        destination_ip: None,
        destination_port: None,
        source_ip: None,
        rtp_enabled: Some(true),
    };
    for line in sdp.lines() {
        let line = line.trim();
        if let Some(conn) = line.strip_prefix("c=") {
            // c=IN IP4 <group>/<ttl>
            if let Some(addr) = conn.split_whitespace().nth(2) {
                let group = addr.split('/').next().unwrap_or(addr);
                params.destination_ip = Some(group.to_owned());
            }
        } else if let Some(media) = line.strip_prefix("m=video ") {
            // m=video <port> RTP/AVP <fmt>
            if let Some(port_str) = media.split_whitespace().next() {
                if let Ok(port) = port_str.parse::<u16>() {
                    params.destination_port = Some(port);
                }
            }
        } else if let Some(filter) = line.strip_prefix("a=source-filter:") {
            // a=source-filter: incl IN IP4 <dest> <source>
            if let Some(source) = filter.split_whitespace().last() {
                params.source_ip = Some(source.to_owned());
            }
        }
    }
    params
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{
        parse_sdp_transport, parse_tai, Activation, ActivationMode, ConnectionRequest,
        ConnectionState, TaiTime, TransportParams,
    };

    fn request(immediate: bool) -> ConnectionRequest {
        ConnectionRequest {
            master_enable: Some(true),
            activation: if immediate {
                Activation::immediate()
            } else {
                Activation::stage_only()
            },
            transport_params: vec![TransportParams {
                destination_ip: Some("239.0.0.1".to_owned()),
                destination_port: Some(5004),
                source_ip: Some("192.0.2.10".to_owned()),
                rtp_enabled: Some(true),
            }],
            sender_id: Some("snd-1".to_owned()),
            transport_file: None,
        }
    }

    #[test]
    fn connection_request_round_trips_through_json() {
        let req = request(true);
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["activation"]["mode"], "activate_immediate");
        assert_eq!(json["transport_params"][0]["destination_port"], 5004);
        let back: ConnectionRequest = serde_json::from_value(json).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn staging_then_immediate_activation_applies_params() {
        let mut state = ConnectionState::default();
        assert!(state.active.is_empty());
        assert!(!state.master_enable);

        state.stage(request(true));
        // Staged, not yet active.
        assert!(state.staged.is_some());
        assert!(state.active.is_empty());

        let activated = state.activate_if_immediate();
        assert!(activated);
        assert!(state.staged.is_none());
        assert_eq!(state.active.len(), 1);
        assert_eq!(state.active[0].destination_port, Some(5004));
        assert!(state.master_enable);
    }

    #[test]
    fn staging_only_request_does_not_activate() {
        let mut state = ConnectionState::default();
        state.stage(request(false));
        let activated = state.activate_if_immediate();
        assert!(!activated, "a stage-only request must not auto-activate");
        assert!(state.staged.is_some(), "it stays staged");
        assert!(state.active.is_empty());
    }

    #[test]
    fn scheduled_activation_is_not_treated_as_immediate() {
        let mut state = ConnectionState::default();
        let mut req = request(true);
        req.activation = Activation {
            mode: Some(ActivationMode::ActivateScheduledAbsolute),
            requested_time: Some("1700001000:0".to_owned()),
        };
        state.stage(req);
        assert!(!state.activate_if_immediate());
        assert!(state.staged.is_some());
    }

    fn scheduled_request(mode: ActivationMode, requested_time: &str) -> ConnectionRequest {
        let mut req = request(true);
        req.activation = Activation {
            mode: Some(mode),
            requested_time: Some(requested_time.to_owned()),
        };
        req
    }

    #[test]
    fn parse_tai_reads_seconds_and_nanos() {
        assert_eq!(
            parse_tai("1700001000:250"),
            Some(TaiTime::new(1_700_001_000, 250))
        );
        // Bare seconds (no colon) defaults the nanosecond field to zero.
        assert_eq!(parse_tai("42"), Some(TaiTime::new(42, 0)));
        // A trailing-colon form is the canonical NMOS "<seconds>:<nanoseconds>".
        assert_eq!(parse_tai("0:0"), Some(TaiTime::new(0, 0)));
    }

    #[test]
    fn parse_tai_rejects_garbage_without_panicking() {
        assert_eq!(parse_tai(""), None);
        assert_eq!(parse_tai("notatime"), None);
        assert_eq!(parse_tai("1700001000:"), None);
        assert_eq!(parse_tai(":5"), None);
        assert_eq!(parse_tai("1.5:0"), None);
        assert_eq!(parse_tai("-1:0"), None);
        // A nanosecond field at or beyond one whole second is invalid.
        assert_eq!(parse_tai("1:1000000000"), None);
        // More than two colon-separated fields is not the NMOS form.
        assert_eq!(parse_tai("1:2:3"), None);
    }

    #[test]
    fn tai_time_orders_by_seconds_then_nanos() {
        assert!(TaiTime::new(10, 0) < TaiTime::new(10, 1));
        assert!(TaiTime::new(10, 999_999_999) < TaiTime::new(11, 0));
        assert_eq!(TaiTime::new(10, 5), TaiTime::new(10, 5));
    }

    #[test]
    fn immediate_activation_is_always_due() {
        let mut state = ConnectionState::default();
        state.stage(request(true));
        // The clock instant is irrelevant for an immediate activation.
        let activated = state.activate_due(TaiTime::new(0, 0));
        assert!(activated);
        assert!(state.staged.is_none());
        assert_eq!(state.active.len(), 1);
        assert!(state.master_enable);
    }

    #[test]
    fn staging_only_request_is_never_due() {
        let mut state = ConnectionState::default();
        state.stage(request(false));
        assert!(!state.activate_due(TaiTime::new(2_000_000_000, 0)));
        assert!(state.staged.is_some());
        assert!(state.active.is_empty());
    }

    #[test]
    fn scheduled_absolute_does_not_activate_before_due() {
        let mut state = ConnectionState::default();
        state.stage(scheduled_request(
            ActivationMode::ActivateScheduledAbsolute,
            "1700001000:0",
        ));
        // One nanosecond before the requested time: not yet due.
        let activated = state.activate_due(TaiTime::new(1_700_000_999, 999_999_999));
        assert!(!activated, "absolute schedule must not fire early");
        assert!(state.staged.is_some());
        assert!(state.active.is_empty());
    }

    #[test]
    fn scheduled_absolute_activates_at_or_after_requested_time() {
        // Exactly at the requested instant.
        let mut at = ConnectionState::default();
        at.stage(scheduled_request(
            ActivationMode::ActivateScheduledAbsolute,
            "1700001000:0",
        ));
        assert!(at.activate_due(TaiTime::new(1_700_001_000, 0)));
        assert!(at.staged.is_none());
        assert_eq!(at.active.len(), 1);

        // Strictly after the requested instant.
        let mut after = ConnectionState::default();
        after.stage(scheduled_request(
            ActivationMode::ActivateScheduledAbsolute,
            "1700001000:0",
        ));
        assert!(after.activate_due(TaiTime::new(1_700_001_005, 1)));
        assert!(after.staged.is_none());
    }

    #[test]
    fn scheduled_relative_resolves_offset_against_staged_at() {
        let mut state = ConnectionState::default();
        // Stage at TAI 1000:0; a 30-second relative offset → due at 1030:0.
        state.stage_at(
            scheduled_request(ActivationMode::ActivateScheduledRelative, "30:0"),
            TaiTime::new(1000, 0),
        );
        assert_eq!(state.staged_at.as_deref(), Some("1000:0"));
        assert!(
            !state.activate_due(TaiTime::new(1029, 999_999_999)),
            "relative schedule must not fire before staged_at + offset"
        );
        assert!(state.staged.is_some());
        assert!(state.activate_due(TaiTime::new(1030, 0)));
        assert!(state.staged.is_none());
        assert_eq!(state.active.len(), 1);
    }

    #[test]
    fn scheduled_relative_without_staged_stamp_never_fires() {
        let mut state = ConnectionState::default();
        // `stage` records no clock stamp, so the relative offset has no base to
        // resolve against and the change can never become due.
        state.stage(scheduled_request(
            ActivationMode::ActivateScheduledRelative,
            "0:0",
        ));
        assert!(state.staged_at.is_none());
        assert!(!state.activate_due(TaiTime::new(9_999_999_999, 0)));
        assert!(state.staged.is_some());
    }

    #[test]
    fn stage_records_the_staged_at_stamp() {
        let mut state = ConnectionState::default();
        state.stage_at(request(false), TaiTime::new(500, 7));
        assert_eq!(state.staged_at.as_deref(), Some("500:7"));
    }

    #[test]
    fn sdp_transport_parse_extracts_2110_binding_fields() {
        let sdp = "\
v=0
o=- 1443716955 1443716955 IN IP4 192.0.2.10
s=Multiview Program
t=0 0
m=video 5004 RTP/AVP 96
c=IN IP4 239.10.20.30/64
a=source-filter: incl IN IP4 239.10.20.30 192.0.2.10
a=rtpmap:96 raw/90000
";
        let params = parse_sdp_transport(sdp);
        assert_eq!(params.destination_ip.as_deref(), Some("239.10.20.30"));
        assert_eq!(params.destination_port, Some(5004));
        assert_eq!(params.source_ip.as_deref(), Some("192.0.2.10"));
        assert_eq!(params.rtp_enabled, Some(true));
    }

    #[test]
    fn sdp_transport_parse_tolerates_missing_fields() {
        // An SDP with no media line yields no port but does not panic.
        let params = parse_sdp_transport("v=0\nc=IN IP4 239.1.1.1\n");
        assert_eq!(params.destination_ip.as_deref(), Some("239.1.1.1"));
        assert_eq!(params.destination_port, None);
        assert_eq!(params.source_ip, None);
    }
}
