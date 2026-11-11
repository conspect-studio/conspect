//! Optional real WebTransport provider. No provider is constructed by default.
use crate::{
    auth::Principal,
    moq::{MoqAudio, MoqFence, MoqLease, MoqProvider, MoqReject, MoqVideo, MoqVideoAlternative},
    preview::WhepScope,
};
use multiview_moq::provider::{Feed, Fence, LadderFeed, Runtime};
use std::{sync::Arc, time::Duration};

/// One selectable rendition of an authorized ladder, exactly as the opened
/// encoder produced it — never a desired-configuration guess.
///
/// Typed alternatives are additive: [`AuthorizedFeed::video`] stays the selected
/// rendition for a single-rendition (back-compatible) feed, and this list carries
/// the same family so an ABR consumer can choose a rung without re-resolving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LadderRendition {
    /// Rendition identity inside the authorized family (the effective output id).
    pub track: String,
    /// `WebCodecs` codec identifier from the opened encoder.
    pub codec: String,
    /// Encoded width in pixels.
    pub coded_width: u32,
    /// Encoded height in pixels.
    pub coded_height: u32,
    /// Base64 decoder initialization bytes, when the encoder supplied them.
    pub description: Option<String>,
    /// The opened encoder's *actual* frames-per-second cadence as an exact
    /// `(numerator, denominator)`, or `None` when no validated source supplied
    /// one. An encoder **timebase is not a frame rate** (`1/90000` is a tick
    /// resolution, not 90000 fps), so a provider must never fill this from the
    /// timebase reciprocal: `None` is the honest value until the explicit
    /// opened-encoder cadence exists.
    pub cadence: Option<(u32, u32)>,
    /// The opened encoder's reported bitrate, or `None` when it reported none.
    pub bitrate_bps: Option<u64>,
}

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
    /// Every authorized rendition of the family, in registry order. Empty for a
    /// single-rendition feed; never a fabricated ladder when the effective
    /// registry admitted only one rendition.
    pub ladder: Vec<LadderRendition>,
    /// Source-demand guard retained through queued media shutdown.
    pub demand: Box<dyn Send + Sync>,
}

/// Authorized shared metadata and ownership from one actual family publisher.
pub struct AuthorizedSharedFeed {
    /// Admitted family metadata, demand guard and publishing budget.
    pub feed: LadderFeed,
    /// Back-compatible initial decoder configuration.
    pub video: MoqVideo,
    /// Independent audio tracks published once per family.
    pub audio: Vec<MoqAudio>,
    /// Authorized decoder configurations, one per published video track.
    pub alternatives: Vec<MoqVideoAlternative>,
    /// Family clock anchor, when established by the publisher.
    pub clock_wall_micros: Option<u64>,
}

/// Factory must be nonblocking; attach only to an existing shared rendition.
/// Its producer uses `sink.try_offer` exclusively, never retaining a raw sender.
/// Queue rejection requires a common discontinuity and matching RAP recovery.
pub trait AuthorizedFeedFactory: Send + Sync {
    /// True only when real authorized shared media is available.
    fn available(&self) -> bool;
    /// Return a shared-family binding only when this factory supports it.
    /// `None` chooses the legacy path; refusal never falls back.
    /// # Errors
    /// Returns the authorization, codec, capacity or lifecycle admission refusal.
    fn attach_shared(
        &self,
        _principal: &Principal,
        _scope: &WhepScope,
        _sink: Arc<Fence>,
    ) -> Result<Option<AuthorizedSharedFeed>, MoqReject> {
        Ok(None)
    }

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
        let (shared_fence, shared_windows) = Fence::ladder_channel(4);
        match self
            .factory
            .attach_shared(principal, scope, shared_fence.clone())
        {
            Ok(Some(mut shared)) => {
                shared.feed.windows = shared_windows;
                let Ok(admission) =
                    self.runtime
                        .admit_shared(shared.feed, shared_fence.clone(), ttl)
                else {
                    shared_fence.revoke();
                    return Err(MoqReject::Unavailable);
                };
                return Ok((
                    MoqLease {
                        lease_id: subscription_id.into(),
                        generation: admission.generation,
                        expires_at: String::new(),
                        expires_in_ms: 0,
                        url: admission.url,
                        broadcast: admission.broadcast,
                        protocol: admission.protocol,
                        video: shared.video,
                        audio: shared.audio,
                        video_alternatives: shared.alternatives,
                        clock_wall_micros: shared.clock_wall_micros,
                    },
                    Arc::new(TransportFence(shared_fence)),
                ));
            }
            Ok(None) => shared_fence.revoke(),
            Err(error) => {
                shared_fence.revoke();
                return Err(error);
            }
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
        let alternatives = feed
            .ladder
            .iter()
            .filter_map(wire_alternative)
            .take(1)
            .map(|mut rung| {
                rung.track = "video".into();
                rung
            })
            .collect();
        Ok((
            MoqLease {
                lease_id: subscription_id.into(),
                generation: admitted.generation,
                expires_at: String::new(),
                expires_in_ms: 0,
                url: admitted.url,
                broadcast: admitted.broadcast,
                protocol: admitted.protocol,
                video: feed.video,
                // Single-rendition publication uses the actual wire track `video`,
                // not the effective production output ID. Advertise only that
                // selected rendition; unrelated outputs are not ABR alternatives.
                video_alternatives: alternatives,
                audio: feed.audio,
                // The engine epoch is not available on this seam; a fabricated
                // zero would claim every rung starts at the Unix epoch.
                clock_wall_micros: None,
            },
            Arc::new(TransportFence(fence)),
        ))
    }
}

/// Map one typed ladder rung to the HTTP lease alternative.
///
/// A rung without a verified exact cadence and a positive verified bitrate is
/// omitted from the wire ladder: an unverifiable quality is dropped explicitly
/// (never fabricated, never `0`), and the back-compatible selected `video` still
/// describes the admitted rendition. `description` is already the base64 form
/// [`crate::moq::MoqVideo`] uses, never the Hang catalog's hexadecimal form.
fn wire_alternative(rendition: &LadderRendition) -> Option<MoqVideoAlternative> {
    let (cadence_numerator, cadence_denominator) = rendition.cadence?;
    let bitrate_bps = rendition.bitrate_bps?;
    if cadence_numerator == 0 || cadence_denominator == 0 || bitrate_bps == 0 {
        return None;
    }
    Some(MoqVideoAlternative {
        track: rendition.track.clone(),
        codec: rendition.codec.clone(),
        coded_width: rendition.coded_width,
        coded_height: rendition.coded_height,
        cadence_numerator,
        cadence_denominator,
        bitrate_bps,
        description: rendition.description.clone(),
    })
}
