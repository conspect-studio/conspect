//! The engine-internal clip request (design section 6): what a runtime load
//! carries from the engine drain to the player. Pure data, no libav.

use std::path::PathBuf;

use super::LoadThen;

/// Opaque immutable reference identity. Never contains a filesystem path or URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotReference {
    /// A durable library object.
    Asset {
        /// Stable library object ID.
        id: String,
    },
    /// One immutable completed artifact version, not a mutable fill location.
    Artifact {
        /// Opaque completed artifact ID.
        id: String,
        /// Immutable artifact version, distinct from the slot generation.
        generation: u64,
    },
}

/// Authoritative committed binding, distinct from a pending load request.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SlotSnapshot {
    /// Monotonic runtime binding generation; zero means no commit yet.
    pub generation: super::Generation,
    /// None means unbound or a legacy path-only clip; no path is disclosed.
    pub reference: Option<SlotReference>,
}

/// A request to load one clip into a player slot.
///
/// In/out and vamp points are optional integer frames; the defaults come from
/// the probe (design 4.2). Paths only: there are no URLs on this path.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ClipSpec {
    /// The asset id this clip was resolved from, when it came from the library.
    pub asset_id: Option<String>,
    /// Trusted resolved immutable origin. Artifact pins remain the caller's duty.
    /// Required for journalled loads: a missing identity refuses precommit and
    /// preserves the current slot, with a redacted typed runtime fault.
    pub reference: Option<SlotReference>,
    /// Owned immutable artifact capability, retained through native retirement.
    pub artifact_lease: Option<ImmutableArtifactLease>,
    /// The file to open.
    pub path: PathBuf,
    /// Declared clip in-point (frames).
    pub in_point: Option<u64>,
    /// Declared clip out-point, exclusive (frames).
    pub out_point: Option<u64>,
    /// Declared vamp-segment start (frames).
    pub vamp_in: Option<u64>,
    /// Declared vamp-segment end, exclusive (frames).
    pub vamp_out: Option<u64>,
    /// What the player lands on at commit.
    pub then: LoadThen,
}

impl ClipSpec {
    /// A spec for `path` with every point defaulted from the probe.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, then: LoadThen) -> Self {
        Self {
            asset_id: None,
            reference: None,
            artifact_lease: None,
            path: path.into(),
            in_point: None,
            out_point: None,
            vamp_in: None,
            vamp_out: None,
            then,
        }
    }
}

/// Shared ownership of one verified immutable local artifact, never a path fallback.
#[derive(Clone)]
pub struct ImmutableArtifactLease(std::sync::Arc<multiview_webengine::artifact::Lease>);
impl std::fmt::Debug for ImmutableArtifactLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ImmutableArtifactLease")
            .field(&self.0.artifact().id)
            .finish()
    }
}
impl PartialEq for ImmutableArtifactLease {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for ImmutableArtifactLease {}
impl ClipSpec {
    /// Resolve a verified artifact to a local descriptor path, with its owner.
    #[cfg(target_os = "linux")]
    #[must_use]
    pub fn from_artifact(lease: multiview_webengine::artifact::Lease, then: LoadThen) -> Self {
        let mut spec = Self::new(lease.local_read_path(), then);
        spec.reference = Some(SlotReference::Artifact {
            id: lease.artifact().id.as_str().into(),
            generation: lease.artifact().provenance.generation,
        });
        spec.artifact_lease = Some(ImmutableArtifactLease(std::sync::Arc::new(lease)));
        spec
    }
}

#[cfg(all(test, target_os = "linux"))]
mod artifact_tests {
    use super::*;
    use std::io::Write;
    #[test]
    fn artifact_spec_retains_pin_and_reads_original_descriptor() {
        use multiview_webengine::artifact::{ArtifactStore, Limits, Provenance};
        let root = tempfile::tempdir().unwrap();
        let mut store = ArtifactStore::open(
            root.path(),
            Limits {
                bytes: 100_000,
                count: 100,
            },
        )
        .unwrap();
        let mut writer = store
            .begin(
                Provenance {
                    owner: "owner".into(),
                    fill: "fill".into(),
                    job: "job".into(),
                    generation: 7,
                },
                5,
                |_| true,
            )
            .unwrap();
        writer.write_all(b"media").unwrap();
        let artifact = writer.complete().unwrap();
        let lease = store.pin(&artifact.id, "owner", |_| true).unwrap();
        let spec = ClipSpec::from_artifact(lease, LoadThen::Cue);
        let staged_owner = spec.artifact_lease.clone();
        assert_eq!(std::fs::read(&spec.path).unwrap(), b"media");
        assert!(store.remove(&artifact.id, "owner", |_| true).is_err());
        drop(spec);
        assert!(store.remove(&artifact.id, "owner", |_| true).is_err());
        drop(staged_owner);
        store.remove(&artifact.id, "owner", |_| true).unwrap();
    }
}
