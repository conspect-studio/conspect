//! A pure, bounded **extended-M3U** catalogue parser (epg-playlist M3U import).
//!
//! Parses `#EXTM3U` header attributes (`url-tvg` / `x-tvg-url`) and every
//! `#EXTINF` entry's `tvg-id`, `tvg-name`, `tvg-logo`, `group-title` and display
//! name (the text after the first unquoted comma), resolving relative entry
//! URLs against an optional base. No I/O: the caller fetches the bytes.
//!
//! Bounds: at most [`MAX_M3U_BYTES`] of input and [`MAX_M3U_ENTRIES`] entries;
//! each attribute value is truncated-rejected beyond [`MAX_FIELD_BYTES`].
//! An HLS **media** or **master** playlist (`#EXT-X-…` tags) is reported as
//! such rather than misread as a catalogue.
use serde::Serialize;

/// Maximum accepted catalogue size in bytes (2 MiB).
pub const MAX_M3U_BYTES: usize = 2 * 1024 * 1024;
/// Maximum number of entries returned.
pub const MAX_M3U_ENTRIES: usize = 5000;
/// Maximum bytes of any single attribute value, name or URL.
pub const MAX_FIELD_BYTES: usize = 2048;

/// A parse failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum M3uError {
    /// The input exceeds [`MAX_M3U_BYTES`].
    #[error("catalogue is larger than {MAX_M3U_BYTES} bytes")]
    TooLarge,
    /// The input exceeds [`MAX_M3U_ENTRIES`].
    #[error("catalogue has more than {MAX_M3U_ENTRIES} entries")]
    TooManyEntries,
    /// The input is not UTF-8.
    #[error("catalogue is not valid UTF-8")]
    NotUtf8,
    /// The input is an HLS media/master playlist, not a channel catalogue.
    #[error(
        "this is an HLS stream playlist, not a channel catalogue; add it as an HLS source directly"
    )]
    HlsPlaylist,
    /// The input has no `#EXTM3U` header and no entries.
    #[error("not an extended M3U catalogue (no #EXTM3U header and no entries)")]
    NotM3u,
}

/// One catalogue candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct M3uEntry {
    /// The display name (text after the `#EXTINF` comma), or the URL when absent.
    pub name: String,
    /// The absolute stream URL.
    pub url: String,
    /// `tvg-id`, if present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tvg_id: Option<String>,
    /// `tvg-name`, if present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tvg_name: Option<String>,
    /// `tvg-logo`, if present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tvg_logo: Option<String>,
    /// `group-title`, if present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

/// A parsed catalogue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct M3uCatalogue {
    /// The header's XMLTV guide URL(s) (`url-tvg` / `x-tvg-url`; a
    /// comma-separated value is split), in declaration order.
    pub url_tvg: Vec<String>,
    /// The entries, in file order.
    pub entries: Vec<M3uEntry>,
    /// Lines skipped because they were malformed (e.g. a URL without a usable
    /// scheme) — informational.
    pub skipped: usize,
}

/// Parse `key="value"` / `key=value` attribute pairs from an `#EXTINF` /
/// `#EXTM3U` attribute section. Returns the attributes and the byte index
/// where the display name starts (after the first comma outside quotes).
fn parse_attrs(section: &str) -> (Vec<(String, String)>, Option<usize>) {
    let bytes = section.as_bytes();
    let mut attrs = Vec::new();
    let mut i = 0;
    let len = bytes.len();
    while i < len {
        let Some(&b) = bytes.get(i) else { break };
        if b == b',' {
            return (attrs, Some(i + 1));
        }
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        // Read a key up to '=' / whitespace / ','.
        let key_start = i;
        while let Some(&c) = bytes.get(i) {
            if c == b'=' || c == b',' || c.is_ascii_whitespace() {
                break;
            }
            i += 1;
        }
        let key = section.get(key_start..i).unwrap_or("").to_ascii_lowercase();
        if bytes.get(i) != Some(&b'=') {
            // A bare token (e.g. the `-1` duration): skip it.
            continue;
        }
        i += 1; // '='
        let value = if let Some(&q @ (b'"' | b'\'')) = bytes.get(i) {
            let start = i + 1;
            let end = section
                .get(start..)
                .and_then(|rest| rest.find(char::from(q)))
                .map_or(len, |off| start + off);
            i = (end + 1).min(len);
            section.get(start..end).unwrap_or("")
        } else {
            let start = i;
            while let Some(&c) = bytes.get(i) {
                if c == b',' || c.is_ascii_whitespace() {
                    break;
                }
                i += 1;
            }
            section.get(start..i).unwrap_or("")
        };
        if !key.is_empty() {
            attrs.push((key, value.to_owned()));
        }
    }
    (attrs, None)
}

fn bounded(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()
        && trimmed.len() <= MAX_FIELD_BYTES
        && !trimmed.chars().any(char::is_control))
    .then(|| trimmed.to_owned())
}

/// Whether `url` is an absolute URL with a scheme (`scheme://…`).
fn has_scheme(url: &str) -> bool {
    url.split_once("://").is_some_and(|(scheme, rest)| {
        !scheme.is_empty()
            && !rest.is_empty()
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    })
}

/// Resolve a (possibly relative) reference against an absolute `base` URL.
/// Supports scheme-relative (`//host/x`), absolute-path (`/x`) and
/// path-relative (`x`, `../x`) references; query/fragment of the base are dropped.
#[must_use]
pub fn resolve_url(base: Option<&str>, reference: &str) -> Option<String> {
    if has_scheme(reference) {
        return Some(reference.to_owned());
    }
    let base = base?;
    let (scheme, rest) = base.split_once("://")?;
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = rest.get(..authority_end)?;
    if authority.is_empty() {
        return None;
    }
    if let Some(stripped) = reference.strip_prefix("//") {
        return Some(format!("{scheme}://{stripped}"));
    }
    let path_part = rest.get(authority_end..).unwrap_or("");
    let path = path_part.split(['?', '#']).next().unwrap_or("");
    let (ref_path, ref_tail) = match reference.find(['?', '#']) {
        Some(at) => (reference.get(..at)?, reference.get(at..)?),
        None => (reference, ""),
    };
    let mut segments: Vec<&str> = if ref_path.starts_with('/') {
        Vec::new()
    } else {
        let mut dir: Vec<&str> = path.split('/').skip(1).collect();
        dir.pop(); // drop the base file name
        dir
    };
    for segment in ref_path.trim_start_matches('/').split('/') {
        match segment {
            "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    Some(format!(
        "{scheme}://{authority}/{}{ref_tail}",
        segments.join("/")
    ))
}

/// Parse an extended-M3U catalogue. `base` resolves relative entry URLs
/// (typically the URL the catalogue was fetched from).
///
/// # Errors
/// [`M3uError`] for oversize input, too many entries, non-UTF-8 input, an HLS
/// stream playlist, or input that is not M3U at all.
pub fn parse_m3u(input: &[u8], base: Option<&str>) -> Result<M3uCatalogue, M3uError> {
    if input.len() > MAX_M3U_BYTES {
        return Err(M3uError::TooLarge);
    }
    let text = std::str::from_utf8(input).map_err(|_| M3uError::NotUtf8)?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut catalogue = M3uCatalogue::default();
    let mut header = false;
    let mut pending: Option<(Vec<(String, String)>, String)> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXTM3U") {
            header = true;
            let (attrs, _) = parse_attrs(rest);
            for (key, value) in attrs {
                if key == "url-tvg" || key == "x-tvg-url" {
                    for url in value.split(',') {
                        if let Some(url) = bounded(url) {
                            if has_scheme(&url) && !catalogue.url_tvg.contains(&url) {
                                catalogue.url_tvg.push(url);
                            }
                        }
                    }
                }
            }
            continue;
        }
        if line.starts_with("#EXT-X-") {
            return Err(M3uError::HlsPlaylist);
        }
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            let (attrs, name_at) = parse_attrs(rest);
            let name = name_at
                .and_then(|at| rest.get(at..))
                .map_or("", str::trim)
                .to_owned();
            pending = Some((attrs, name));
            continue;
        }
        if line.starts_with('#') {
            continue; // comments and unsupported directives (#EXTGRP, #EXTVLCOPT …)
        }
        // A URL line.
        let Some((attrs, name)) = pending.take() else {
            // Plain (non-extended) M3U line: accept it with no metadata.
            if let Some(url) = bounded(line).and_then(|l| resolve_url(base, &l)) {
                push(&mut catalogue, &[], "", url)?;
            } else {
                catalogue.skipped += 1;
            }
            continue;
        };
        match bounded(line).and_then(|l| resolve_url(base, &l)) {
            Some(url) if url.len() <= MAX_FIELD_BYTES => push(&mut catalogue, &attrs, &name, url)?,
            _ => catalogue.skipped += 1,
        }
    }
    if !header && catalogue.entries.is_empty() {
        return Err(M3uError::NotM3u);
    }
    Ok(catalogue)
}

fn push(
    catalogue: &mut M3uCatalogue,
    attrs: &[(String, String)],
    name: &str,
    url: String,
) -> Result<(), M3uError> {
    if catalogue.entries.len() >= MAX_M3U_ENTRIES {
        return Err(M3uError::TooManyEntries);
    }
    let get = |key: &str| {
        attrs
            .iter()
            .find(|(k, _)| k == key)
            .and_then(|(_, v)| bounded(v))
    };
    let tvg_name = get("tvg-name");
    let name = bounded(name)
        .or_else(|| tvg_name.clone())
        .unwrap_or_else(|| url.clone());
    catalogue.entries.push(M3uEntry {
        name,
        tvg_id: get("tvg-id"),
        tvg_name,
        tvg_logo: get("tvg-logo"),
        group: get("group-title"),
        url,
    });
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn quoted_attrs_and_commas_in_names() {
        let text = "#EXTM3U url-tvg=\"https://epg.example/a.xml\"\n\
            #EXTINF:-1 tvg-id=\"CNN.us\" tvg-name=\"CNN, Intl\" tvg-logo=\"https://l/x.png\" group-title=\"US News\",CNN US, backup (1080p)\n\
            https://cdn.example/cnn/index.m3u8\n";
        let cat = parse_m3u(text.as_bytes(), None).unwrap();
        assert_eq!(cat.url_tvg, vec!["https://epg.example/a.xml"]);
        let e = &cat.entries[0];
        assert_eq!(e.name, "CNN US, backup (1080p)");
        assert_eq!(e.tvg_id.as_deref(), Some("CNN.us"));
        assert_eq!(e.tvg_name.as_deref(), Some("CNN, Intl"));
        assert_eq!(e.tvg_logo.as_deref(), Some("https://l/x.png"));
        assert_eq!(e.group.as_deref(), Some("US News"));
        assert_eq!(e.url, "https://cdn.example/cnn/index.m3u8");
    }

    #[test]
    fn url_tvg_variants() {
        for header in [
            "#EXTM3U x-tvg-url=\"https://a/g.xml\"",
            "#EXTM3U url-tvg='https://a/g.xml'",
            "#EXTM3U URL-TVG=https://a/g.xml",
        ] {
            let cat = parse_m3u(header.as_bytes(), None).unwrap();
            assert_eq!(cat.url_tvg, vec!["https://a/g.xml"], "{header}");
        }
        let cat = parse_m3u(
            b"#EXTM3U url-tvg=\"https://a/1.xml,https://b/2.xml.gz\" x-tvg-url=\"https://a/1.xml\"",
            None,
        )
        .unwrap();
        assert_eq!(cat.url_tvg, vec!["https://a/1.xml", "https://b/2.xml.gz"]);
    }

    #[test]
    fn relative_urls_resolve_against_the_base() {
        let text = "#EXTM3U\n#EXTINF:-1,A\nlive/a.m3u8\n#EXTINF:-1,B\n/root/b.m3u8\n#EXTINF:-1,C\n../c.m3u8?x=1\n#EXTINF:-1,D\n//other/d.m3u8\n";
        let cat = parse_m3u(
            text.as_bytes(),
            Some("https://h.example/lists/all.m3u?tok=1"),
        )
        .unwrap();
        let urls: Vec<_> = cat.entries.iter().map(|e| e.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://h.example/lists/live/a.m3u8",
                "https://h.example/root/b.m3u8",
                "https://h.example/c.m3u8?x=1",
                "https://other/d.m3u8",
            ]
        );
        // Without a base a relative URL cannot be used.
        let cat = parse_m3u(text.as_bytes(), None).unwrap();
        assert_eq!(cat.entries.len(), 0);
        assert_eq!(cat.skipped, 4);
    }

    #[test]
    fn limits_are_enforced() {
        let big = vec![b'#'; MAX_M3U_BYTES + 1];
        assert_eq!(parse_m3u(&big, None), Err(M3uError::TooLarge));
        let mut text = String::from("#EXTM3U\n");
        for i in 0..=MAX_M3U_ENTRIES {
            text += &[
                "#EXTINF:-1,c",
                &i.to_string(),
                "\nhttp://h/",
                &i.to_string(),
                "\n",
            ]
            .concat();
        }
        assert_eq!(
            parse_m3u(text.as_bytes(), None),
            Err(M3uError::TooManyEntries)
        );
        assert_eq!(parse_m3u(&[0xff, 0xfe], None), Err(M3uError::NotUtf8));
    }

    #[test]
    fn hls_stream_playlists_and_garbage_are_rejected() {
        let hls = "#EXTM3U\n#EXT-X-VERSION:3\n#EXTINF:6.0,\nseg1.ts\n";
        assert_eq!(parse_m3u(hls.as_bytes(), None), Err(M3uError::HlsPlaylist));
        assert_eq!(parse_m3u(b"<html>nope</html>", None), Err(M3uError::NotM3u));
        assert_eq!(parse_m3u(b"", None), Err(M3uError::NotM3u));
    }

    #[test]
    fn name_falls_back_to_tvg_name_then_url() {
        let text = "#EXTM3U\n#EXTINF:-1 tvg-name=\"Named\",\nhttp://h/a\n#EXTINF:-1,\nhttp://h/b\n";
        let cat = parse_m3u(text.as_bytes(), None).unwrap();
        assert_eq!(cat.entries[0].name, "Named");
        assert_eq!(cat.entries[1].name, "http://h/b");
    }

    #[test]
    fn gist_shaped_fixture_parses() {
        let text = include_str!("../../tests/fixtures/news-catalogue.m3u8");
        let cat = parse_m3u(text.as_bytes(), None).unwrap();
        assert_eq!(cat.url_tvg, vec!["https://epg.example.test/guide.xml.gz"]);
        assert_eq!(cat.entries.len(), 4);
        assert_eq!(cat.entries[0].tvg_id.as_deref(), Some("CNN.us"));
        assert_eq!(cat.entries[0].group.as_deref(), Some("US News"));
        assert_eq!(cat.entries[0].name, "CNN US (1080p)");
        // An entry with no tvg-logo (the gist's "official preview" row).
        assert_eq!(cat.entries[2].tvg_logo, None);
        assert_eq!(cat.entries[3].name, "Example — em dash, comma (720p)");
        // Commented-out appendix URLs are not entries.
        assert!(cat.entries.iter().all(|e| !e.url.starts_with("srt://")));
    }
}
