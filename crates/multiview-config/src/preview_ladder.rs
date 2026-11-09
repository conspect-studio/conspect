//! Pure admission of a finite SHARED program preview ladder.
//!
//! Production encoders and their fixed cadence/resolution are immutable. A
//! catalog is returned only after a runtime opener atomically commits every
//! NEW optional encoder and effective registry binding; no per-viewer encode.
#![allow(missing_docs)] // The pure admission fields are described by the API contracts below.
use multiview_core::time::Rational;

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewKey {
    pub program: String,
    pub bus: String,
    pub codec: String,
    pub encoder_name: String,
    pub width: u32,
    pub height: u32,
    pub cadence: Rational,
    pub bitrate_bps: u64,
    /// Explicit opened profile constraint. `None` means caller has no profile
    /// constraint; the effective descriptor must still expose actual/unknown.
    pub codec_profile: Option<i32>,
    pub pixel_format: String,
    pub color: String,
    pub gpu_pin: Option<String>,
    pub codec_config: Option<Vec<u8>>,
}
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewRequest {
    pub profile_id: String,
    pub key: PreviewKey,
}
/// Resource units are supplied by a measured platform admission probe. CPU and
/// GPU units are opaque within one probe generation, never invented here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceCost {
    pub encoder_sessions: u64,
    pub cpu_units: u64,
    pub gpu_units: u64,
    pub memory_bytes: u64,
    pub uplink_bps: u64,
}
impl ResourceCost {
    fn checked_add(self, other: Self) -> Option<Self> {
        Some(Self {
            encoder_sessions: self.encoder_sessions.checked_add(other.encoder_sessions)?,
            cpu_units: self.cpu_units.checked_add(other.cpu_units)?,
            gpu_units: self.gpu_units.checked_add(other.gpu_units)?,
            memory_bytes: self.memory_bytes.checked_add(other.memory_bytes)?,
            uplink_bps: self.uplink_bps.checked_add(other.uplink_bps)?,
        })
    }
    fn within(self, budget: Self) -> bool {
        self.encoder_sessions <= budget.encoder_sessions
            && self.cpu_units <= budget.cpu_units
            && self.gpu_units <= budget.gpu_units
            && self.memory_bytes <= budget.memory_bytes
            && self.uplink_bps <= budget.uplink_bps
    }
}
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductionReservation {
    pub output_id: String,
    pub key: PreviewKey,
    pub cost: ResourceCost,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenedPreview {
    pub output_id: String,
    /// The requested constraints that this opened encoder was checked against.
    pub key: PreviewKey,
    pub cost: ResourceCost,
    /// The actual opened `FFmpeg` codec identity (not a configured alias).
    pub opened_codec: String,
    /// The actual opened profile/level; negative values mean unknown.
    pub opened_profile: i32,
    pub opened_level: i32,
    /// The actual libav pixel format identifier; negative means unknown.
    pub opened_pixel_format: i32,
    /// Exact opened extradata, if any; absent is not inferred from a request.
    pub opened_codec_config: Option<Vec<u8>>,
}
/// One staged optional encoder. Dropping it MUST release its own reservation;
/// this is the only place an actual runtime resource is staged.
pub trait PreviewLease {
    fn descriptor(&self) -> &OpenedPreview;
}
/// Opens exactly one optional encoder into an RAII lease. The returned lease
/// owns and releases its reservation if a later rung fails; production keys
/// are never passed to this opener.
pub trait PreviewOpener {
    type Lease: PreviewLease;
    /// The single RAII registry registration guard. Dropping it retires only
    /// this optional catalog, never production outputs.
    type Registration;
    fn open(&mut self, requested: &PreviewKey) -> Option<Self::Lease>;
    /// Publish verified optional bindings as one effective registry generation.
    /// The returned RAII guard must remain owned by `AdmittedLadder` for the
    /// catalog lifetime; `None` drops every staged encoder lease.
    fn publish(&mut self, staged: &[Self::Lease]) -> Option<Self::Registration>;
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedProfile {
    pub profile_id: String,
    pub output_id: String,
    pub key: PreviewKey,
    pub reused_production: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmissionError {
    InvalidProfile,
    TooManyProfiles,
    DuplicateProfile,
    UnknownCapacity,
    InsufficientCapacity,
    EncoderUnavailable,
    OpenedMismatch,
    UplinkOverflow,
}
/// Owns all optional leases for the catalog lifetime. Dropping this object
/// releases ONLY its new preview encoders; production is referenced, not owned.
pub struct AdmittedLadder<L: PreviewLease, R> {
    pub production: Vec<ProductionReservation>,
    pub profiles: Vec<AdmittedProfile>,
    pub total_cost: ResourceCost,
    // Rust drops fields in declaration order: retire registry subscribers and
    // stop optional delivery before dropping their opened encoder leases.
    registration: Option<R>,
    optional_leases: Vec<L>,
}
impl<L: PreviewLease, R> AdmittedLadder<L, R> {
    /// Transfer admitted runtime ownership without retiring the registration.
    pub fn into_runtime_parts(self) -> (Option<R>, Vec<L>) {
        (self.registration, self.optional_leases)
    }
    /// Count optional opened encoder leases retained by this catalog.
    #[must_use]
    pub fn optional_encoder_count(&self) -> usize {
        self.optional_leases.len()
    }
    /// An optional registry generation is active only while this RAII guard is
    /// owned by the admitted catalog.
    #[must_use]
    pub fn registered(&self) -> bool {
        self.registration.is_some()
    }
    /// Mutably borrow one opened optional encoder lease for the program's
    /// shared-frame fan-out; never create a lease per subscriber.
    pub fn optional_encoder_mut(&mut self, index: usize) -> Option<&mut L> {
        self.optional_leases.get_mut(index)
    }
}
pub const MAX_PREVIEW_PROFILES: usize = 3;

/// Reserve production first. Uplink is the checked sum of ALL simultaneously
/// published unique video bitrates + shared audio ONCE + supplied transport
/// overhead, distinct from session/CPU/GPU/memory costs. Existing exact keys
/// require no new encoder but still count their video bitrate once.
///
/// # Errors
/// Rejects invalid or duplicate profiles, unknown/exhausted measured capacity,
/// failed optional opens, and any opened descriptor mismatch.
#[allow(clippy::too_many_lines)] // Preflight and atomic publication belong to one transaction.
pub fn admit_ladder<O>(
    production: &[ProductionReservation],
    requests: &[PreviewRequest],
    capacity: Option<ResourceCost>,
    additional_cost: &[(PreviewKey, ResourceCost)],
    audio_once_bps: u64,
    overhead_bps: u64,
    opener: &mut O,
) -> Result<AdmittedLadder<O::Lease, O::Registration>, AdmissionError>
where
    O: PreviewOpener,
{
    if requests.len() > MAX_PREVIEW_PROFILES {
        return Err(AdmissionError::TooManyProfiles);
    }
    let capacity = capacity.ok_or(AdmissionError::UnknownCapacity)?;
    let mut used = ResourceCost::default();
    let mut unique: Vec<(PreviewKey, String)> = Vec::new();
    for p in production {
        if !valid(&p.key) || p.output_id.is_empty() {
            return Err(AdmissionError::InvalidProfile);
        }
        if !unique.iter().any(|(key, _)| key == &p.key) {
            used = used
                .checked_add(p.cost)
                .ok_or(AdmissionError::UplinkOverflow)?;
            unique.push((p.key.clone(), p.output_id.clone()));
        }
    }
    if !used.within(capacity) {
        return Err(AdmissionError::InsufficientCapacity);
    }
    let mut planned: Vec<(PreviewKey, ResourceCost)> = Vec::new();
    for request in requests {
        if request.profile_id.is_empty() || !valid(&request.key) {
            return Err(AdmissionError::InvalidProfile);
        }
        if requests
            .iter()
            .filter(|other| other.profile_id == request.profile_id)
            .count()
            > 1
        {
            return Err(AdmissionError::DuplicateProfile);
        }
        if unique.iter().any(|(key, _)| key == &request.key)
            || planned.iter().any(|(key, _)| key == &request.key)
        {
            continue;
        }
        let measured = additional_cost
            .iter()
            .find(|(key, _)| key == &request.key)
            .ok_or(AdmissionError::UnknownCapacity)?
            .1;
        used = used
            .checked_add(measured)
            .ok_or(AdmissionError::UplinkOverflow)?;
        if !used.within(capacity) {
            return Err(AdmissionError::InsufficientCapacity);
        }
        planned.push((request.key.clone(), measured));
    }
    let video_uplink = unique
        .iter()
        .map(|(key, _)| key)
        .chain(planned.iter().map(|(key, _)| key))
        .try_fold(0u64, |sum, key| sum.checked_add(key.bitrate_bps))
        .ok_or(AdmissionError::UplinkOverflow)?;
    let uplink = video_uplink
        .checked_add(audio_once_bps)
        .and_then(|sum| sum.checked_add(overhead_bps))
        .ok_or(AdmissionError::UplinkOverflow)?;
    if uplink > capacity.uplink_bps {
        return Err(AdmissionError::InsufficientCapacity);
    }
    // Preflight is complete. Open ONLY new optional keys. The leases own the
    // reservations; a failed second open or mismatched descriptor drops every
    // prior optional lease without touching any production handle.
    let mut staged: Vec<O::Lease> = Vec::with_capacity(planned.len());
    for (key, cost) in &planned {
        let lease = opener.open(key).ok_or(AdmissionError::EncoderUnavailable)?;
        let actual = lease.descriptor();
        if actual.key != *key || actual.cost != *cost || actual.output_id.is_empty() {
            return Err(AdmissionError::OpenedMismatch);
        }
        staged.push(lease);
    }
    // The catalog is built from opened descriptors, not desired config.
    for lease in &staged {
        let actual = lease.descriptor();
        unique.push((actual.key.clone(), actual.output_id.clone()));
    }
    let profiles = requests
        .iter()
        .map(|request| {
            let output_id = unique
                .iter()
                .find(|(key, _)| key == &request.key)
                .map(|(_, id)| id.clone())
                .ok_or(AdmissionError::OpenedMismatch)?;
            Ok(AdmittedProfile {
                profile_id: request.profile_id.clone(),
                output_id,
                key: request.key.clone(),
                reused_production: production.iter().any(|p| p.key == request.key),
            })
        })
        .collect::<Result<Vec<_>, AdmissionError>>()?;
    // Publish only after the complete admitted catalog resolves to actual
    // opened bindings. The opener commits atomically or returns None while
    // keeping all staged leases RAII-owned for rollback on this error path.
    let registration = if staged.is_empty() {
        None
    } else {
        Some(
            opener
                .publish(&staged)
                .ok_or(AdmissionError::EncoderUnavailable)?,
        )
    };
    used.uplink_bps = uplink;
    Ok(AdmittedLadder {
        production: production.to_vec(),
        profiles,
        total_cost: used,
        optional_leases: staged,
        registration,
    })
}
fn valid(key: &PreviewKey) -> bool {
    !key.program.is_empty()
        && !key.bus.is_empty()
        && !key.codec.is_empty()
        && !key.encoder_name.is_empty()
        && !key.pixel_format.is_empty()
        && !key.color.is_empty()
        && key.width > 0
        && key.height > 0
        && key.cadence.num > 0
        && key.cadence.den > 0
        && key.bitrate_bps > 0
}
