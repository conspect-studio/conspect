//! The typed CASTV2 namespace messages (DEV-D2, ADR-M011) — pure JSON
//! payload construction and tolerant decoding, no socket, no protobuf.
//!
//! A CASTV2 conversation multiplexes labelled JSON protocols ("namespaces")
//! over one TLS channel; the four the session actor speaks are the
//! connection, heartbeat, receiver, and media namespaces. This module models
//! one message as a [`CastFrame`] (namespace + routing ids + the JSON payload
//! string) so **all** driver logic above the channel seam is socket-free
//! unit-testable; the wire form (4-byte big-endian length prefix + protobuf
//! `CastMessage`) lives in [`super::net`] behind the off-by-default `cast`
//! feature.
//!
//! Message shapes are implemented from the BSD-3-Clause Chromium Open Screen
//! protocol sources and community documentation — no proprietary SDK.
//! Decoding is **tolerant**: an unknown message type or a malformed payload
//! is [`InboundMessage::Unknown`], never an error (the receiver platform may
//! grow new messages at any time — the ADR-M011 proprietary-protocol drift
//! posture).
//!
//! Google Cast and Chromecast are trademarks of Google LLC; Multiview is not
//! certified by, endorsed by, or affiliated with Google.

use serde::{Deserialize, Serialize};

use super::media::CastMediaTarget;

/// The connection namespace (virtual CONNECT/CLOSE per destination).
pub const NS_CONNECTION: &str = "urn:x-cast:com.google.cast.tp.connection";
/// The heartbeat namespace (PING/PONG).
pub const NS_HEARTBEAT: &str = "urn:x-cast:com.google.cast.tp.heartbeat";
/// The receiver namespace (`LAUNCH` / `RECEIVER_STATUS` / `SET_VOLUME` / `STOP`).
pub const NS_RECEIVER: &str = "urn:x-cast:com.google.cast.receiver";
/// The media namespace (`LOAD` / `MEDIA_STATUS`).
pub const NS_MEDIA: &str = "urn:x-cast:com.google.cast.media";

/// Google's hosted **Default Media Receiver** app id (ADR-M011): requires no
/// developer registration, no public URL, and no cloud account.
pub const DEFAULT_MEDIA_RECEIVER_APP_ID: &str = "CC1AD845";

/// Our sender id on the channel (the conventional first-sender id).
pub const SENDER_ID: &str = "sender-0";
/// The platform receiver's id (the pre-app destination).
pub const PLATFORM_RECEIVER_ID: &str = "receiver-0";

/// The HLS content type a LOAD declares for an HLS rendition.
const HLS_CONTENT_TYPE: &str = "application/vnd.apple.mpegurl";

/// One CASTV2 message above the wire codec: the namespace, the routing ids,
/// and the JSON payload **string** exactly as it rides the channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CastFrame {
    /// The labelled protocol this payload belongs to (one of the `NS_*`).
    pub namespace: String,
    /// The sending endpoint id (`sender-0` for our frames).
    pub source: String,
    /// The destination endpoint id (`receiver-0` or an app transport id).
    pub destination: String,
    /// The UTF-8 JSON payload.
    pub payload: String,
}

/// Build an outbound frame from `SENDER_ID` to `destination`.
fn frame(namespace: &str, destination: &str, payload: &serde_json::Value) -> CastFrame {
    CastFrame {
        namespace: namespace.to_owned(),
        source: SENDER_ID.to_owned(),
        destination: destination.to_owned(),
        payload: payload.to_string(),
    }
}

/// The virtual CONNECT that opens a conversation with `destination` (the
/// platform receiver, or a launched app's transport id).
#[must_use]
pub fn connect_frame(destination: &str) -> CastFrame {
    frame(
        NS_CONNECTION,
        destination,
        &serde_json::json!({ "type": "CONNECT" }),
    )
}

/// The virtual CLOSE for `destination` (best-effort teardown courtesy).
#[must_use]
pub fn close_frame(destination: &str) -> CastFrame {
    frame(
        NS_CONNECTION,
        destination,
        &serde_json::json!({ "type": "CLOSE" }),
    )
}

/// A heartbeat PING (sent every [`super::session::CastSessionConfig::ping_interval`]).
#[must_use]
pub fn ping_frame() -> CastFrame {
    frame(
        NS_HEARTBEAT,
        PLATFORM_RECEIVER_ID,
        &serde_json::json!({ "type": "PING" }),
    )
}

/// A heartbeat PONG (the answer to the device's own PING).
#[must_use]
pub fn pong_frame() -> CastFrame {
    frame(
        NS_HEARTBEAT,
        PLATFORM_RECEIVER_ID,
        &serde_json::json!({ "type": "PONG" }),
    )
}

/// LAUNCH `app_id` on the platform receiver.
#[must_use]
pub fn launch_frame(request_id: u32, app_id: &str) -> CastFrame {
    frame(
        NS_RECEIVER,
        PLATFORM_RECEIVER_ID,
        &serde_json::json!({ "type": "LAUNCH", "requestId": request_id, "appId": app_id }),
    )
}

/// `GET_STATUS` on the platform receiver (drives a `RECEIVER_STATUS` answer).
#[must_use]
pub fn get_status_frame(request_id: u32) -> CastFrame {
    frame(
        NS_RECEIVER,
        PLATFORM_RECEIVER_ID,
        &serde_json::json!({ "type": "GET_STATUS", "requestId": request_id }),
    )
}

/// LOAD the HLS rendition on the launched app's `transport_id` (ADR-M011):
/// `contentId` = the device-reachable rendition URL, HLS content type,
/// `streamType: LIVE`, plus the explicit `hlsVideoSegmentFormat` — receivers
/// assume MPEG-TS unless told otherwise, so the format is always signalled
/// (`fmp4` for a CMAF rendition, `mpeg2_ts` otherwise).
#[must_use]
pub fn load_frame(request_id: u32, transport_id: &str, media: &CastMediaTarget) -> CastFrame {
    frame(
        NS_MEDIA,
        transport_id,
        &serde_json::json!({
            "type": "LOAD",
            "requestId": request_id,
            "autoplay": true,
            "media": {
                "contentId": media.url,
                "contentType": HLS_CONTENT_TYPE,
                "streamType": "LIVE",
                "hlsVideoSegmentFormat": media.format.wire_token(),
            }
        }),
    )
}

/// `SET_VOLUME` on the platform receiver: `percent` (0–100) maps to the
/// protocol's unit `level` (0.0–1.0). Carried as an integer percent end to
/// end so the control-channel command stays `Eq`-comparable (no float in
/// [`crate::devices::PollerControl`]).
#[must_use]
pub fn set_volume_frame(request_id: u32, percent: u8) -> CastFrame {
    let level = f64::from(percent.min(100)) / 100.0;
    frame(
        NS_RECEIVER,
        PLATFORM_RECEIVER_ID,
        &serde_json::json!({
            "type": "SET_VOLUME",
            "requestId": request_id,
            "volume": { "level": level }
        }),
    )
}

/// STOP the receiver app running as `session_id` (the receiver-namespace app
/// stop — this is what actually clears the TV, the Default Media Receiver
/// keeps playing when a sender merely disconnects).
#[must_use]
pub fn stop_frame(request_id: u32, session_id: &str) -> CastFrame {
    frame(
        NS_RECEIVER,
        PLATFORM_RECEIVER_ID,
        &serde_json::json!({
            "type": "STOP",
            "requestId": request_id,
            "sessionId": session_id
        }),
    )
}

/// One application row in a `RECEIVER_STATUS`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReceiverApplication {
    /// The receiver app id (`CC1AD845` for the Default Media Receiver).
    #[serde(rename = "appId")]
    pub app_id: String,
    /// The receiver-side session id (what a STOP names).
    #[serde(rename = "sessionId")]
    pub session_id: String,
    /// The transport id media-namespace messages address.
    #[serde(rename = "transportId")]
    pub transport_id: String,
    /// Whether this row is the receiver's own idle screen (the backdrop real
    /// hardware launches when no sender app runs) rather than a sender's app
    /// — absent on the wire means `false`. The session actor's "nothing else
    /// running" checks skip idle-screen rows.
    #[serde(rename = "isIdleScreen", default)]
    pub is_idle_screen: bool,
}

/// The decoded application set of a `RECEIVER_STATUS`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReceiverStatusInfo {
    /// The applications currently running on the receiver (empty when idle —
    /// or when another sender stopped ours: the preemption signal).
    pub applications: Vec<ReceiverApplication>,
}

/// The receiver player state carried in a `MEDIA_STATUS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum PlayerState {
    /// Media playback is running.
    Playing,
    /// The player is buffering.
    Buffering,
    /// Playback is paused (e.g. by the TV remote).
    Paused,
    /// No media is loaded / playback ended (`idleReason` says why) — the
    /// session supervisor re-LOADs on this state (ADR-M011).
    Idle,
}

impl PlayerState {
    /// The lowercase token published as the conflated `device.status` `mode`
    /// field (the operator-facing player state).
    #[must_use]
    pub const fn mode_token(self) -> &'static str {
        match self {
            Self::Playing => "playing",
            Self::Buffering => "buffering",
            Self::Paused => "paused",
            Self::Idle => "idle",
        }
    }
}

/// One status row of a `MEDIA_STATUS`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaStatusEntry {
    /// The media session id (used to address further media commands).
    pub media_session_id: Option<i64>,
    /// The player state this row reports.
    pub player_state: PlayerState,
    /// Why the player is IDLE (`FINISHED`/`ERROR`/`CANCELLED`/`INTERRUPTED`),
    /// when it is.
    pub idle_reason: Option<String>,
}

/// A decoded inbound message, namespace-dispatched and tolerant: anything
/// unrecognised (a future message type, a malformed payload, an unknown
/// player state) is [`InboundMessage::Unknown`], never an error.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum InboundMessage {
    /// A heartbeat PONG (answers our PING).
    Pong,
    /// The device's own heartbeat PING (we answer with a PONG).
    Ping,
    /// The device closed the virtual connection (teardown / app gone).
    CloseConnection,
    /// A receiver status snapshot (applications present/absent).
    ReceiverStatus(ReceiverStatusInfo),
    /// A media status report (player states, idle reasons).
    MediaStatus(Vec<MediaStatusEntry>),
    /// The LAUNCH was refused.
    LaunchError {
        /// The receiver's reason token, if it gave one.
        reason: Option<String>,
    },
    /// The receiver rejected or tore down a media LOAD (`LOAD_FAILED`,
    /// `LOAD_CANCELLED`, or a media-namespace `INVALID_REQUEST`). Typed so
    /// the session actor can degrade and schedule the bounded re-LOAD — as
    /// `Unknown` it would be ignored and a receiver that answers PINGs but
    /// rejects every LOAD would sit "loading" forever.
    LoadError {
        /// The wire error type token (`LOAD_FAILED` / `LOAD_CANCELLED` /
        /// `INVALID_REQUEST`).
        kind: String,
        /// The receiver's reason token, when it gave one (`INVALID_REQUEST`
        /// carries e.g. `INVALID_COMMAND`).
        reason: Option<String>,
    },
    /// Anything this driver does not model — tolerated and ignored.
    Unknown,
}

/// Decode one inbound frame into its typed message (tolerant; see
/// [`InboundMessage`]).
#[must_use]
pub fn decode(frame: &CastFrame) -> InboundMessage {
    let Ok(payload) = serde_json::from_str::<serde_json::Value>(&frame.payload) else {
        return InboundMessage::Unknown;
    };
    let kind = payload.get("type").and_then(serde_json::Value::as_str);
    match (frame.namespace.as_str(), kind) {
        (NS_HEARTBEAT, Some("PONG")) => InboundMessage::Pong,
        (NS_HEARTBEAT, Some("PING")) => InboundMessage::Ping,
        (NS_CONNECTION, Some("CLOSE")) => InboundMessage::CloseConnection,
        (NS_RECEIVER, Some("RECEIVER_STATUS")) => {
            InboundMessage::ReceiverStatus(decode_receiver_status(&payload))
        }
        (NS_RECEIVER, Some("LAUNCH_ERROR")) => InboundMessage::LaunchError {
            reason: payload
                .get("reason")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
        },
        (NS_MEDIA, Some("MEDIA_STATUS")) => {
            InboundMessage::MediaStatus(decode_media_status(&payload))
        }
        (NS_MEDIA, Some(kind @ ("LOAD_FAILED" | "LOAD_CANCELLED" | "INVALID_REQUEST"))) => {
            InboundMessage::LoadError {
                kind: kind.to_owned(),
                reason: payload
                    .get("reason")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
            }
        }
        _ => InboundMessage::Unknown,
    }
}

/// Extract the application rows of a `RECEIVER_STATUS` payload (absent or
/// malformed rows are skipped — tolerant decode).
fn decode_receiver_status(payload: &serde_json::Value) -> ReceiverStatusInfo {
    let applications = payload
        .get("status")
        .and_then(|s| s.get("applications"))
        .and_then(serde_json::Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| serde_json::from_value(row.clone()).ok())
                .collect()
        })
        .unwrap_or_default();
    ReceiverStatusInfo { applications }
}

/// Extract the status rows of a `MEDIA_STATUS` payload (rows with an unknown
/// player state are skipped — tolerant decode).
fn decode_media_status(payload: &serde_json::Value) -> Vec<MediaStatusEntry> {
    payload
        .get("status")
        .and_then(serde_json::Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    let player_state: PlayerState =
                        serde_json::from_value(row.get("playerState")?.clone()).ok()?;
                    Some(MediaStatusEntry {
                        media_session_id: row
                            .get("mediaSessionId")
                            .and_then(serde_json::Value::as_i64),
                        player_state,
                        idle_reason: row
                            .get("idleReason")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}
