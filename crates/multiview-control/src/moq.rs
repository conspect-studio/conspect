//! Authorized, finite-lived `MoQ` subscription control. No transport is enabled by default.
use crate::{auth::Principal, preview::WhepScope};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

/// Decoder configuration; description is base64 decoder initialization data.
#[derive(Clone, serde::Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct MoqVideo {
    /// `WebCodecs` codec identifier.
    pub codec: String,
    /// Encoded width in pixels.
    pub coded_width: u32,
    /// Encoded height in pixels.
    pub coded_height: u32,
    /// Base64 decoder initialization bytes, when required.
    pub description: Option<String>,
}
/// Independent audio decoder configuration.
#[derive(Clone, serde::Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct MoqAudio {
    /// Stable audio track identifier.
    pub track: String,
    /// `WebCodecs` codec identifier.
    pub codec: String,
    /// Audio samples per second.
    pub sample_rate: u32,
    /// Decoded channel count.
    pub number_of_channels: u32,
    /// Base64 decoder initialization bytes, when required.
    pub description: Option<String>,
}
/// One selectable video alternative of an authorized ladder, on the HTTP wire.
///
/// `description` stays **base64**, exactly like [`MoqVideo::description`]; the
/// Hang catalog's hexadecimal form is a different, internal encoding and must
/// never be substituted here. A published alternative always carries an exact
/// rational cadence and a positive verified bitrate: a rung whose cadence or
/// bitrate is not verified is omitted (`video_alternatives` stays empty) rather
/// than fabricated or emitted as `0`.
#[derive(Clone, serde::Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct MoqVideoAlternative {
    /// Stable rendition identity inside the family (the published track name).
    pub track: String,
    /// `WebCodecs` codec identifier from the opened encoder.
    pub codec: String,
    /// Encoded width in pixels.
    pub coded_width: u32,
    /// Encoded height in pixels.
    pub coded_height: u32,
    /// Exact cadence numerator (frames per `cadence_denominator`).
    pub cadence_numerator: u32,
    /// Exact cadence denominator; never a float frame rate.
    pub cadence_denominator: u32,
    /// Verified bitrate in bits per second; never `0` and never a guess.
    pub bitrate_bps: u64,
    /// Base64 decoder initialization bytes, when the encoder supplied them.
    pub description: Option<String>,
}
/// Secret-bearing admission response. Deliberately does not implement Debug.
#[derive(Clone, serde::Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct MoqLease {
    /// Unique subscription identifier.
    pub lease_id: String,
    /// Immutable binding generation.
    pub generation: u64,
    /// Server-enforced expiry in RFC3339 format.
    pub expires_at: String,
    /// Conservative remaining server lifetime at response construction.
    pub expires_in_ms: u64,
    /// Short-lived authorized transport URL; never log.
    pub url: String,
    /// Authorized broadcast namespace.
    pub broadcast: String,
    /// Explicit negotiated protocol identifier.
    pub protocol: String,
    /// Video decoder configuration of the selected rendition.
    pub video: MoqVideo,
    /// Every authorized video alternative of this lease's ladder, in registry
    /// order. Empty for a single-rendition lease; the selected [`Self::video`]
    /// stays populated for back-compatible consumers.
    pub video_alternatives: Vec<MoqVideoAlternative>,
    /// Independent audio decoder configurations, shared by every alternative.
    pub audio: Vec<MoqAudio>,
    /// PTS-zero wall time in microseconds, when the pipeline supplied the epoch.
    /// `None` when no validated epoch is available on this seam — never a
    /// fabricated `0`, which would silently claim all renditions start at the
    /// Unix epoch.
    pub clock_wall_micros: Option<u64>,
}
/// Acknowledges a synchronous stop-enqueue fence, not transport shutdown.
#[derive(serde::Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct MoqRevoked {
    /// Unique subscription identifier.
    pub lease_id: String,
    /// Immutable binding generation.
    pub generation: u64,
    /// True only after the enqueue fence completes.
    pub revoked: bool,
}
/// Admission errors distinguish transport unavailability from policy/capacity failures.
#[derive(Debug, Clone, Copy)]
pub enum MoqReject {
    /// No authorized transport is ready.
    Unavailable,
    /// The principal does not own the resource.
    Forbidden,
    /// The bounded admission limit is reached.
    Capacity,
    /// The requested codec is unsupported.
    Codec,
    /// The lease does not exist.
    NotFound,
    /// The generation does not match.
    Generation,
}
/// Subscription-owned registry fence. Must synchronously stop enqueue and release demand;
/// transport teardown and retained batch leases remain independently bounded.
pub trait MoqFence: Send + Sync {
    /// Stop enqueue synchronously; never wait for network teardown. Implementations
    /// must be bounded and nonpanicking. A failed/panicking fence is never acknowledged.
    fn revoke(&self);
}
/// Provider must authorize resolved dependencies before admission. It must never encode per viewer.
/// Admission and fence callbacks must be bounded and nonblocking: no network or disk I/O.
/// The transport must independently enforce the supplied TTL at its enqueue/session gate,
/// so a delayed control-plane executor cannot extend access to future media.
pub trait MoqProvider: Send + Sync {
    /// Whether the complete authorized transport is ready.
    fn available(&self) -> bool {
        false
    }
    /// Authorize resolved dependencies and acquire one bounded subscription.
    /// # Errors
    /// Returns admission, authorization, or transport failures.
    fn admit(
        &self,
        principal: &Principal,
        scope: &WhepScope,
        subscription_id: &str,
        ttl: Duration,
    ) -> Result<(MoqLease, Arc<dyn MoqFence>), MoqReject>;
}
struct Unavailable;
impl MoqProvider for Unavailable {
    fn admit(
        &self,
        _: &Principal,
        _: &WhepScope,
        _: &str,
        _: Duration,
    ) -> Result<(MoqLease, Arc<dyn MoqFence>), MoqReject> {
        Err(MoqReject::Unavailable)
    }
}
struct Entry {
    owner: String,
    scope: String,
    generation: u64,
    fence: Option<Arc<dyn MoqFence>>,
    fenced: Arc<std::sync::atomic::AtomicBool>,
}
/// Bounded control registry. Identity tombstones survive expiry for one bounded
/// grace period so a matching DELETE can acknowledge the completed fence.
pub struct MoqRegistry {
    provider: Arc<dyn MoqProvider>,
    entries: Mutex<HashMap<String, Entry>>,
    ttl: Duration,
    capacity: usize,
    tombstones: Mutex<HashMap<String, (Entry, std::time::Instant)>>,
}
impl Default for MoqRegistry {
    fn default() -> Self {
        Self::new(Arc::new(Unavailable), Duration::from_secs(30), 128)
    }
}
impl MoqRegistry {
    /// TTL is always finite and capped at one minute, including configured providers.
    pub fn new(provider: Arc<dyn MoqProvider>, ttl: Duration, capacity: usize) -> Self {
        Self {
            provider,
            entries: Mutex::new(HashMap::new()),
            ttl: ttl.clamp(Duration::from_secs(1), Duration::from_secs(60)),
            capacity: capacity.min(4096),
            tombstones: Mutex::new(HashMap::new()),
        }
    }
    /// Whether admission has a real transport provider.
    pub fn available(&self) -> bool {
        self.provider.available()
    }
    /// Admit a finite subscription and schedule independent expiry.
    /// # Errors
    /// Returns capacity or provider admission failure.
    pub fn open(
        self: &Arc<Self>,
        principal: &Principal,
        scope: &WhepScope,
    ) -> Result<MoqLease, MoqReject> {
        let mut entries = self.entries.lock().map_err(|_| MoqReject::Unavailable)?;
        if entries.len() >= self.capacity {
            return Err(MoqReject::Capacity);
        }
        let started = std::time::Instant::now();
        let id = uuid::Uuid::new_v4().to_string();
        let (mut lease, fence) = self.provider.admit(principal, scope, &id, self.ttl)?;
        lease.lease_id.clone_from(&id);
        lease.expires_in_ms = u64::try_from(self.ttl.saturating_sub(started.elapsed()).as_millis())
            .unwrap_or(u64::MAX);
        let seconds = (SystemTime::now() + self.ttl)
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        lease.expires_at =
            chrono::DateTime::from_timestamp(i64::try_from(seconds).unwrap_or(i64::MAX), 0)
                .map(|v| v.to_rfc3339())
                .unwrap_or_default();
        entries.insert(
            id.clone(),
            Entry {
                owner: principal.key_id.clone(),
                scope: scope.label(),
                generation: lease.generation,
                fence: Some(fence),
                fenced: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
        );
        let weak = Arc::downgrade(self);
        let ttl = self.ttl;
        tokio::spawn(async move {
            tokio::time::sleep(ttl).await;
            if let Some(registry) = weak.upgrade() {
                let transition = (|| {
                    // Acquire both maps before removing the identity. A poisoned
                    // second lock cannot discard an otherwise valid lease.
                    let mut entries = registry.entries.lock().ok()?;
                    let mut tombstones = registry.tombstones.lock().ok()?;
                    let mut entry = entries.remove(&id)?;
                    let fence = entry.fence.take();
                    let fenced = entry.fenced.clone();
                    let now = std::time::Instant::now();
                    tombstones.retain(|_, (_, expiry)| *expiry > now);
                    if tombstones.len() >= registry.capacity {
                        if let Some(oldest) = tombstones
                            .iter()
                            .min_by_key(|(_, (_, expiry))| *expiry)
                            .map(|(id, _)| id.clone())
                        {
                            tombstones.remove(&oldest);
                        }
                    }
                    tombstones.insert(id.clone(), (entry, now + Duration::from_secs(60)));
                    Some((fence, fenced))
                })();
                if let Some((Some(fence), fenced)) = transition {
                    fence.revoke();
                    fenced.store(true, std::sync::atomic::Ordering::Release);
                }
            }
        });
        Ok(lease)
    }
    /// Fence only the owner, scope and exact binding generation.
    /// # Errors
    /// Returns unknown lease, wrong owner/scope, or stale generation.
    pub fn revoke(
        &self,
        principal: &Principal,
        scope: &WhepScope,
        id: &str,
        generation: u64,
    ) -> Result<MoqRevoked, MoqReject> {
        let mut entries = self.entries.lock().map_err(|_| MoqReject::Unavailable)?;
        let mut tombstones = self.tombstones.lock().map_err(|_| MoqReject::Unavailable)?;
        let now = std::time::Instant::now();
        tombstones.retain(|_, (_, expiry)| *expiry > now);
        let entry = if let Some(entry) = entries.get_mut(id) {
            entry
        } else {
            &mut tombstones.get_mut(id).ok_or(MoqReject::NotFound)?.0
        };
        if entry.owner != principal.key_id || entry.scope != scope.label() {
            return Err(MoqReject::Forbidden);
        }
        if entry.generation != generation {
            return Err(MoqReject::Generation);
        }
        let fence = entry.fence.take();
        let fenced = entry.fenced.clone();
        if fence.is_none() && !fenced.load(std::sync::atomic::Ordering::Acquire) {
            // A concurrent expiry/DELETE owns the synchronous fence. Never ACK
            // before that callback completes and never block unrelated leases.
            return Err(MoqReject::Unavailable);
        }
        drop(tombstones);
        drop(entries);
        if let Some(fence) = fence {
            fence.revoke();
            fenced.store(true, std::sync::atomic::Ordering::Release);
        }
        Ok(MoqRevoked {
            lease_id: id.to_owned(),
            generation,
            revoked: true,
        })
    }
}
impl Drop for MoqRegistry {
    fn drop(&mut self) {
        if let Ok(entries) = self.entries.get_mut() {
            for (_, entry) in entries.drain() {
                if let Some(fence) = entry.fence {
                    fence.revoke();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Fence(Arc<AtomicUsize>);
    impl MoqFence for Fence {
        fn revoke(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    struct Provider(Arc<AtomicUsize>);
    impl MoqProvider for Provider {
        fn admit(
            &self,
            _: &Principal,
            _: &WhepScope,
            _: &str,
            _: Duration,
        ) -> Result<(MoqLease, Arc<dyn MoqFence>), MoqReject> {
            Ok((
                MoqLease {
                    lease_id: String::new(),
                    generation: 7,
                    expires_at: String::new(),
                    expires_in_ms: 0,
                    url: "https://example.test".into(),
                    broadcast: "test".into(),
                    protocol: "moqt-16".into(),
                    video: MoqVideo {
                        codec: "vp8".into(),
                        coded_width: 320,
                        coded_height: 180,
                        description: None,
                    },
                    video_alternatives: vec![],
                    clock_wall_micros: None,
                    audio: vec![],
                },
                Arc::new(Fence(self.0.clone())),
            ))
        }
    }
    fn principal(id: &str) -> Principal {
        Principal {
            key_id: id.into(),
            role: crate::auth::Role::Admin,
            scoped_object_ids: None,
            scoped_output_ids: None,
            scoped_discovery_domains: None,
        }
    }
    #[tokio::test]
    async fn owner_scope_generation_fence_and_idempotence() {
        let calls = Arc::new(AtomicUsize::new(0));
        let r = Arc::new(MoqRegistry::new(
            Arc::new(Provider(calls.clone())),
            Duration::from_secs(30),
            2,
        ));
        let p = principal("a");
        let lease = r.open(&p, &WhepScope::Program).unwrap();
        assert!(matches!(
            r.revoke(&principal("b"), &WhepScope::Program, &lease.lease_id, 7),
            Err(MoqReject::Forbidden)
        ));
        assert!(matches!(
            r.revoke(&p, &WhepScope::Input("a".into()), &lease.lease_id, 7),
            Err(MoqReject::Forbidden)
        ));
        assert!(matches!(
            r.revoke(&p, &WhepScope::Program, &lease.lease_id, 6),
            Err(MoqReject::Generation)
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        for _ in 0..2 {
            assert!(
                r.revoke(&p, &WhepScope::Program, &lease.lease_id, 7)
                    .unwrap()
                    .revoked
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        drop(r);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn finite_expiry_releases_abandoned_subscription_and_capacity() {
        let calls = Arc::new(AtomicUsize::new(0));
        let r = Arc::new(MoqRegistry::new(
            Arc::new(Provider(calls.clone())),
            Duration::ZERO,
            1,
        ));
        let p = principal("a");
        r.open(&p, &WhepScope::Program).unwrap();
        assert!(matches!(
            r.open(&p, &WhepScope::Program),
            Err(MoqReject::Capacity)
        ));
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(r.open(&p, &WhepScope::Program).is_ok());
        drop(r);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
    #[tokio::test]
    async fn expired_identity_ack_requires_owner_scope_and_generation() {
        let calls = Arc::new(AtomicUsize::new(0));
        let registry = Arc::new(MoqRegistry::new(
            Arc::new(Provider(calls.clone())),
            Duration::from_secs(1),
            1,
        ));
        let owner = principal("owner");
        let lease = registry.open(&owner, &WhepScope::Program).unwrap();
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(matches!(
            registry.revoke(&principal("other"), &WhepScope::Program, &lease.lease_id, 7),
            Err(MoqReject::Forbidden)
        ));
        assert!(matches!(
            registry.revoke(&owner, &WhepScope::Program, &lease.lease_id, 8),
            Err(MoqReject::Generation)
        ));
        assert!(
            registry
                .revoke(&owner, &WhepScope::Program, &lease.lease_id, 7)
                .unwrap()
                .revoked
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(registry.open(&owner, &WhepScope::Program).is_ok());
        assert!(registry.tombstones.lock().unwrap().len() <= 1);
    }

    #[tokio::test]
    async fn fence_callback_runs_without_registry_locks_and_pending_never_acks() {
        struct Reentrant {
            registry: std::sync::Weak<MoqRegistry>,
            id: String,
            checked: Arc<AtomicUsize>,
        }
        impl MoqFence for Reentrant {
            fn revoke(&self) {
                let registry = self.registry.upgrade().unwrap();
                assert!(registry.entries.try_lock().is_ok());
                assert!(registry.tombstones.try_lock().is_ok());
                assert!(matches!(
                    registry.revoke(&principal("a"), &WhepScope::Program, &self.id, 7),
                    Err(MoqReject::Unavailable)
                ));
                self.checked.fetch_add(1, Ordering::SeqCst);
            }
        }
        let registry = Arc::new(MoqRegistry::default());
        let checked = Arc::new(AtomicUsize::new(0));
        registry.entries.lock().unwrap().insert(
            "id".into(),
            Entry {
                owner: "a".into(),
                scope: WhepScope::Program.label(),
                generation: 7,
                fence: Some(Arc::new(Reentrant {
                    registry: Arc::downgrade(&registry),
                    id: "id".into(),
                    checked: checked.clone(),
                })),
                fenced: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
        );
        assert!(
            registry
                .revoke(&principal("a"), &WhepScope::Program, "id", 7)
                .unwrap()
                .revoked
        );
        assert_eq!(checked.load(Ordering::SeqCst), 1);
        assert!(
            registry
                .revoke(&principal("a"), &WhepScope::Program, "id", 7)
                .unwrap()
                .revoked
        );
    }

    #[test]
    fn panicking_fence_never_becomes_acknowledged() {
        struct Panics;
        impl MoqFence for Panics {
            fn revoke(&self) {
                panic!("invalid fence implementation");
            }
        }
        let registry = MoqRegistry::default();
        registry.entries.lock().unwrap().insert(
            "id".into(),
            Entry {
                owner: "a".into(),
                scope: WhepScope::Program.label(),
                generation: 7,
                fence: Some(Arc::new(Panics)),
                fenced: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
        );
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            registry.revoke(&principal("a"), &WhepScope::Program, "id", 7)
        }));
        assert!(result.is_err());
        assert!(matches!(
            registry.revoke(&principal("a"), &WhepScope::Program, "id", 7),
            Err(MoqReject::Unavailable)
        ));
    }

    #[tokio::test]
    async fn default_unavailable() {
        let r = Arc::new(MoqRegistry::default());
        assert!(!r.available());
        assert!(matches!(
            r.open(&principal("a"), &WhepScope::Program),
            Err(MoqReject::Unavailable)
        ));
    }
}
