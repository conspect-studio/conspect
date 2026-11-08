//! Authorized, finite-lived MoQ subscription control. No transport is enabled by default.
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
    /// WebCodecs codec identifier.
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
    /// WebCodecs codec identifier.
    pub codec: String,
    /// Audio samples per second.
    pub sample_rate: u32,
    /// Decoded channel count.
    pub number_of_channels: u32,
    /// Base64 decoder initialization bytes, when required.
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
    /// Short-lived authorized transport URL; never log.
    pub url: String,
    /// Authorized broadcast namespace.
    pub broadcast: String,
    /// Explicit negotiated protocol identifier.
    pub protocol: String,
    /// Video decoder configuration.
    pub video: MoqVideo,
    /// Independent audio decoder configurations.
    pub audio: Vec<MoqAudio>,
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
#[derive(Debug)]
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
    /// Stop enqueue synchronously; never wait for network teardown.
    fn revoke(&self);
}
/// Provider must authorize resolved dependencies before admission. It must never encode per viewer.
pub trait MoqProvider: Send + Sync {
    /// Whether the complete authorized transport is ready.
    fn available(&self) -> bool {
        false
    }
    /// Authorize resolved dependencies and acquire one bounded subscription.
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
}
/// Bounded control registry. Tombstones live only until original TTL to allow idempotent close.
pub struct MoqRegistry {
    provider: Arc<dyn MoqProvider>,
    entries: Mutex<HashMap<String, Entry>>,
    ttl: Duration,
    capacity: usize,
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
        }
    }
    /// Whether admission has a real transport provider.
    pub fn available(&self) -> bool {
        self.provider.available()
    }
    /// Admit a finite subscription and schedule independent expiry.
    pub fn open(
        self: &Arc<Self>,
        principal: &Principal,
        scope: &WhepScope,
    ) -> Result<MoqLease, MoqReject> {
        let mut entries = self.entries.lock().map_err(|_| MoqReject::Unavailable)?;
        if entries.len() >= self.capacity {
            return Err(MoqReject::Capacity);
        }
        let id = uuid::Uuid::new_v4().to_string();
        let (mut lease, fence) = self.provider.admit(principal, scope, &id, self.ttl)?;
        lease.lease_id = id.clone();
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
            },
        );
        let weak = Arc::downgrade(self);
        let ttl = self.ttl;
        tokio::spawn(async move {
            tokio::time::sleep(ttl).await;
            if let Some(registry) = weak.upgrade() {
                if let Ok(mut entries) = registry.entries.lock() {
                    if let Some(entry) = entries.remove(&id) {
                        if let Some(fence) = entry.fence {
                            fence.revoke();
                        }
                    }
                }
            }
        });
        Ok(lease)
    }
    /// Fence only the owner, scope and exact binding generation.
    pub fn revoke(
        &self,
        principal: &Principal,
        scope: &WhepScope,
        id: &str,
        generation: u64,
    ) -> Result<MoqRevoked, MoqReject> {
        let mut entries = self.entries.lock().map_err(|_| MoqReject::Unavailable)?;
        let entry = entries.get_mut(id).ok_or(MoqReject::NotFound)?;
        if entry.owner != principal.key_id || entry.scope != scope.label() {
            return Err(MoqReject::Forbidden);
        }
        if entry.generation != generation {
            return Err(MoqReject::Generation);
        }
        if let Some(fence) = entry.fence.take() {
            fence.revoke();
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
                    url: "https://example.test".into(),
                    broadcast: "test".into(),
                    protocol: "moqt-16".into(),
                    video: MoqVideo {
                        codec: "vp8".into(),
                        coded_width: 320,
                        coded_height: 180,
                        description: None,
                    },
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
    async fn default_unavailable() {
        let r = Arc::new(MoqRegistry::default());
        assert!(!r.available());
        assert!(matches!(
            r.open(&principal("a"), &WhepScope::Program),
            Err(MoqReject::Unavailable)
        ));
    }
}
