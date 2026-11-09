//! Validated effective decoder descriptors. This module never acquires source demand.
/// Read a bounded measured startup request from a private, regular, non-symlink
/// file. Parsing uses the eventual config-owned serde type; this reader never
/// creates profiles, guesses measurements, or opens an encoder.
#[cfg(unix)]
pub fn read_measured_startup<T: serde::de::DeserializeOwned>(
    path: &std::path::Path,
) -> Result<T, multiview_control::moq::MoqReject> {
    use multiview_control::moq::MoqReject;
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(
            i32::try_from(rustix::fs::OFlags::NOFOLLOW.bits())
                .map_err(|_| MoqReject::Unavailable)?,
        )
        .open(path)
        .map_err(|_| MoqReject::Unavailable)?;
    let meta = file.metadata().map_err(|_| MoqReject::Unavailable)?;
    let uid = rustix::process::geteuid().as_raw();
    if meta.uid() != uid {
        return Err(MoqReject::Unavailable);
    }
    if !meta.is_file() || meta.permissions().mode() & 0o077 != 0 || meta.len() > 64 * 1024 {
        return Err(MoqReject::Unavailable);
    }
    let mut value = Vec::new();
    file.take(64 * 1024 + 1)
        .read_to_end(&mut value)
        .map_err(|_| MoqReject::Unavailable)?;
    if value.len() > 64 * 1024 {
        return Err(MoqReject::Unavailable);
    }
    serde_json::from_slice(&value).map_err(|_| MoqReject::Unavailable)
}

use super::{Timebase, TrackMapping};
use base64::{engine::general_purpose::STANDARD, Engine};
use multiview_control::{
    auth::{authorize_object, authorize_output, Action, Principal},
    moq::{MoqAudio, MoqReject, MoqVideo},
    preview::WhepScope,
};
use std::collections::{BTreeMap, BTreeSet};

/// Exact decoder metadata obtained from the active encoder, not requested config.
pub struct VideoDescriptor {
    /// `WebCodecs` codec string supplied by the actual encoder parameter exporter.
    pub codec: String,
    /// Out-of-band decoder initialization record (AVCC/HVCC), if applicable.
    pub description: Vec<u8>,
    /// Actual encoded pixel width.
    pub width: u32,
    /// Actual encoded pixel height.
    pub height: u32,
    /// Encoder PTS timebase.
    pub timebase: Timebase,
    /// Maximum reorder depth. Nonzero is unsupported by the current mapper.
    pub reorder_depth: u32,
}
/// Independent selected audio stream metadata.
pub struct AudioDescriptor {
    /// None means the mixed stream; Some identifies a discrete stream.
    pub index: Option<u16>,
    /// Exact broadcast track identity.
    pub track: String,
    /// `WebCodecs` codec string.
    pub codec: String,
    /// `AudioSpecificConfig` for AAC, or other exact decoder initialization bytes.
    pub description: Vec<u8>,
    /// Actual sample rate.
    pub sample_rate: u32,
    /// Actual channel count.
    pub channels: u32,
    /// Encoder PTS timebase.
    pub timebase: Timebase,
}
/// Validated wire metadata plus exact packet routing.
pub struct PreparedMetadata {
    /// HTTP lease video description, base64 encoded.
    pub video: MoqVideo,
    /// HTTP lease independent audio descriptions, base64 encoded.
    pub audio: Vec<MoqAudio>,
    /// Actual hang catalog, whose descriptions are hexadecimal, not base64.
    pub catalog: Vec<u8>,
    /// Packet stream/timebase mapping for the zero-copy converter.
    pub mapping: TrackMapping,
}
fn valid_timebase(base: Timebase) -> bool {
    base.numerator > 0 && base.denominator > 0
}
fn hex(bytes: &[u8]) -> String {
    {
        use std::fmt::Write;
        bytes.iter().fold(String::new(), |mut output, byte| {
            let _ = write!(output, "{byte:02x}");
            output
        })
    }
}
fn valid_avcc(bytes: &[u8]) -> bool {
    let Some(count) = bytes.get(5).map(|value| value & 31) else {
        return false;
    };
    if count == 0 {
        return false;
    }
    let mut cursor = 6;
    for _ in 0..count {
        if !take_nal(bytes, &mut cursor) {
            return false;
        }
    }
    let Some(count) = bytes.get(cursor).copied() else {
        return false;
    };
    cursor += 1;
    count > 0 && (0..count).all(|_| take_nal(bytes, &mut cursor))
}
fn take_nal(bytes: &[u8], cursor: &mut usize) -> bool {
    let Some(size) = bytes.get(*cursor..*cursor + 2) else {
        return false;
    };
    let [high, low] = size else {
        return false;
    };
    let size = usize::from(u16::from_be_bytes([*high, *low]));
    *cursor += 2;
    if size == 0 || bytes.get(*cursor..*cursor + size).is_none() {
        return false;
    }
    *cursor += size;
    true
}
fn valid_hvcc(bytes: &[u8]) -> bool {
    let Some(arrays) = bytes.get(22).copied() else {
        return false;
    };
    let mut cursor = 23;
    let mut required = BTreeSet::new();
    for _ in 0..arrays {
        let Some(header) = bytes.get(cursor..cursor + 3) else {
            return false;
        };
        let [kind, high, low] = header else {
            return false;
        };
        let kind = kind & 63;
        let count = u16::from_be_bytes([*high, *low]);
        cursor += 3;
        if count == 0 {
            return false;
        }
        for _ in 0..count {
            if !take_nal(bytes, &mut cursor) {
                return false;
            }
        }
        required.insert(kind);
    }
    [32, 33, 34].iter().all(|kind| required.contains(kind))
}
fn description(bytes: &[u8]) -> Option<String> {
    (!bytes.is_empty()).then(|| STANDARD.encode(bytes))
}

/// A resolved dependency axis. Unknown/new axes must not become source IDs implicitly.
pub enum Dependency<'a> {
    /// Ingest source identity.
    Source(&'a str),
    /// Effective output identity.
    Output(&'a str),
    /// Program identity, using the distinct program grant namespace.
    Program(&'a str),
    /// Mix/effect buses have no scoped authorization axis yet; require global access.
    MixEffect(&'a str),
}
/// Authorize typed dependencies without creating a source-demand lease.
/// # Errors
/// Returns forbidden for any denied or unsupported scoped axis.
pub fn authorize_dependencies(
    principal: &Principal,
    dependencies: &[Dependency<'_>],
) -> Result<(), MoqReject> {
    principal
        .role
        .require(Action::Write)
        .map_err(|_| MoqReject::Forbidden)?;
    for dependency in dependencies {
        match dependency {
            Dependency::Source(id) => {
                authorize_object(principal, id).map_err(|_| MoqReject::Forbidden)?;
            }
            Dependency::Output(id) => {
                authorize_output(principal, id).map_err(|_| MoqReject::Forbidden)?;
            }
            Dependency::Program(id) => multiview_control::auth::authorize_scope(
                principal,
                multiview_events::AuthzScope::Program(id),
            )
            .map_err(|_| MoqReject::Forbidden)?,
            Dependency::MixEffect(_) if principal.is_global() => {}
            Dependency::MixEffect(_) => return Err(MoqReject::Forbidden),
        }
    }
    Ok(())
}

/// Check the public resource and all resolved source dependencies before any demand lease.
/// Unknown input renditions must remain unavailable; this function does not create one.
/// # Errors
/// Returns a sanitized forbidden error for any denied role or resource axis.
pub fn authorize_binding(
    principal: &Principal,
    scope: &WhepScope,
    resolved_output: &str,
    source_dependencies: &[String],
) -> Result<(), MoqReject> {
    principal
        .role
        .require(Action::Write)
        .map_err(|_| MoqReject::Forbidden)?;
    match scope {
        WhepScope::Program => {
            if principal.scoped_object_ids.is_some()
                || principal.scoped_output_ids.is_some()
                || principal.scoped_discovery_domains.is_some()
            {
                return Err(MoqReject::Forbidden);
            }
        }
        WhepScope::Input(id) => {
            authorize_object(principal, id).map_err(|_| MoqReject::Forbidden)?;
        }
        WhepScope::Output(id) => {
            authorize_output(principal, id).map_err(|_| MoqReject::Forbidden)?;
        }
        _ => return Err(MoqReject::Forbidden),
    }
    authorize_output(principal, resolved_output).map_err(|_| MoqReject::Forbidden)?;
    for id in source_dependencies {
        authorize_object(principal, id).map_err(|_| MoqReject::Forbidden)?;
    }
    Ok(())
}

/// Produce mutually consistent lease metadata, packet mapping and parsed hang catalog.
/// Codec strings originate in the encoder export; this code never guesses HEVC profiles.
/// # Errors
/// Rejects missing configurations, malformed codec records, reordered video, duplicate
/// tracks, invalid dimensions/timebases or a catalog rejected by the pinned hang parser.
#[allow(clippy::too_many_lines)]
pub fn prepare(
    video: &VideoDescriptor,
    audio: &[AudioDescriptor],
) -> Result<PreparedMetadata, MoqReject> {
    if video.width == 0
        || video.height == 0
        || !valid_timebase(video.timebase)
        || video.reorder_depth != 0
        || audio.len() > 16
    {
        return Err(MoqReject::Codec);
    }
    let bytes = &video.description;
    if video.codec.starts_with("avc1.") {
        if bytes.len() < 7 || bytes.first() != Some(&1) || !valid_avcc(bytes) {
            return Err(MoqReject::Codec);
        }
        let expected = format!(
            "avc1.{:02x}{:02x}{:02x}",
            bytes.get(1).ok_or(MoqReject::Codec)?,
            bytes.get(2).ok_or(MoqReject::Codec)?,
            bytes.get(3).ok_or(MoqReject::Codec)?
        );
        if video.codec.to_ascii_lowercase() != expected {
            return Err(MoqReject::Codec);
        }
    } else if video.codec.starts_with("hvc1.") || video.codec.starts_with("hev1.") {
        if bytes.len() < 23 || bytes.first() != Some(&1) || !valid_hvcc(bytes) {
            return Err(MoqReject::Codec);
        }
    } else if video.codec != "vp8"
        && !video.codec.starts_with("vp09.")
        && !video.codec.starts_with("av01.")
    {
        return Err(MoqReject::Codec);
    }
    let wire_video = MoqVideo {
        codec: video.codec.clone(),
        coded_width: video.width,
        coded_height: video.height,
        description: description(bytes),
    };
    let mut video_json = serde_json::json!({"codec":video.codec,"codedWidth":video.width,"codedHeight":video.height});
    if !bytes.is_empty() {
        video_json
            .as_object_mut()
            .ok_or(MoqReject::Codec)?
            .insert("description".into(), hex(bytes).into());
    }
    let mut wire_audio = Vec::new();
    let mut audio_json = serde_json::Map::new();
    let mut mapping = TrackMapping {
        video: video.timebase,
        mixed_audio: None,
        discrete_audio: BTreeMap::new(),
    };
    let mut names = BTreeSet::new();
    for track in audio {
        if track.track.is_empty()
            || track.track == "video"
            || track.track == "catalog.json"
            || !names.insert(track.track.clone())
            || track.sample_rate == 0
            || track.channels == 0
            || !valid_timebase(track.timebase)
        {
            return Err(MoqReject::Codec);
        }
        if track.codec.starts_with("mp4a.40.") {
            let Some(&first) = track.description.first() else {
                return Err(MoqReject::Codec);
            };
            let Some(&second) = track.description.get(1) else {
                return Err(MoqReject::Codec);
            };
            let object = first >> 3;
            let frequency = ((first & 7) << 1) | (second >> 7);
            let channels = (second >> 3) & 15;
            let rates = [
                96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000,
                7350,
            ];
            // Explicit-frequency, program-config-element and HE extensions need a
            // complete parser; refuse them rather than guess decoder metadata.
            if object != 2
                || track.codec != "mp4a.40.2"
                || rates.get(usize::from(frequency)).copied() != Some(track.sample_rate)
                || channels == 0
                || u32::from(channels) != track.channels
            {
                return Err(MoqReject::Codec);
            }
        } else if track.codec != "opus" {
            return Err(MoqReject::Codec);
        }
        let selected = (track.track.clone(), track.timebase);
        if let Some(index) = track.index {
            if mapping.discrete_audio.insert(index, selected).is_some() {
                return Err(MoqReject::Codec);
            }
        } else if mapping.mixed_audio.replace(selected).is_some() {
            return Err(MoqReject::Codec);
        }
        wire_audio.push(MoqAudio {
            track: track.track.clone(),
            codec: track.codec.clone(),
            sample_rate: track.sample_rate,
            number_of_channels: track.channels,
            description: description(&track.description),
        });
        let mut config = serde_json::json!({"codec":track.codec,"sampleRate":track.sample_rate,"numberOfChannels":track.channels});
        if !track.description.is_empty() {
            config
                .as_object_mut()
                .ok_or(MoqReject::Codec)?
                .insert("description".into(), hex(&track.description).into());
        }
        audio_json.insert(track.track.clone(), config);
    }
    let catalog=serde_json::to_vec(&serde_json::json!({"video":{"renditions":{"video":video_json}},"audio":{"renditions":audio_json}})).map_err(|_|MoqReject::Codec)?;
    let _: hang::catalog::Catalog =
        hang::catalog::Catalog::from_slice(&catalog).map_err(|_| MoqReject::Codec)?;
    Ok(PreparedMetadata {
        video: wire_video,
        audio: wire_audio,
        catalog,
        mapping,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::indexing_slicing)]
    use super::*;
    fn video() -> VideoDescriptor {
        VideoDescriptor {
            codec: "avc1.42c01e".into(),
            description: vec![
                1, 0x42, 0xc0, 0x1e, 0xff, 0xe1, 0, 2, 0x67, 0x42, 1, 0, 2, 0x68, 0,
            ],
            width: 320,
            height: 180,
            timebase: Timebase {
                numerator: 1,
                denominator: 25,
            },
            reorder_depth: 0,
        }
    }
    fn audio() -> AudioDescriptor {
        AudioDescriptor {
            index: None,
            track: "audio".into(),
            codec: "mp4a.40.2".into(),
            description: vec![0x11, 0x90],
            sample_rate: 48000,
            channels: 2,
            timebase: Timebase {
                numerator: 1,
                denominator: 48000,
            },
        }
    }
    #[cfg(unix)]
    #[test]
    fn measured_startup_reader_rejects_unsafe_file_and_never_fabricates_config() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = std::env::temp_dir().join(format!("measured-startup-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("request.json");
        std::fs::write(&path, br#"{"measured":true}"#).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let value: serde_json::Value = read_measured_startup(&path).unwrap();
        assert_eq!(value["measured"], true);
        let link = dir.join("link.json");
        if !link.exists() {
            symlink(&path, &link).unwrap();
        }
        assert!(read_measured_startup::<serde_json::Value>(&link).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_measured_startup::<serde_json::Value>(&path).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(&path, vec![b' '; 64 * 1024 + 1]).unwrap();
        assert!(read_measured_startup::<serde_json::Value>(&path).is_err());
    }

    #[test]
    fn descriptions_match_lease_base64_and_catalog_hex() {
        let v = video();
        let a = audio();
        let prepared = prepare(&v, &[a]).expect("metadata");
        let catalog: serde_json::Value = serde_json::from_slice(&prepared.catalog).expect("json");
        assert_eq!(
            catalog["video"]["renditions"]["video"]["description"],
            hex(&v.description)
        );
        assert_eq!(
            STANDARD
                .decode(prepared.video.description.expect("base64"))
                .expect("bytes"),
            v.description
        );
        assert_eq!(
            catalog["audio"]["renditions"]["audio"]["description"],
            "1190"
        );
        assert_eq!(prepared.audio[0].sample_rate, 48000);
        assert_eq!(prepared.mapping.mixed_audio.expect("selected").0, "audio");
    }
    #[test]
    fn rejects_inconsistent_codec_config_and_reordered_video() {
        let mut v = video();
        v.reorder_depth = 1;
        assert!(matches!(prepare(&v, &[]), Err(MoqReject::Codec)));
        v = video();
        v.codec = "avc1.640028".into();
        assert!(matches!(prepare(&v, &[]), Err(MoqReject::Codec)));
        v = video();
        v.description.truncate(9);
        assert!(matches!(prepare(&v, &[]), Err(MoqReject::Codec)));
        let mut a = audio();
        a.sample_rate = 44100;
        assert!(matches!(prepare(&video(), &[a]), Err(MoqReject::Codec)));
    }
    #[test]
    fn hevc_initialization_arrays_are_required_not_guessed() {
        let mut v = video();
        v.codec = "hvc1.1.6.L93.B0".into();
        v.description = vec![0; 23];
        v.description[0] = 1;
        v.description[22] = 3;
        for kind in [32, 33, 34] {
            v.description
                .extend_from_slice(&[kind, 0, 1, 0, 2, kind << 1, 1]);
        }
        assert!(prepare(&v, &[]).is_ok());
        v.description.pop();
        assert!(matches!(prepare(&v, &[]), Err(MoqReject::Codec)));
    }
    #[test]
    fn typed_dependency_helper_enforces_write_role_and_global_bus_axis() {
        let mut p = Principal {
            key_id: "global".into(),
            role: multiview_control::Role::Viewer,
            scoped_object_ids: None,
            scoped_output_ids: None,
            scoped_discovery_domains: None,
        };
        for dependency in [
            Dependency::Source("s"),
            Dependency::Output("o"),
            Dependency::Program("p"),
            Dependency::MixEffect("bus"),
        ] {
            assert!(authorize_dependencies(&p, &[dependency]).is_err());
        }
        p.role = multiview_control::Role::Operator;
        assert!(authorize_dependencies(&p, &[Dependency::MixEffect("bus")]).is_ok());
        p.scoped_discovery_domains = Some(vec!["site".into()]);
        assert!(authorize_dependencies(&p, &[Dependency::MixEffect("bus")]).is_err());
    }
    #[test]
    fn requested_and_resolved_objects_are_authorized_before_demand() {
        let mut p = Principal {
            key_id: "key".into(),
            role: multiview_control::Role::Operator,
            scoped_object_ids: Some(vec!["source".into()]),
            scoped_output_ids: Some(vec!["requested".into(), "effective".into()]),
            scoped_discovery_domains: None,
        };
        let scope = WhepScope::Output("requested".into());
        assert!(authorize_binding(&p, &scope, "effective", &["source".into()]).is_ok());
        assert!(authorize_binding(&p, &scope, "other", &[]).is_err());
        assert!(authorize_binding(&p, &scope, "effective", &["secret-source".into()]).is_err());
        assert!(authorize_binding(&p, &WhepScope::Program, "effective", &[]).is_err());
        assert!(
            authorize_binding(&p, &WhepScope::Input("other".into()), "effective", &[]).is_err()
        );
        assert!(authorize_dependencies(&p, &[Dependency::Program("effective")]).is_err());
        assert!(authorize_dependencies(&p, &[Dependency::MixEffect("bus")]).is_err());
        p.scoped_output_ids
            .as_mut()
            .expect("scope")
            .push("program:program1".into());
        assert!(authorize_dependencies(
            &p,
            &[
                Dependency::Program("program1"),
                Dependency::Output("effective"),
                Dependency::Source("source")
            ]
        )
        .is_ok());
        assert!(authorize_dependencies(&p, &[Dependency::Output("program1")]).is_err());
        p.role = multiview_control::Role::Viewer;
        assert!(authorize_binding(&p, &scope, "effective", &[]).is_err());
    }
}

/// Explicit measurements and requested profiles. The engine constructor is the
/// validation boundary; unknown fields do not become silent capacity defaults.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeasuredPreviewConfig {
    parent_output: String,
    production: Vec<multiview_config::preview_ladder::ProductionReservation>,
    requests: Vec<multiview_config::preview_ladder::PreviewRequest>,
    capacity: Option<multiview_config::preview_ladder::ResourceCost>,
    measured: Vec<(
        multiview_config::preview_ladder::PreviewKey,
        multiview_config::preview_ladder::ResourceCost,
    )>,
    audio_once_bps: u64,
    overhead_bps: u64,
}
impl MeasuredPreviewConfig {
    /// Validate without opening any encoder or exposing internal parent metadata.
    pub fn into_startup(self) -> Option<crate::preview_vertical::PreviewStartup> {
        crate::preview_vertical::PreviewStartup::new(
            self.parent_output,
            self.production,
            self.requests,
            self.capacity,
            self.measured,
            self.audio_once_bps,
            self.overhead_bps,
        )
    }
}
/// Install only explicit measured configuration before the run starts.
pub fn configure_optional_preview(
    pipeline: &mut crate::pipeline::Pipeline,
) -> Result<(), multiview_control::moq::MoqReject> {
    let Some(path) = std::env::var("MULTIVIEW_MOQ_PREVIEW_CONFIG_FILE").ok() else {
        return Ok(());
    };
    #[cfg(unix)]
    let config: MeasuredPreviewConfig = read_measured_startup(std::path::Path::new(&path))?;
    #[cfg(not(unix))]
    let config: MeasuredPreviewConfig = {
        let _ = path;
        return Err(multiview_control::moq::MoqReject::Unavailable);
    };
    pipeline.set_optional_preview(
        config
            .into_startup()
            .ok_or(multiview_control::moq::MoqReject::Unavailable)?,
    );
    Ok(())
}

#[cfg(test)]
mod measured_config_tests {
    use super::MeasuredPreviewConfig;
    #[test]
    fn unknown_measured_config_fields_do_not_become_capacity_defaults() {
        let source = r#"{"parent_output":"out","production":[],"requests":[],"capacity":null,"measured":[],"audio_once_bps":0,"overhead_bps":0,"invented":1}"#;
        assert!(serde_json::from_str::<MeasuredPreviewConfig>(source).is_err());
    }
    #[test]
    fn absent_capacity_and_measurements_fail_engine_constructor() {
        let source = r#"{"parent_output":"out","production":[],"requests":[],"capacity":null,"measured":[],"audio_once_bps":0,"overhead_bps":0}"#;
        let config: MeasuredPreviewConfig = serde_json::from_str(source).unwrap();
        assert!(config.into_startup().is_none());
    }
    #[test]
    fn malformed_typed_cadence_is_not_interpreted_as_default() {
        let source = r#"{"profile_id":"p","key":{"program":"main","bus":"main","codec":"h264","encoder_name":"libx264","width":64,"height":64,"cadence":"invalid","bitrate_bps":100000,"codec_profile":null,"pixel_format":"yuv420p","color":"sdr-bt709-limited","gpu_pin":null,"codec_config":null}}"#;
        assert!(
            serde_json::from_str::<multiview_config::preview_ladder::PreviewRequest>(source)
                .is_err()
        );
    }
}
