//! mDNS/DNS-SD device **discovery** — new shared control-plane infrastructure
//! (ADR-M008 §6, ADR-0041 untrusted-inventory doctrine; DEV-A5).
//!
//! No working mDNS code existed in the repo before this module (the NMOS
//! transport is a compile-only seam). Discovery is built once here and shared by
//! the Cast / NDI / zowietek browses. Its output is an **untrusted inventory
//! requiring explicit confirm-adopt**: discovered services are *hints*, never
//! registry entries. Discovery NEVER creates a device — an operator confirms by
//! `POST`ing to the existing `/api/v1/devices/{id}` adopt referencing a
//! discovered address. This is the ADR-0041 doctrine applied to devices.
//!
//! ## What is browsed
//!
//! * `_googlecast._tcp.local.` — Cast targets. Cast **groups** advertise a
//!   non-8009 port, so the mDNS-advertised port is always used, never assumed.
//! * `_ndi._tcp.local.` — NDI sources. `ZowieBox` encoder-mode units advertise
//!   this (verified on hardware 2026-06-10).
//! * an **operator-configured** zowietek-control service type — the vendor's
//!   control-API mDNS service type is **undocumented / unverified** (the public
//!   doc mentions an mDNS section but gives no service-type string). We do **not**
//!   fabricate one: a zowietek-control browse type is recognised **only** when the
//!   operator configures it via the `[discovery]` config section
//!   (`multiview_config::DiscoveryConfig::zowietek_service_type`; best-effort,
//!   clearly labelled unverified). Until then, such a service is reported
//!   `unknown`, never mis-claimed as zowietek. The same section's
//!   `extra_service_types` adds further browse types (extra scope, never extra
//!   trust — they classify by the same honest inference).
//!
//! ## Trust + isolation (ADR-0041, invariant #10)
//!
//! The inventory is bounded (drop-oldest), TTL-expiring, and dedup-keyed. The
//! browse is a control-plane task that publishes `device.discovered` via the
//! conflating [`DeviceBroadcaster`](super::broadcaster::DeviceBroadcaster) into
//! the engine's **non-blocking drop-oldest** event broadcast and writes the
//! inventory behind a short-held `Mutex` the engine never touches. Nothing here
//! can back-pressure the engine — the same proof shape as the other
//! control-plane producers. The real multicast socket lives behind the
//! off-by-default `discovery` feature ([`MdnsBrowser`]); the model, the
//! driver-kind inference, the interleaved drain ([`drain_interleaved`]), and
//! the injected [`DiscoveryBrowser`] seam are always compiled and tested
//! socket-free.
//!
//! ## Hostile-responder bounds
//!
//! mDNS answers are unauthenticated LAN input. Everything a responder controls
//! is bounded before it is retained: one scan collects at most
//! [`MAX_SCAN_EVENTS`] services (fair-shared across the browsed types), and
//! every advertised string/list is truncated/capped by
//! [`DiscoveredService::from_raw`] ([`MAX_FIELD_LEN`], [`MAX_TXT_VALUE_LEN`],
//! [`MAX_TXT_RECORDS`], [`MAX_ENDPOINTS`]). Scans themselves are single-flight
//! ([`ScanGate`]) — bounded *and* rate-limited (ADR-M008).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use multiview_events::AddressFamily;
use serde::{Deserialize, Serialize};

use crate::command::OperationId;

/// The DNS-SD service type for Cast targets (Chromecast / Cast groups).
pub const CAST_SERVICE_TYPE: &str = "_googlecast._tcp.local.";

/// The DNS-SD service type for NDI sources. `ZowieBox` encoder-mode units
/// advertise this (verified on hardware).
pub const NDI_SERVICE_TYPE: &str = "_ndi._tcp.local.";

/// The default time budget a single scan browses for before it stops.
pub const DEFAULT_SCAN_BUDGET: Duration = Duration::from_secs(5);

/// The default time a discovered service stays in the inventory after its last
/// sighting before it is purged as stale (TTL).
pub const DEFAULT_ENTRY_TTL: Duration = Duration::from_secs(120);

/// The default cap on the number of retained discovered services (bounded,
/// drop-oldest — invariant #10).
pub const DEFAULT_INVENTORY_CAPACITY: usize = 256;

/// Hostile-responder bound: the cap on raw services one scan may collect
/// across all browsed types — 4× the default inventory capacity, far beyond
/// any sane LAN's mDNS population, so a flood of forged responses bounds the
/// scan's transient memory instead of growing it (invariant #10).
pub const MAX_SCAN_EVENTS: usize = DEFAULT_INVENTORY_CAPACITY * 4;

/// Hostile-responder bound: bytes retained of any advertised name / host /
/// service-type / TXT key (a legitimate DNS name is at most 253 bytes).
pub const MAX_FIELD_LEN: usize = 256;

/// Hostile-responder bound: bytes retained of one advertised TXT value.
pub const MAX_TXT_VALUE_LEN: usize = 512;

/// Hostile-responder bound: TXT records retained per discovered service.
pub const MAX_TXT_RECORDS: usize = 64;

/// Hostile-responder bound: resolved endpoints retained per discovered
/// service. Endpoints are ordered AAAA-first **before** the cap, so a flood of
/// forged IPv4 answers can never evict the IPv6 lead (ADR-0042).
pub const MAX_ENDPOINTS: usize = 16;

/// The inferred driver family of a discovered service, from its service type
/// (and the operator-configured zowietek-control type, if any).
///
/// A **closed** `#[non_exhaustive]` enum: a new discoverable family is a new
/// variant plus a wired browse type, never an open registry. `Unknown` is the
/// honest catch-all — a service we browsed but cannot classify is reported as
/// such, never guessed into a family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[non_exhaustive]
pub enum DiscoveryDriverKind {
    /// A Cast target (`_googlecast._tcp`).
    Cast,
    /// An NDI source (`_ndi._tcp`) — includes `ZowieBox` encoder-mode units.
    NdiSource,
    /// A zowietek control-API service, recognised **only** when the operator
    /// configures the (unverified) service type — never fabricated.
    ZowietekControl,
    /// A browsed service we cannot classify into a known family.
    Unknown,
}

impl DiscoveryDriverKind {
    /// The wire token for this kind (matches the kebab-case serde rename), used
    /// as the `driver` field of the `device.discovered` event.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cast => "cast",
            Self::NdiSource => "ndi-source",
            Self::ZowietekControl => "zowietek-control",
            Self::Unknown => "unknown",
        }
    }
}

/// Infer the [`DiscoveryDriverKind`] from a DNS-SD `service_type`.
///
/// `configured_zowietek` is the operator-configured zowietek-control service
/// type, if any: when a browsed service type matches it exactly, the service is
/// classified [`DiscoveryDriverKind::ZowietekControl`]. The vendor's control-API
/// service type is **unverified**, so it is never assumed from a built-in
/// constant — only the configured string recognises it. Matching is suffix-
/// tolerant of the trailing `.local.` so a configured type works with or without
/// it. Anything unrecognised is [`DiscoveryDriverKind::Unknown`].
#[must_use]
pub fn infer_driver_kind(
    service_type: &str,
    configured_zowietek: Option<&str>,
) -> DiscoveryDriverKind {
    if service_types_match(service_type, CAST_SERVICE_TYPE) {
        return DiscoveryDriverKind::Cast;
    }
    if service_types_match(service_type, NDI_SERVICE_TYPE) {
        return DiscoveryDriverKind::NdiSource;
    }
    if let Some(configured) = configured_zowietek {
        if service_types_match(service_type, configured) {
            return DiscoveryDriverKind::ZowietekControl;
        }
    }
    DiscoveryDriverKind::Unknown
}

/// Compare two DNS-SD service types ignoring a trailing `.local.` / `.local`
/// difference (and a trailing dot), so `_ndi._tcp` and `_ndi._tcp.local.` are
/// treated as the same type.
fn service_types_match(a: &str, b: &str) -> bool {
    normalize_service_type(a) == normalize_service_type(b)
}

/// Strip a trailing `.local.` / `.local` and any trailing dot from a service
/// type for tolerant comparison.
fn normalize_service_type(ty: &str) -> &str {
    let trimmed = ty.trim_end_matches('.');
    trimmed
        .strip_suffix(".local")
        .unwrap_or(trimmed)
        .trim_end_matches('.')
}

/// One resolved management endpoint of a discovered service: a presentation-
/// ready address and its family. IPv6 is the lead family (ADR-0042); IPv4 is
/// labelled [`AddressFamily::Ipv4Legacy`]. IPv6 literals are bracketed so the
/// `host:port` form is URL-safe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DiscoveredEndpoint {
    /// The `host:port` management address (IPv6 literals bracketed).
    pub address: String,
    /// The address family — `ipv6` (lead) or `ipv4-legacy`. The shared
    /// [`AddressFamily`] enum carries no `ToSchema`, so the schema models the
    /// wire string directly (the same `ipv6` / `ipv4-legacy` tokens serde emits).
    #[cfg_attr(feature = "openapi", schema(value_type = String, example = "ipv6"))]
    pub family: AddressFamily,
}

impl DiscoveredEndpoint {
    /// Build an endpoint from an [`IpAddr`] + port: bracket IPv6 literals and
    /// label the family (IPv6 lead, IPv4 legacy).
    #[must_use]
    pub fn from_addr(addr: IpAddr, port: u16) -> Self {
        match addr {
            IpAddr::V6(v6) => Self {
                address: format!("[{v6}]:{port}"),
                family: AddressFamily::Ipv6,
            },
            IpAddr::V4(v4) => Self {
                address: format!("{v4}:{port}"),
                family: AddressFamily::Ipv4Legacy,
            },
        }
    }
}

/// A **raw** discovered service as the browser yields it, before classification
/// and ordering. The injected [`DiscoveryBrowser`] produces these; the model
/// turns them into a [`DiscoveredService`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RawDiscoveredService {
    /// The DNS-SD service type (e.g. `_ndi._tcp.local.`).
    pub service_type: String,
    /// The instance name (the human-facing service name).
    pub instance_name: String,
    /// The advertised host (`*.local.`), informational.
    pub host: String,
    /// The mDNS-**advertised** port (used verbatim — Cast groups advertise
    /// non-8009 ports).
    pub port: u16,
    /// The resolved addresses (mixed IPv4/IPv6).
    pub addresses: Vec<IpAddr>,
    /// The decoded TXT key/value records.
    pub txt: Vec<(String, String)>,
}

impl RawDiscoveredService {
    /// Build a raw discovered service. The constructor exists because the struct
    /// is `#[non_exhaustive]` (so adding a field is not a breaking change), which
    /// otherwise forbids struct-expression construction from outside this crate.
    #[must_use]
    pub fn new(
        service_type: impl Into<String>,
        instance_name: impl Into<String>,
        host: impl Into<String>,
        port: u16,
        addresses: Vec<IpAddr>,
        txt: Vec<(String, String)>,
    ) -> Self {
        Self {
            service_type: service_type.into(),
            instance_name: instance_name.into(),
            host: host.into(),
            port,
            addresses,
            txt,
        }
    }
}

/// One **untrusted** discovery-inventory row: a classified, AAAA-first service
/// requiring explicit confirm-adopt (ADR-0041). It carries **no registry id** —
/// it is not a device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DiscoveredService {
    /// The dedup key (driver kind + instance name + service type). Two sightings
    /// of the same service share a key and the newer replaces the older.
    pub key: String,
    /// The inferred driver family.
    pub driver_kind: DiscoveryDriverKind,
    /// The DNS-SD service type browsed.
    pub service_type: String,
    /// The advertised instance/service name.
    pub name: String,
    /// The advertised host (`*.local.`), informational.
    pub host: String,
    /// The mDNS-advertised port.
    pub port: u16,
    /// The management endpoints, **AAAA-first** (IPv6 lead, IPv4 legacy).
    pub endpoints: Vec<DiscoveredEndpoint>,
    /// The primary management address — the first (IPv6, if any) endpoint's
    /// `host:port`. This is the address an operator references when confirming
    /// adoption via `POST /devices/{id}`.
    pub primary_address: String,
    /// The decoded TXT records (advertised metadata), key-sorted.
    pub txt: Vec<TxtRecord>,
    /// When this row was last seen (Unix nanoseconds), informational for the UI.
    pub last_seen_unix_ns: i64,
    /// The TTL deadline after which the row is purged as stale. Not serialized —
    /// it is internal bookkeeping (a monotonic [`Instant`]).
    #[serde(skip)]
    expires_at: Option<Instant>,
}

/// One advertised TXT key/value record (serializable, sortable).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct TxtRecord {
    /// The TXT key.
    pub key: String,
    /// The TXT value (decoded as UTF-8 by the browser).
    pub value: String,
}

impl DiscoveredService {
    /// Classify + order a [`RawDiscoveredService`] into an untrusted inventory
    /// row, stamping its TTL deadline at `expires_at`.
    ///
    /// `configured_zowietek` is the operator-configured zowietek-control service
    /// type, if any (see [`infer_driver_kind`]). Endpoints are ordered
    /// **AAAA-first** with IPv4 labelled legacy; the primary address is the
    /// leading (IPv6, where present) endpoint.
    ///
    /// Every responder-controlled value is bounded here (mDNS answers are
    /// unauthenticated LAN input): strings are truncated to [`MAX_FIELD_LEN`] /
    /// [`MAX_TXT_VALUE_LEN`] bytes (on a char boundary), TXT records are capped
    /// at [`MAX_TXT_RECORDS`], and endpoints at [`MAX_ENDPOINTS`] — applied
    /// after AAAA-first ordering so the IPv6 lead survives the cap.
    #[must_use]
    pub fn from_raw(
        raw: &RawDiscoveredService,
        configured_zowietek: Option<&str>,
        expires_at: Instant,
    ) -> Self {
        let service_type = truncate_field(&raw.service_type, MAX_FIELD_LEN);
        let name = truncate_field(&raw.instance_name, MAX_FIELD_LEN);
        let host = truncate_field(&raw.host, MAX_FIELD_LEN);
        let driver_kind = infer_driver_kind(&service_type, configured_zowietek);
        let mut endpoints = order_endpoints(&raw.addresses, raw.port);
        endpoints.truncate(MAX_ENDPOINTS);
        let primary_address = endpoints
            .first()
            .map_or_else(|| host.clone(), |ep| ep.address.clone());
        let mut txt: Vec<TxtRecord> = raw
            .txt
            .iter()
            .take(MAX_TXT_RECORDS)
            .map(|(k, v)| TxtRecord {
                key: truncate_field(k, MAX_FIELD_LEN),
                value: truncate_field(v, MAX_TXT_VALUE_LEN),
            })
            .collect();
        txt.sort();
        Self {
            key: dedup_key(driver_kind, &name, &service_type),
            driver_kind,
            service_type,
            name,
            host,
            port: raw.port,
            endpoints,
            primary_address,
            txt,
            last_seen_unix_ns: unix_now_ns(),
            expires_at: Some(expires_at),
        }
    }

    /// The primary endpoint (IPv6-first), or a synthetic host-only endpoint when
    /// no address resolved.
    #[must_use]
    pub fn primary(&self) -> DiscoveredEndpoint {
        self.endpoints
            .first()
            .cloned()
            .unwrap_or(DiscoveredEndpoint {
                address: self.primary_address.clone(),
                family: AddressFamily::Ipv6,
            })
    }

    /// Whether this row has passed its TTL deadline at `now`.
    #[must_use]
    fn is_expired(&self, now: Instant) -> bool {
        self.expires_at.is_some_and(|deadline| now >= deadline)
    }
}

/// Order resolved addresses **AAAA-first** (IPv6 lead, IPv4 legacy) into
/// presentation-ready endpoints. Within a family, original order is preserved.
#[must_use]
pub fn order_endpoints(addrs: &[IpAddr], port: u16) -> Vec<DiscoveredEndpoint> {
    // Partition on the IP variant itself (the lead-IPv6 ordering), not the
    // `#[non_exhaustive]` family enum, so the AAAA-first ordering stays exhaustive.
    let mut v6: Vec<DiscoveredEndpoint> = Vec::new();
    let mut v4: Vec<DiscoveredEndpoint> = Vec::new();
    for &addr in addrs {
        let ep = DiscoveredEndpoint::from_addr(addr, port);
        if addr.is_ipv6() {
            v6.push(ep);
        } else {
            v4.push(ep);
        }
    }
    v6.extend(v4);
    v6
}

/// Build the dedup key for a service: a service is the same across sightings
/// when its driver kind, instance name, and service type match.
fn dedup_key(kind: DiscoveryDriverKind, instance: &str, service_type: &str) -> String {
    format!("{}|{}|{}", kind.as_str(), instance, service_type)
}

/// Truncate a responder-controlled string to at most `max` **bytes** on a char
/// boundary (a hostile responder must never grow retained memory or split a
/// UTF-8 sequence). Values within the bound are kept verbatim.
fn truncate_field(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_owned();
    }
    let mut end = max;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value.get(..end).unwrap_or_default().to_owned()
}

/// System time as Unix nanoseconds (informational `last_seen` stamp; not used
/// for TTL, which is monotonic).
fn unix_now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

/// The bounded, TTL-expiring, dedup-keyed **untrusted** discovery inventory
/// (ADR-0041). Control-plane state behind a `Mutex`; bounded drop-oldest so it
/// can never grow without bound (invariant #10). It is **never** the device
/// registry — its rows are hints requiring explicit confirm-adopt.
#[derive(Debug)]
pub struct DiscoveryInventory {
    inner: Mutex<InventoryInner>,
    /// The retained-row cap (drop-oldest beyond it).
    capacity: usize,
}

/// The mutex-guarded interior of [`DiscoveryInventory`].
#[derive(Debug, Default)]
struct InventoryInner {
    /// key → row (latest-wins on re-sighting).
    rows: HashMap<String, DiscoveredService>,
    /// Insertion order of keys, so the bound evicts the oldest row first.
    order: std::collections::VecDeque<String>,
}

impl DiscoveryInventory {
    /// A fresh, empty inventory bounded at `capacity` rows (a `0` is promoted to
    /// `1` so the store always retains at least one row).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(InventoryInner::default()),
            capacity: capacity.max(1),
        }
    }

    /// Lock the inner map, recovering from a poisoned lock (a panic in another
    /// request must not wedge discovery).
    fn lock(&self) -> std::sync::MutexGuard<'_, InventoryInner> {
        match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Insert or refresh a discovered service (latest-wins by dedup key),
    /// enforcing the bound by evicting the oldest row when over capacity.
    pub fn upsert(&self, service: DiscoveredService) {
        let mut guard = self.lock();
        let key = service.key.clone();
        if guard.rows.insert(key.clone(), service).is_none() {
            // A genuinely new key: track insertion order for the drop-oldest bound.
            guard.order.push_back(key);
        }
        let cap = self.capacity;
        while guard.order.len() > cap {
            if let Some(evicted) = guard.order.pop_front() {
                guard.rows.remove(&evicted);
            }
        }
    }

    /// The current untrusted inventory, **id-sorted by key**, with stale (TTL-
    /// expired) rows purged. Reads at `Instant::now()`.
    #[must_use]
    pub fn snapshot(&self) -> Vec<DiscoveredService> {
        self.snapshot_at(Instant::now())
    }

    /// The untrusted inventory at an explicit `now` (purging expired rows),
    /// id-sorted by key. Split out so TTL expiry is deterministically testable.
    #[must_use]
    pub fn snapshot_at(&self, now: Instant) -> Vec<DiscoveredService> {
        let mut guard = self.lock();
        // Purge expired rows lazily on read.
        let expired: Vec<String> = guard
            .rows
            .iter()
            .filter(|(_, svc)| svc.is_expired(now))
            .map(|(k, _)| k.clone())
            .collect();
        for key in expired {
            guard.rows.remove(&key);
            guard.order.retain(|k| k != &key);
        }
        let mut out: Vec<DiscoveredService> = guard.rows.values().cloned().collect();
        out.sort_by(|a, b| a.key.cmp(&b.key));
        out
    }

    /// Drop every row (e.g. before a fresh scan, if an operator wants a clean
    /// sweep — not done by default so concurrent scans accrete).
    pub fn clear(&self) {
        let mut guard = self.lock();
        guard.rows.clear();
        guard.order.clear();
    }
}

impl Default for DiscoveryInventory {
    fn default() -> Self {
        Self::new(DEFAULT_INVENTORY_CAPACITY)
    }
}

/// The browse seam (ADR-M008 §6 socket-free posture). A scan asks the browser
/// for the services it finds within a time budget across the given service
/// types. The default ([`NullBrowser`]) finds nothing; the binary swaps in the
/// real [`MdnsBrowser`] behind the `discovery` feature; tests inject a
/// [`StaticBrowser`]. The browser is the **only** thing that touches a socket,
/// exactly like the NMOS/router transport seams.
pub trait DiscoveryBrowser: Send + Sync {
    /// Browse `service_types` for up to `budget`, returning every resolved raw
    /// service. This is run on a control-plane task (never the engine), so a
    /// blocking implementation is acceptable; it must respect `budget` so a scan
    /// is always time-bounded.
    fn browse(&self, service_types: &[String], budget: Duration) -> Vec<RawDiscoveredService>;
}

/// The default browser: finds nothing (the pure default build has no mDNS
/// socket). Swapped for [`MdnsBrowser`] in a `discovery`-feature binary.
#[derive(Debug, Clone, Copy, Default)]
pub struct NullBrowser;

impl DiscoveryBrowser for NullBrowser {
    fn browse(&self, _service_types: &[String], _budget: Duration) -> Vec<RawDiscoveredService> {
        Vec::new()
    }
}

/// A test/seed browser that yields a fixed set of raw services (socket-free).
/// Used by the integration tests and any deterministic seed scenario.
#[derive(Debug, Clone, Default)]
pub struct StaticBrowser {
    services: Vec<RawDiscoveredService>,
}

impl StaticBrowser {
    /// A browser that yields `services` for any browse request.
    #[must_use]
    pub fn new(services: Vec<RawDiscoveredService>) -> Self {
        Self { services }
    }
}

impl DiscoveryBrowser for StaticBrowser {
    fn browse(&self, service_types: &[String], _budget: Duration) -> Vec<RawDiscoveredService> {
        // Only return services whose type was requested (so a scan restricted to
        // a subset of types behaves correctly).
        self.services
            .iter()
            .filter(|svc| {
                service_types
                    .iter()
                    .any(|ty| service_types_match(ty, &svc.service_type))
            })
            .cloned()
            .collect()
    }
}

/// The default set of service types a scan browses: Cast + NDI (+ a configured
/// zowietek-control type when present).
#[must_use]
pub fn default_service_types(configured_zowietek: Option<&str>) -> Vec<String> {
    let mut types = vec![CAST_SERVICE_TYPE.to_owned(), NDI_SERVICE_TYPE.to_owned()];
    if let Some(z) = configured_zowietek {
        if !types.iter().any(|t| service_types_match(t, z)) {
            types.push(z.to_owned());
        }
    }
    types
}

/// The full set of service types one scan browses: the
/// [`default_service_types`] (Cast + NDI + the configured zowietek-control
/// type) plus the operator-configured `extra` types, deduplicated with the
/// `.local.`-tolerant comparison so no type is ever browsed twice (re-browsing
/// a type would overwrite its live `mdns-sd` listener).
#[must_use]
pub fn scan_service_types(configured_zowietek: Option<&str>, extra: &[String]) -> Vec<String> {
    let mut types = default_service_types(configured_zowietek);
    for ty in extra {
        if !types.iter().any(|t| service_types_match(t, ty)) {
            types.push(ty.clone());
        }
    }
    types
}

/// One browse-event receiver the interleaved scan drain polls — the seam that
/// makes [`drain_interleaved`] CI-testable without sockets. The `discovery`
/// feature implements it over an `mdns-sd` browse channel; tests implement it
/// over plain tokio channels.
pub trait DrainReceiver {
    /// The event type the receiver yields.
    type Event;

    /// Await the next event, resolving to [`None`] once the channel is closed.
    /// Must be **cancel-safe** (the drain drops an in-flight `recv` future at
    /// the deadline; a delivered event must not be lost by that drop) — both
    /// `flume` and `tokio::sync::mpsc` receivers are.
    fn recv(&mut self) -> impl std::future::Future<Output = Option<Self::Event>> + Send;

    /// Take one already-queued event without waiting (the final post-deadline
    /// sweep).
    fn try_recv(&mut self) -> Option<Self::Event>;
}

/// Drain `receivers` **concurrently** until `budget` elapses, collecting at
/// most `max_events` events in total.
///
/// Each receiver is drained by its own task, so no receiver can starve
/// another: a chatty browse channel (`_googlecast._tcp` plus `mdns-sd`'s
/// periodic `SearchStarted` keepalives) is consumed continuously while the
/// quieter channels' events are collected the moment they arrive. Continuous
/// consumption is also the daemon-side liveness proof — `mdns-sd` delivers
/// into a bounded channel with a **blocking** send from its single daemon
/// thread, so an undrained receiver would stall every other browse; here every
/// receiver is always being awaited.
///
/// The global `max_events` cap (hostile-responder bound) is split fairly: each
/// receiver may contribute at most `max_events / receivers.len()` events, so a
/// flood of forged answers on one type cannot spend another type's share. At
/// the deadline each task takes a final **non-blocking sweep** of its queue so
/// events that arrived within the budget are never dropped on the floor.
pub async fn drain_interleaved<R>(
    receivers: Vec<R>,
    budget: Duration,
    max_events: usize,
) -> Vec<R::Event>
where
    R: DrainReceiver + Send + 'static,
    R::Event: Send + 'static,
{
    if receivers.is_empty() || max_events == 0 {
        return Vec::new();
    }
    let deadline = tokio::time::Instant::now() + budget;
    // Each receiver's fair share of the global cap (at least 1 so a scan of
    // many types still collects something); the per-task caps sum to at most
    // `max_events` except in the degenerate more-receivers-than-cap case,
    // which the final truncate bounds exactly.
    let share = (max_events / receivers.len()).max(1);
    let mut tasks = tokio::task::JoinSet::new();
    for mut rx in receivers {
        tasks.spawn(async move {
            let mut out: Vec<R::Event> = Vec::new();
            while out.len() < share {
                tokio::select! {
                    event = rx.recv() => match event {
                        Some(event) => out.push(event),
                        // Closed: nothing more can arrive on this receiver.
                        None => return out,
                    },
                    () = tokio::time::sleep_until(deadline) => break,
                }
            }
            // Deadline (or share cap) reached: sweep whatever is already
            // queued without waiting, so in-budget events are never dropped.
            while out.len() < share {
                match rx.try_recv() {
                    Some(event) => out.push(event),
                    None => break,
                }
            }
            out
        });
    }
    let mut all = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        // A task can only fail by panicking, which only a test receiver can do
        // (the library receivers never panic); a panicked drain simply
        // contributes no events.
        if let Ok(events) = joined {
            all.extend(events);
        }
    }
    all.truncate(max_events);
    all
}

/// Single-flight admission for the discovery scan: **one in-flight browse**.
///
/// `mdns-sd` keeps one listener per service type — a second concurrent browse
/// of the same type overwrites the first scan's listener, and either scan's
/// `stop_browse` removes the other's live querier and flushes its cache, so
/// concurrent scans destructively corrupt each other. The gate admits one
/// running scan; a concurrent request **attaches** to it (it is answered with
/// the running scan's operation id) instead of starting a second browse. This
/// is also the scan rate limit (ADR-M008 "bounded and rate-limited"): at most
/// one LAN browse runs at any time.
#[derive(Debug, Default)]
pub struct ScanGate {
    /// The operation id of the running scan, if any.
    running: Mutex<Option<OperationId>>,
}

/// The outcome of asking the [`ScanGate`] to admit a scan.
#[derive(Debug)]
pub enum ScanAdmission {
    /// No scan was running: this request owns the slot. The held
    /// [`ScanGuard`] clears the slot when the scan task ends (it is dropped
    /// with the task, so even an unwound or cancelled scan can never wedge
    /// discovery).
    Started(ScanGuard),
    /// A scan is already running: answer with **its** operation id and do not
    /// browse again.
    Attached(OperationId),
}

impl ScanGate {
    /// A fresh gate with no scan running.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Admit a scan: claim the slot for `op`, or attach to the running scan.
    #[must_use]
    pub fn begin(self: &Arc<Self>, op: OperationId) -> ScanAdmission {
        let mut slot = self.lock();
        if let Some(running) = slot.as_ref() {
            return ScanAdmission::Attached(running.clone());
        }
        *slot = Some(op.clone());
        ScanAdmission::Started(ScanGuard {
            gate: Arc::clone(self),
            op,
        })
    }

    /// Clear the slot **iff** it still holds `op` (idempotent; a newer scan's
    /// claim is never erased by a stale guard).
    fn finish(&self, op: &OperationId) {
        let mut slot = self.lock();
        if slot.as_ref() == Some(op) {
            *slot = None;
        }
    }

    /// Lock the slot, recovering from a poisoned lock (a panic elsewhere must
    /// not wedge discovery).
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<OperationId>> {
        match self.running.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// Clears the [`ScanGate`] slot when dropped (the scan task holds it for the
/// duration of the browse).
#[derive(Debug)]
pub struct ScanGuard {
    gate: Arc<ScanGate>,
    op: OperationId,
}

impl Drop for ScanGuard {
    fn drop(&mut self) {
        self.gate.finish(&self.op);
    }
}

#[cfg(feature = "discovery")]
pub use mdns::MdnsBrowser;

/// The real `mdns-sd`-backed multicast browser, compiled only behind the
/// off-by-default `discovery` feature. This is the **only** socket-touching code
/// in the discovery subsystem — everything above (including the interleaved
/// drain it runs) is pure and tested socket-free.
#[cfg(feature = "discovery")]
mod mdns {
    use std::time::Duration;

    use mdns_sd::{ServiceDaemon, ServiceEvent};

    use super::{
        drain_interleaved, DiscoveryBrowser, DrainReceiver, RawDiscoveredService, MAX_SCAN_EVENTS,
    };

    /// A multicast mDNS/DNS-SD browser over [`mdns_sd::ServiceDaemon`].
    ///
    /// `mdns-sd` runs its own runtime-agnostic background thread and delivers
    /// browse events over one bounded channel per service type, with a
    /// **blocking** send from its single daemon thread. The browse drains all
    /// of those channels **concurrently** ([`drain_interleaved`]) until the
    /// time budget elapses, so no service type can starve another and the
    /// daemon is never left blocked on an undrained channel. The daemon socket
    /// is owned by the daemon thread, never the engine — so this cannot
    /// back-pressure the engine (invariant #10).
    pub struct MdnsBrowser {
        daemon: ServiceDaemon,
    }

    impl std::fmt::Debug for MdnsBrowser {
        // `mdns_sd::ServiceDaemon` is not `Debug` (it wraps a background-thread
        // handle); print an opaque marker so the public type stays debuggable.
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("MdnsBrowser").finish_non_exhaustive()
        }
    }

    impl MdnsBrowser {
        /// Start the mDNS daemon (binds the multicast sockets on the host's
        /// interfaces).
        ///
        /// # Errors
        ///
        /// Returns the `mdns-sd` error if the daemon cannot bind its sockets.
        pub fn new() -> Result<Self, mdns_sd::Error> {
            Ok(Self {
                daemon: ServiceDaemon::new()?,
            })
        }
    }

    /// One browse channel adapted to the interleaved drain: yields only
    /// resolved services, consuming (and discarding) the browse-lifecycle
    /// noise (`SearchStarted` etc.) so the keepalives keep the channel drained
    /// without counting against the scan's event cap.
    struct ResolvedReceiver(mdns_sd::Receiver<ServiceEvent>);

    impl DrainReceiver for ResolvedReceiver {
        type Event = RawDiscoveredService;

        async fn recv(&mut self) -> Option<RawDiscoveredService> {
            loop {
                match self.0.recv_async().await {
                    Ok(ServiceEvent::ServiceResolved(info)) => {
                        return Some(raw_from_resolved(&info));
                    }
                    // Browse-lifecycle noise: keep the channel drained.
                    Ok(_) => {}
                    Err(_) => return None,
                }
            }
        }

        fn try_recv(&mut self) -> Option<RawDiscoveredService> {
            loop {
                match self.0.try_recv() {
                    Ok(ServiceEvent::ServiceResolved(info)) => {
                        return Some(raw_from_resolved(&info));
                    }
                    Ok(_) => {}
                    Err(_) => return None,
                }
            }
        }
    }

    impl DiscoveryBrowser for MdnsBrowser {
        fn browse(&self, service_types: &[String], budget: Duration) -> Vec<RawDiscoveredService> {
            let mut receivers = Vec::new();
            for ty in service_types {
                match self.daemon.browse(ty) {
                    Ok(rx) => receivers.push(ResolvedReceiver(rx)),
                    Err(e) => tracing::warn!(
                        service_type = %ty,
                        error = %e,
                        "mDNS browse failed to start for service type; it is \
                         skipped for this scan"
                    ),
                }
            }
            let out = run_drain(receivers, budget);
            for ty in service_types {
                let _ = self.daemon.stop_browse(ty);
            }
            out
        }
    }

    /// Run the interleaved drain to completion on a **scan-local**
    /// current-thread runtime. `browse` is a blocking call by contract (the
    /// scan task wraps it in `spawn_blocking`), so blocking this thread on a
    /// private mini-runtime is safe, keeps the drain's timers/tasks off any
    /// shared runtime, and works even when no ambient runtime exists.
    fn run_drain(receivers: Vec<ResolvedReceiver>, budget: Duration) -> Vec<RawDiscoveredService> {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
        {
            Ok(runtime) => runtime,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "could not build the scan drain runtime; this scan finds nothing"
                );
                return Vec::new();
            }
        };
        runtime.block_on(drain_interleaved(receivers, budget, MAX_SCAN_EVENTS))
    }

    /// Convert a resolved `mdns-sd` service into our raw model.
    fn raw_from_resolved(info: &mdns_sd::ResolvedService) -> RawDiscoveredService {
        let addresses = info
            .addresses
            .iter()
            .map(mdns_sd::ScopedIp::to_ip_addr)
            .collect();
        let txt = info
            .txt_properties
            .iter()
            .map(|p| (p.key().to_owned(), p.val_str().to_owned()))
            .collect();
        // The fullname is `instance._type._proto.local.`; take the instance label
        // for a human-facing name, falling back to the fullname.
        let instance_name = info
            .fullname
            .split_once('.')
            .map_or_else(|| info.fullname.clone(), |(label, _)| label.to_owned());
        RawDiscoveredService {
            service_type: info.ty_domain.clone(),
            instance_name,
            host: info.host.clone(),
            port: info.port,
            addresses,
            txt,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn raw_ndi() -> RawDiscoveredService {
        RawDiscoveredService {
            service_type: NDI_SERVICE_TYPE.to_owned(),
            instance_name: "ZowieBox-A".to_owned(),
            host: "zb-a.local.".to_owned(),
            port: 5961,
            addresses: vec!["192.0.2.7".parse().unwrap(), "fd00:db8::7".parse().unwrap()],
            txt: vec![("model".to_owned(), "ZowieBox".to_owned())],
        }
    }

    #[test]
    fn infer_handles_trailing_local_difference() {
        // The browse constant carries `.local.`; a configured type without it
        // still matches (tolerant comparison).
        assert_eq!(
            infer_driver_kind("_ndi._tcp", None),
            DiscoveryDriverKind::NdiSource
        );
        assert_eq!(
            infer_driver_kind("_googlecast._tcp.local.", None),
            DiscoveryDriverKind::Cast
        );
    }

    #[test]
    fn zowietek_control_only_when_configured() {
        let cfg = "_zowietek-ctl._tcp.local.";
        assert_eq!(
            infer_driver_kind(cfg, Some(cfg)),
            DiscoveryDriverKind::ZowietekControl
        );
        assert_eq!(infer_driver_kind(cfg, None), DiscoveryDriverKind::Unknown);
    }

    #[test]
    fn order_endpoints_is_aaaa_first() {
        let eps = order_endpoints(
            &[
                "192.0.2.7".parse().unwrap(),
                "fd00:db8::7".parse().unwrap(),
                "198.51.100.9".parse().unwrap(),
            ],
            5961,
        );
        assert_eq!(eps[0].family, AddressFamily::Ipv6);
        assert_eq!(eps[1].family, AddressFamily::Ipv4Legacy);
        assert_eq!(eps[2].family, AddressFamily::Ipv4Legacy);
        assert!(eps[0].address.starts_with("[fd00:db8::7]"));
    }

    #[test]
    fn from_raw_classifies_and_orders() {
        let now = Instant::now() + Duration::from_secs(60);
        let svc = DiscoveredService::from_raw(&raw_ndi(), None, now);
        assert_eq!(svc.driver_kind, DiscoveryDriverKind::NdiSource);
        assert_eq!(svc.endpoints[0].family, AddressFamily::Ipv6);
        assert!(svc.primary_address.contains("fd00:db8::7"));
        // TXT records are key-sorted + decoded.
        assert_eq!(svc.txt[0].key, "model");
    }

    #[test]
    fn inventory_dedups_latest_wins() {
        let inv = DiscoveryInventory::new(8);
        let future = Instant::now() + Duration::from_secs(60);
        inv.upsert(DiscoveredService::from_raw(&raw_ndi(), None, future));
        inv.upsert(DiscoveredService::from_raw(&raw_ndi(), None, future));
        assert_eq!(inv.snapshot().len(), 1);
    }

    #[test]
    fn inventory_purges_expired() {
        let inv = DiscoveryInventory::new(8);
        let past = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .unwrap_or_else(Instant::now);
        inv.upsert(DiscoveredService::from_raw(&raw_ndi(), None, past));
        assert!(inv.snapshot().is_empty());
    }

    #[test]
    fn inventory_is_bounded() {
        let inv = DiscoveryInventory::new(2);
        let future = Instant::now() + Duration::from_secs(60);
        for n in 0..5 {
            let mut raw = raw_ndi();
            raw.instance_name = format!("ZB-{n}");
            inv.upsert(DiscoveredService::from_raw(&raw, None, future));
        }
        assert!(inv.snapshot().len() <= 2);
    }

    #[test]
    fn null_browser_finds_nothing() {
        let b = NullBrowser;
        assert!(b
            .browse(&default_service_types(None), Duration::from_millis(1))
            .is_empty());
    }

    #[test]
    fn static_browser_filters_by_requested_type() {
        let b = StaticBrowser::new(vec![raw_ndi()]);
        // NDI requested → found.
        assert_eq!(
            b.browse(&[NDI_SERVICE_TYPE.to_owned()], Duration::from_millis(1))
                .len(),
            1
        );
        // Only Cast requested → the NDI service is not returned.
        assert!(b
            .browse(&[CAST_SERVICE_TYPE.to_owned()], Duration::from_millis(1))
            .is_empty());
    }

    #[test]
    fn default_service_types_include_configured_zowietek_once() {
        let cfg = "_zowietek-ctl._tcp.local.";
        let types = default_service_types(Some(cfg));
        assert!(types.contains(&CAST_SERVICE_TYPE.to_owned()));
        assert!(types.contains(&NDI_SERVICE_TYPE.to_owned()));
        assert_eq!(types.iter().filter(|t| t.as_str() == cfg).count(), 1);
    }
}
