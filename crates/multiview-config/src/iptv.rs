//! Managed IPTV playlist publication (`[iptv]`, epg-playlist design).
//!
//! An optional, **disabled-by-default** section declaring which outputs and
//! sources the server publishes in its own extended-M3U playlist
//! (`GET /iptv/playlist.m3u8`) and filtered XMLTV guide (`GET /iptv/guide.xml`).
//! Presentation only: nothing here changes media identity, ingest, decode or
//! output, and an absent section keeps every pre-existing document loading
//! byte-identically.
//!
//! Cross-reference validation (an entry's `ref` must name an existing output or
//! source) happens in [`crate::MultiviewConfig::validate`], so a desired
//! document can never publish a deleted resource.
use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

/// The maximum number of published entries (bounded playlist + guide work).
pub const MAX_IPTV_ENTRIES: usize = 512;

/// What a published playlist entry refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum IptvEntryKind {
    /// A configured HLS / LL-HLS output (program, preview, aux …), published
    /// at its public `/hls/{mount}/{playlist}` delivery URL.
    Output,
    /// A configured source (input), published at its **upstream** URL with any
    /// userinfo removed. Requires [`IptvEntry::allow_direct_source_url`].
    Source,
}

/// One published playlist entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct IptvEntry {
    /// Whether `ref` names an output or a source.
    pub kind: IptvEntryKind,
    /// The referenced output id or source id.
    #[serde(rename = "ref")]
    pub reference: String,
    /// Optional published `tvg-id` override (also the guide channel id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tvg_id: Option<String>,
    /// Optional display-name override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Optional `tvg-logo` override (credential-free http(s) URL).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo: Option<String>,
    /// Optional `group-title` override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Explicit operator permission to publish a **source's upstream URL**
    /// (never its stored credentials). Required for `kind = "source"`.
    #[serde(default, skip_serializing_if = "crate::iptv::is_false")]
    pub allow_direct_source_url: bool,
}

/// The `[iptv]` section.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct IptvConfig {
    /// Whether the playlist and guide are served. Default `false` (404).
    #[serde(default)]
    pub enabled: bool,
    /// Optional absolute public base URL (`https://mv.example.com`) used for
    /// playlist links. Absent ⇒ derived from the request `Host`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_base_url: Option<String>,
    /// Published entries, in playlist order.
    #[serde(default)]
    pub entries: Vec<IptvEntry>,
    /// Minutes between background guide refreshes while publication is
    /// enabled. Absent means [`DEFAULT_GUIDE_REFRESH_MINUTES`]; validated to
    /// [`MIN_GUIDE_REFRESH_MINUTES`]..=[`MAX_GUIDE_REFRESH_MINUTES`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guide_refresh_minutes: Option<u32>,
}

/// The default [`IptvConfig::guide_refresh_minutes`].
pub const DEFAULT_GUIDE_REFRESH_MINUTES: u32 = 30;
/// The shortest allowed guide refresh interval: guide publishers rate-limit,
/// and XMLTV rarely changes faster.
pub const MIN_GUIDE_REFRESH_MINUTES: u32 = 5;
/// The longest allowed guide refresh interval (one day), so a guide never
/// runs out inside its seven-day window.
pub const MAX_GUIDE_REFRESH_MINUTES: u32 = 24 * 60;

impl IptvConfig {
    /// The effective refresh interval in minutes, clamped to the valid range.
    #[must_use]
    pub fn guide_refresh_minutes(&self) -> u32 {
        self.guide_refresh_minutes
            .unwrap_or(DEFAULT_GUIDE_REFRESH_MINUTES)
            .clamp(MIN_GUIDE_REFRESH_MINUTES, MAX_GUIDE_REFRESH_MINUTES)
    }
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde skip_serializing_if signature
pub(crate) const fn is_false(value: &bool) -> bool {
    !*value
}

const TEXT_MAX: usize = 512;
const URL_MAX: usize = 2048;

fn text(field: &str, index: usize, value: &str) -> Result<(), ConfigError> {
    if value.trim().is_empty() || value.len() > TEXT_MAX || value.chars().any(char::is_control) {
        return Err(ConfigError::Validation(format!(
            "iptv.entries[{index}].{field} must be 1..={TEXT_MAX} printable bytes"
        )));
    }
    Ok(())
}

/// Validate a credential-free absolute http(s) URL.
fn http_url(field: &str, value: &str) -> Result<(), ConfigError> {
    let lower = value.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"));
    let authority = rest
        .and_then(|rest| rest.split(['/', '?', '#']).next())
        .unwrap_or("");
    if value.len() > URL_MAX
        || authority.is_empty()
        || authority.contains('@')
        || value.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(ConfigError::Validation(format!(
            "{field} must be a credential-free absolute http(s) URL of at most {URL_MAX} bytes"
        )));
    }
    Ok(())
}

impl IptvConfig {
    /// Structural validation plus — when supplied — reference resolution
    /// against the declared `source_ids` and the output **resource ids** in
    /// `outputs` (each id paired with whether it is an HLS/LL-HLS output, i.e.
    /// has a public playback URL). `None` skips output resolution (the config
    /// document alone does not carry the API's output identities).
    ///
    /// # Errors
    /// [`ConfigError::Validation`] naming the violated rule.
    pub fn validate(
        &self,
        source_ids: Option<&[&str]>,
        outputs: Option<&[(&str, bool)]>,
    ) -> Result<(), ConfigError> {
        if self.entries.len() > MAX_IPTV_ENTRIES {
            return Err(ConfigError::Validation(format!(
                "iptv.entries has {} entries, over the limit of {MAX_IPTV_ENTRIES}",
                self.entries.len()
            )));
        }
        if let Some(minutes) = self.guide_refresh_minutes {
            if !(MIN_GUIDE_REFRESH_MINUTES..=MAX_GUIDE_REFRESH_MINUTES).contains(&minutes) {
                return Err(ConfigError::Validation(format!(
                    "iptv.guide_refresh_minutes must be {MIN_GUIDE_REFRESH_MINUTES}..={MAX_GUIDE_REFRESH_MINUTES}, got {minutes}"
                )));
            }
        }
        if let Some(base) = &self.public_base_url {
            http_url("iptv.public_base_url", base)?;
            if base.contains(['?', '#']) {
                return Err(ConfigError::Validation(
                    "iptv.public_base_url must not carry a query or fragment".into(),
                ));
            }
        }
        let mut seen = std::collections::HashSet::new();
        for (index, entry) in self.entries.iter().enumerate() {
            text("ref", index, &entry.reference)?;
            if !seen.insert((entry.kind, entry.reference.as_str())) {
                return Err(ConfigError::Validation(format!(
                    "iptv.entries[{index}] duplicates {:?} {:?}",
                    entry.kind, entry.reference
                )));
            }
            for (field, value) in [
                ("tvg_id", &entry.tvg_id),
                ("name", &entry.name),
                ("group", &entry.group),
            ] {
                if let Some(value) = value {
                    text(field, index, value)?;
                    if value.contains('"') {
                        return Err(ConfigError::Validation(format!(
                            "iptv.entries[{index}].{field} must not contain a double quote"
                        )));
                    }
                }
            }
            if let Some(logo) = &entry.logo {
                http_url(&format!("iptv.entries[{index}].logo"), logo)?;
            }
            match entry.kind {
                IptvEntryKind::Source => {
                    if source_ids.is_some_and(|ids| !ids.contains(&entry.reference.as_str())) {
                        return Err(ConfigError::Validation(format!(
                            "iptv.entries[{index}] references unknown source {:?}",
                            entry.reference
                        )));
                    }
                    if !entry.allow_direct_source_url {
                        return Err(ConfigError::Validation(format!(
                            "iptv.entries[{index}] publishes source {:?}'s upstream URL; set \
                             allow_direct_source_url = true to permit it explicitly",
                            entry.reference
                        )));
                    }
                }
                IptvEntryKind::Output => {
                    let Some(outputs) = outputs else { continue };
                    match outputs.iter().find(|(id, _)| *id == entry.reference) {
                        None => {
                            return Err(ConfigError::Validation(format!(
                                "iptv.entries[{index}] references unknown output {:?}",
                                entry.reference
                            )));
                        }
                        Some((_, false)) => {
                            return Err(ConfigError::Validation(format!(
                                "iptv.entries[{index}]: output {:?} is not an HLS/LL-HLS output \
                                 and has no public playlist URL",
                                entry.reference
                            )));
                        }
                        Some((_, true)) => {}
                    }
                }
            }
        }
        Ok(())
    }
}
