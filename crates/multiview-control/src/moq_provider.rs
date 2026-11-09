//! Optional real WebTransport provider. No provider is constructed by default.
use crate::{
    auth::Principal,
    moq::{MoqAudio, MoqFence, MoqLease, MoqProvider, MoqReject, MoqVideo},
    preview::WhepScope,
};
use multiview_moq::provider::{Feed, Fence, Runtime};
use std::{sync::Arc, time::Duration};

/// Authorized immutable binding metadata and RAII source-demand ownership.
/// Construction must authorize the scope AND every resolved bus/source/output.
pub struct AuthorizedFeed {
    /// Immutable binding/configuration generation.
    pub generation: u64,
    /// Serialized hang catalog for exactly the admitted tracks.
    pub catalog: Vec<u8>,
    /// Video decoder configuration matching catalog.
    pub video: MoqVideo,
    /// Independent audio decoder configurations matching catalog.
    pub audio: Vec<MoqAudio>,
    /// Source-demand guard retained through queued media shutdown.
    pub demand: Box<dyn Send + Sync>,
}

/// Factory must be nonblocking; attach only to an existing shared rendition.
/// Its producer uses `sink.try_offer` exclusively, never retaining a raw sender.
/// Queue rejection requires a common discontinuity and matching RAP recovery.
pub trait AuthorizedFeedFactory: Send + Sync {
    /// True only when real authorized shared media is available.
    fn available(&self) -> bool;
    /// Check all resolved-object permissions before acquiring source demand.
    ///
    /// # Errors
    /// Returns a rejection when authorization, rendition availability, codec
    /// support, or bounded subscription admission prevents attaching the feed.
    fn attach(
        &self,
        principal: &Principal,
        scope: &WhepScope,
        sink: Arc<Fence>,
    ) -> Result<AuthorizedFeed, MoqReject>;
}

struct TransportFence(Arc<Fence>);
impl MoqFence for TransportFence {
    fn revoke(&self) {
        self.0.revoke();
    }
}

/// Actual token-admitted listener plus injectable authorized shared-feed owner.
pub struct WebTransportProvider {
    runtime: Arc<Runtime>,
    factory: Arc<dyn AuthorizedFeedFactory>,
}
impl WebTransportProvider {
    /// The listener must already be bound with one explicit draft16/18 version.
    pub fn new(runtime: Arc<Runtime>, factory: Arc<dyn AuthorizedFeedFactory>) -> Self {
        Self { runtime, factory }
    }
}
impl MoqProvider for WebTransportProvider {
    fn available(&self) -> bool {
        self.runtime.available() && self.factory.available()
    }
    fn admit(
        &self,
        principal: &Principal,
        scope: &WhepScope,
        subscription_id: &str,
        ttl: Duration,
    ) -> Result<(MoqLease, Arc<dyn MoqFence>), MoqReject> {
        if !self.available() {
            return Err(MoqReject::Unavailable);
        }
        let (fence, windows) = Fence::channel(4);
        let feed = match self.factory.attach(principal, scope, fence.clone()) {
            Ok(feed) => feed,
            Err(error) => {
                fence.revoke();
                return Err(error);
            }
        };
        let transport = Feed {
            generation: feed.generation,
            catalog: feed.catalog.into(),
            audio_tracks: feed.audio.iter().map(|track| track.track.clone()).collect(),
            windows,
            demand: feed.demand,
        };
        let Ok(admitted) = self.runtime.admit(transport, fence.clone(), ttl) else {
            fence.revoke();
            return Err(MoqReject::Unavailable);
        };
        Ok((
            MoqLease {
                lease_id: subscription_id.into(),
                generation: admitted.generation,
                expires_at: String::new(),
                url: admitted.url,
                broadcast: admitted.broadcast,
                protocol: admitted.protocol,
                video: feed.video,
                audio: feed.audio,
            },
            Arc::new(TransportFence(fence)),
        ))
    }
}
