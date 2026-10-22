//! The **Google Cast** managed-device driver (DEV-D2, ADR-M011).
//!
//! Cast playback of the program is a Devices-domain driver — **not** an
//! [`Output`](multiview_config::Output) variant: the media path is an existing
//! HLS rendition `multiview-output` already serves (encode-once-mux-many
//! preserved), and a Cast session is pure control plane, so invariants #1/#10
//! hold by construction — the engine never sees a session and nothing here can
//! pace or back-pressure the output clock.
//!
//! Layering (everything above the channel seam is socket-free testable):
//!
//! * [`protocol`] — the typed CASTV2 namespace payloads (pure JSON values).
//! * [`media`] — device-reachable media-URL construction (`cast_media_base`
//!   validation + the DEV-D1 mount delivery map).
//! * [`session`] — the supervised session actor over the [`session::CastChannel`]
//!   seam (CONNECT → LAUNCH `CC1AD845` → LOAD; 10 s PING / 20 s expiry / 5 s
//!   reconnect; re-LOAD on IDLE; preemption surfaced, never fought).
//! * [`runtime`] — the [`runtime::CastSessionFactory`] plugged into the SAME
//!   DEV-A4 `DevicePollerRegistry`/factory/tombstone machinery as `zowietek`.
//! * [`store`] — the ephemeral session records (runtime-only, never exported).
//! * [`net`] — the real wire layer (TLS + length-prefixed protobuf framing),
//!   behind the **off-by-default `cast` feature** so the default build pulls
//!   no protobuf/TLS dependency.
//!
//! Google Cast and Chromecast are trademarks of Google LLC; Multiview is not
//! certified by, endorsed by, or affiliated with Google. The protocol is
//! implemented from the BSD-3-Clause Chromium Open Screen sources and
//! community documentation — no proprietary SDK.

pub mod media;
#[cfg(feature = "cast")]
pub mod net;
pub mod protocol;
pub mod runtime;
pub mod session;
pub mod store;
