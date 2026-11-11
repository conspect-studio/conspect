//! Bounded **streaming** XMLTV filter (epg-playlist guide passthrough).
//!
//! Reads an upstream XMLTV document event by event (never a document tree),
//! keeps only the `<channel>` / `<programme>` elements of the wanted provider
//! channel ids, rewrites their ids to the published `tvg-id`(s), and drops
//! programmes outside the guide window. DTD/`DOCTYPE` declarations and any
//! non-predefined entity reference are rejected (no entity expansion attack
//! surface). Every dimension is capped ([`FilterLimits`]).
//!
//! Pure and synchronous: the refresher runs it on a blocking thread over a
//! spooled temp file, so neither the async runtime nor the engine waits on it.
use std::collections::{HashMap, HashSet};
use std::io::BufRead;

use quick_xml::events::{BytesEnd, BytesStart, Event};
use quick_xml::{Reader, Writer};

/// Filter resource caps.
#[derive(Debug, Clone, Copy)]
pub struct FilterLimits {
    /// Maximum element nesting depth.
    pub max_depth: usize,
    /// Maximum kept `<channel>` elements.
    pub max_channels: usize,
    /// Maximum kept `<programme>` elements.
    pub max_programmes: usize,
    /// Maximum filtered output bytes.
    pub max_output_bytes: usize,
    /// Maximum bytes of one kept element (channel/programme subtree).
    pub max_element_bytes: usize,
}

impl Default for FilterLimits {
    fn default() -> Self {
        Self {
            max_depth: 32,
            max_channels: 4096,
            max_programmes: 200_000,
            max_output_bytes: 64 * 1024 * 1024,
            max_element_bytes: 256 * 1024,
        }
    }
}

/// The schedule window kept (unix seconds): a programme is kept when it ends
/// after `from` and starts before `until`.
#[derive(Debug, Clone, Copy)]
pub struct Window {
    /// Window start (usually "now").
    pub from: i64,
    /// Window end (usually now + 7 days).
    pub until: i64,
}

impl Window {
    /// The default 7-day guide window starting at `now`.
    #[must_use]
    pub const fn seven_days_from(now: i64) -> Self {
        Self {
            from: now,
            until: now + 7 * 24 * 3600,
        }
    }
}

/// A filter failure (the feed is rejected as a whole; the last good stays).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FilterError {
    /// A `DOCTYPE` / DTD declaration was present.
    #[error("XMLTV feed declares a DOCTYPE/DTD, which is rejected")]
    Doctype,
    /// A non-predefined entity reference was used.
    #[error("XMLTV feed uses an undeclared entity reference")]
    Entity,
    /// The document is not well-formed or not XMLTV.
    #[error("malformed XMLTV: {0}")]
    Malformed(String),
    /// A resource cap was hit.
    #[error("XMLTV limit exceeded: {0}")]
    Limit(&'static str),
}

/// Kept, id-rewritten XML for one **published** channel id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TargetFragment {
    /// The serialized `<channel>` element (empty when the feed had none).
    pub channel: Vec<u8>,
    /// Serialized `<programme>` elements, in feed order.
    pub programmes: Vec<u8>,
    /// Number of programmes in [`programmes`](Self::programmes).
    pub programme_count: usize,
}

/// The filtered result for one feed, keyed by published channel id, so a
/// guide can be re-assembled for the current publication without refetching
/// (a removed entry simply stops being assembled).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fragment {
    /// Per published id.
    pub targets: HashMap<String, TargetFragment>,
    /// Number of kept channels.
    pub channel_count: usize,
    /// Number of kept programmes.
    pub programme_count: usize,
    /// Total kept bytes.
    pub bytes: usize,
}

fn malformed(e: impl std::fmt::Display) -> FilterError {
    FilterError::Malformed(e.to_string())
}

/// Parse an XMLTV timestamp (`YYYYMMDDhhmmss[ ±hhmm]`, seconds/zone optional)
/// into unix seconds. Returns `None` when unparseable.
#[must_use]
pub fn parse_xmltv_time(value: &str) -> Option<i64> {
    let value = value.trim();
    let (digits, zone) = match value.find([' ', '+', '-']) {
        Some(at) => (value.get(..at)?, value.get(at..)?.trim()),
        None => (value, ""),
    };
    if digits.len() < 12 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let num = |range: std::ops::Range<usize>| digits.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (num(0..4)?, num(4..6)?, num(6..8)?);
    let (hour, minute) = (num(8..10)?, num(10..12)?);
    let second = if digits.len() >= 14 { num(12..14)? } else { 0 };
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let offset = if zone.is_empty() {
        0
    } else {
        let sign = match zone.as_bytes().first()? {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        let z = zone.get(1..)?;
        if z.len() != 4 || !z.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let hh = z.get(0..2)?.parse::<i64>().ok()?;
        let mm = z.get(2..4)?.parse::<i64>().ok()?;
        sign * (hh * 3600 + mm * 60)
    };
    // Howard Hinnant's days-from-civil.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hour * 3600 + minute * 60 + second - offset)
}

/// Copy `start` with attribute `key` replaced by `value` (others kept raw).
fn rewrite_start(
    start: &BytesStart<'_>,
    key: &[u8],
    value: &str,
) -> Result<BytesStart<'static>, FilterError> {
    let name = std::str::from_utf8(start.name().as_ref())
        .map_err(malformed)?
        .to_owned();
    let mut out = BytesStart::new(name);
    for attr in start.attributes() {
        let attr = attr.map_err(malformed)?;
        if attr.key.as_ref() == key {
            out.push_attribute((std::str::from_utf8(key).map_err(malformed)?, value));
        } else {
            out.push_attribute(attr);
        }
    }
    Ok(out)
}

fn attr_value(start: &BytesStart<'_>, key: &[u8]) -> Result<Option<String>, FilterError> {
    for attr in start.attributes() {
        let attr = attr.map_err(malformed)?;
        if attr.key.as_ref() == key {
            return attr
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .map(|v| Some(v.into_owned()))
                .map_err(malformed);
        }
    }
    Ok(None)
}

/// Whether a reference event names a predefined entity or a character ref.
fn allowed_ref(name: &[u8]) -> bool {
    matches!(name, b"amp" | b"lt" | b"gt" | b"apos" | b"quot")
        || (name.first() == Some(&b'#') && name.len() > 1)
}

/// Stream-filter an XMLTV document.
///
/// `wanted` maps a **provider** channel id to the published id(s) it is
/// rewritten to. Channels already present in `already` are not re-emitted.
///
/// # Errors
/// [`FilterError`] for a DTD, an undeclared entity, malformed input or a cap.
#[allow(clippy::too_many_lines)] // one linear event loop; splitting hides the state machine
pub fn filter_xmltv<R: BufRead, S: std::hash::BuildHasher>(
    input: R,
    wanted: &HashMap<String, Vec<String>, S>,
    window: Window,
    limits: FilterLimits,
) -> Result<Fragment, FilterError> {
    let mut reader = Reader::from_reader(input);
    let config = reader.config_mut();
    config.check_end_names = true;
    config.expand_empty_elements = false;
    let mut buf = Vec::new();
    let mut depth = 0usize;
    let mut seen_root = false;
    let mut root_closed = false;
    let mut fragment = Fragment::default();
    let mut emitted_channels: HashSet<String> = HashSet::new();
    // Capture state for a kept subtree: (kind, start tag, body writer, depth at start).
    let mut capture: Option<(Capture, Writer<Vec<u8>>, usize)> = None;
    // Depth of a subtree being skipped (not wanted).
    let mut skipping: Option<usize> = None;
    loop {
        let event = reader.read_event_into(&mut buf).map_err(malformed)?;
        match &event {
            Event::DocType(_) => return Err(FilterError::Doctype),
            Event::GeneralRef(r) => {
                if !allowed_ref(r.as_ref()) {
                    return Err(FilterError::Entity);
                }
            }
            Event::Eof => break,
            _ => {}
        }
        // Track depth for every event.
        let (opens, closes) = match &event {
            Event::Start(_) => (true, false),
            Event::End(_) => (false, true),
            _ => (false, false),
        };
        if opens {
            depth += 1;
            if depth > limits.max_depth {
                return Err(FilterError::Limit("depth"));
            }
        }
        if root_closed && matches!(event, Event::Start(_) | Event::Empty(_)) {
            return Err(FilterError::Malformed("content after the <tv> root".into()));
        }
        // Inside a skipped subtree: ignore until it closes.
        if let Some(skip_depth) = skipping {
            if closes && depth == skip_depth {
                skipping = None;
            }
            if closes {
                depth -= 1;
            }
            buf.clear();
            continue;
        }
        // Inside a kept subtree: copy the event.
        if let Some((_, writer, start_depth)) = capture.as_mut() {
            if closes && depth == *start_depth {
                // Close the captured element and emit it per published id.
                let Some((kind, writer, _)) = capture.take() else {
                    return Err(malformed("capture state lost"));
                };
                emit(
                    kind,
                    Some(&writer.into_inner()),
                    &mut fragment,
                    &mut emitted_channels,
                    limits,
                )?;
            } else {
                writer.write_event(event.borrow()).map_err(malformed)?;
                if writer.get_ref().len() > limits.max_element_bytes {
                    return Err(FilterError::Limit("element size"));
                }
            }
            if closes {
                depth -= 1;
            }
            buf.clear();
            continue;
        }
        match &event {
            Event::Start(start) | Event::Empty(start) => {
                let empty = matches!(event, Event::Empty(_));
                let level = if empty { depth + 1 } else { depth };
                if level == 1 {
                    if start.name().as_ref() != b"tv" || seen_root {
                        return Err(FilterError::Malformed("root element is not <tv>".into()));
                    }
                    seen_root = true;
                    if empty {
                        root_closed = true;
                    }
                } else if level == 2 {
                    let selected = select(start, wanted, window, &emitted_channels)?;
                    match (selected, empty) {
                        (Some(kind), true) => {
                            emit(kind, None, &mut fragment, &mut emitted_channels, limits)?;
                        }
                        (Some(kind), false) => {
                            capture = Some((kind, Writer::new(Vec::new()), depth));
                        }
                        (None, false) => skipping = Some(depth),
                        (None, true) => {}
                    }
                } else {
                    return Err(malformed("unexpected nesting"));
                }
            }
            Event::End(_) => {
                if depth == 1 {
                    root_closed = true;
                }
                depth = depth.saturating_sub(1);
            }
            Event::Text(text) => {
                if depth == 0 && !text.iter().all(u8::is_ascii_whitespace) {
                    return Err(malformed("text outside the <tv> root"));
                }
            }
            Event::CData(_) if depth == 0 => {
                return Err(malformed("text outside the <tv> root"));
            }
            _ => {}
        }
        buf.clear();
    }
    if !seen_root || !root_closed || capture.is_some() || skipping.is_some() {
        return Err(FilterError::Malformed(
            "truncated document (no closing </tv>)".into(),
        ));
    }
    Ok(fragment)
}

/// A kept element: its rewritten start tags (one per published id) and kind.
#[derive(Debug)]
struct Capture {
    is_channel: bool,
    starts: Vec<(BytesStart<'static>, String)>,
}

fn select<S: std::hash::BuildHasher>(
    start: &BytesStart<'_>,
    wanted: &HashMap<String, Vec<String>, S>,
    window: Window,
    emitted: &HashSet<String>,
) -> Result<Option<Capture>, FilterError> {
    let (is_channel, key): (bool, &[u8]) = match start.name().as_ref() {
        b"channel" => (true, b"id"),
        b"programme" => (false, b"channel"),
        _ => return Ok(None),
    };
    let Some(provider) = attr_value(start, key)? else {
        return Ok(None);
    };
    let Some(targets) = wanted.get(&provider) else {
        return Ok(None);
    };
    if !is_channel {
        let begin = attr_value(start, b"start")?
            .as_deref()
            .and_then(parse_xmltv_time);
        let end = attr_value(start, b"stop")?
            .as_deref()
            .and_then(parse_xmltv_time);
        let Some(begin) = begin else { return Ok(None) };
        // Without a stop time assume a programme lasts at most 12 hours.
        let end = end.unwrap_or(begin + 12 * 3600);
        if end <= window.from || begin >= window.until {
            return Ok(None);
        }
    }
    let mut starts = Vec::with_capacity(targets.len());
    for target in targets {
        if is_channel && emitted.contains(target) {
            continue;
        }
        starts.push((rewrite_start(start, key, target)?, target.clone()));
    }
    if starts.is_empty() {
        return Ok(None);
    }
    Ok(Some(Capture { is_channel, starts }))
}

fn emit(
    capture: Capture,
    body: Option<&[u8]>,
    fragment: &mut Fragment,
    emitted: &mut HashSet<String>,
    limits: FilterLimits,
) -> Result<(), FilterError> {
    for (start, target) in capture.starts {
        if capture.is_channel {
            if !emitted.insert(target.clone()) {
                continue;
            }
            fragment.channel_count += 1;
            if fragment.channel_count > limits.max_channels {
                return Err(FilterError::Limit("channels"));
            }
        } else {
            fragment.programme_count += 1;
            if fragment.programme_count > limits.max_programmes {
                return Err(FilterError::Limit("programmes"));
            }
        }
        let mut writer = Writer::new(Vec::new());
        match body {
            None => writer.write_event(Event::Empty(start)).map_err(malformed)?,
            Some(body) => {
                let end_name = std::str::from_utf8(start.name().as_ref())
                    .map_err(malformed)?
                    .to_owned();
                writer.write_event(Event::Start(start)).map_err(malformed)?;
                writer.get_mut().extend_from_slice(body);
                writer
                    .write_event(Event::End(BytesEnd::new(end_name)))
                    .map_err(malformed)?;
            }
        }
        let mut bytes = writer.into_inner();
        bytes.push(b'\n');
        fragment.bytes += bytes.len();
        if fragment.bytes > limits.max_output_bytes {
            return Err(FilterError::Limit("output bytes"));
        }
        let slot = fragment.targets.entry(target).or_default();
        if capture.is_channel {
            slot.channel = bytes;
        } else {
            slot.programmes.extend_from_slice(&bytes);
            slot.programme_count += 1;
        }
    }
    Ok(())
}

/// Escape text for XML element content / attribute values.
#[must_use]
pub fn escape(value: &str) -> String {
    quick_xml::escape::escape(value).into_owned()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const NOW: i64 = 1_793_000_000; // 2026-10-27T07:33:20Z

    fn wanted(pairs: &[(&str, &str)]) -> HashMap<String, Vec<String>> {
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        for (provider, published) in pairs {
            map.entry((*provider).to_owned())
                .or_default()
                .push((*published).to_owned());
        }
        map
    }

    fn ts(unix: i64) -> String {
        // Format unix seconds as an XMLTV UTC timestamp (test helper).
        let days = unix.div_euclid(86_400);
        let secs = unix.rem_euclid(86_400);
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = yoe + era * 400 + i64::from(m <= 2);
        format!(
            "{y:04}{m:02}{d:02}{:02}{:02}{:02} +0000",
            secs / 3600,
            secs % 3600 / 60,
            secs % 60
        )
    }

    fn feed() -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<tv generator-info-name="up">
  <channel id="464857"><display-name lang="en">CNN &amp; Friends</display-name><icon src="https://l/cnn.png"/></channel>
  <channel id="999"><display-name>Other</display-name></channel>
  <programme start="{a}" stop="{b}" channel="464857"><title>Now &lt;Live&gt;</title><desc><![CDATA[raw <b>]]></desc></programme>
  <programme start="{c}" stop="{d}" channel="464857"><title>Past</title></programme>
  <programme start="{e}" stop="{f}" channel="464857"><title>Far future</title></programme>
  <programme start="{a}" stop="{b}" channel="999"><title>Not wanted</title></programme>
</tv>"#,
            a = ts(NOW - 600),
            b = ts(NOW + 1800),
            c = ts(NOW - 7200),
            d = ts(NOW - 3600),
            e = ts(NOW + 8 * 86_400),
            f = ts(NOW + 8 * 86_400 + 3600),
        )
    }

    #[test]
    fn xmltv_time_parses_zones() {
        assert_eq!(parse_xmltv_time("19700101000000 +0000"), Some(0));
        assert_eq!(parse_xmltv_time("19700101010000 +0100"), Some(0));
        assert_eq!(parse_xmltv_time("19700101000000 -0030"), Some(1800));
        assert_eq!(parse_xmltv_time("197001010000"), Some(0));
        assert_eq!(parse_xmltv_time(&ts(NOW)), Some(NOW));
        assert_eq!(parse_xmltv_time("garbage"), None);
        assert_eq!(parse_xmltv_time("20261399000000"), None);
    }

    #[test]
    fn selects_rewrites_and_windows() {
        let out = filter_xmltv(
            feed().as_bytes(),
            &wanted(&[("464857", "CNN.us")]),
            Window::seven_days_from(NOW),
            FilterLimits::default(),
        )
        .unwrap();
        assert_eq!(out.targets.len(), 1);
        let cnn = &out.targets["CNN.us"];
        let channels = String::from_utf8(cnn.channel.clone()).unwrap();
        let programmes = String::from_utf8(cnn.programmes.clone()).unwrap();
        assert_eq!(out.channel_count, 1);
        assert!(channels.contains(r#"<channel id="CNN.us">"#), "{channels}");
        assert!(channels.contains("CNN &amp; Friends"), "{channels}");
        assert!(channels.contains(r#"<icon src="https://l/cnn.png"/>"#));
        assert!(!channels.contains("Other"));
        assert_eq!(out.programme_count, 1);
        assert!(programmes.contains(r#"channel="CNN.us""#), "{programmes}");
        assert!(programmes.contains("Now &lt;Live&gt;"));
        assert!(programmes.contains("<![CDATA[raw <b>]]>"));
        assert!(!programmes.contains("Past"));
        assert!(!programmes.contains("Far future"));
        assert!(!programmes.contains("Not wanted"));
        assert!(!programmes.contains("464857"));
    }

    #[test]
    fn one_provider_channel_can_feed_two_published_ids() {
        let out = filter_xmltv(
            feed().as_bytes(),
            &wanted(&[("464857", "a"), ("464857", "b")]),
            Window::seven_days_from(NOW),
            FilterLimits::default(),
        )
        .unwrap();
        assert_eq!(out.channel_count, 2);
        assert_eq!(out.programme_count, 2);
        assert!(String::from_utf8(out.targets["a"].channel.clone())
            .unwrap()
            .contains(r#"id="a""#));
        assert!(String::from_utf8(out.targets["b"].programmes.clone())
            .unwrap()
            .contains(r#"channel="b""#));
    }

    #[test]
    fn doctype_and_entities_are_rejected() {
        let bomb = r#"<?xml version="1.0"?><!DOCTYPE tv [<!ENTITY a "aaaa">]><tv><channel id="x"><display-name>&a;</display-name></channel></tv>"#;
        assert_eq!(
            filter_xmltv(
                bomb.as_bytes(),
                &wanted(&[("x", "x")]),
                Window::seven_days_from(NOW),
                FilterLimits::default()
            ),
            Err(FilterError::Doctype)
        );
        let undeclared = r#"<tv><channel id="x"><display-name>&a;</display-name></channel></tv>"#;
        assert_eq!(
            filter_xmltv(
                undeclared.as_bytes(),
                &wanted(&[("x", "x")]),
                Window::seven_days_from(NOW),
                FilterLimits::default()
            ),
            Err(FilterError::Entity)
        );
        // Character references and predefined entities pass.
        let ok = r#"<tv><channel id="x"><display-name>&#169; &amp;</display-name></channel></tv>"#;
        let out = filter_xmltv(
            ok.as_bytes(),
            &wanted(&[("x", "y")]),
            Window::seven_days_from(NOW),
            FilterLimits::default(),
        )
        .unwrap();
        assert!(String::from_utf8(out.targets["y"].channel.clone())
            .unwrap()
            .contains("&#169; &amp;"));
    }

    #[test]
    fn malformed_input_is_rejected() {
        let w = wanted(&[("x", "x")]);
        let win = Window::seven_days_from(NOW);
        let lim = FilterLimits::default();
        for bad in [
            "<tv><channel id=\"x\"><display-name>x</channel></tv>",
            "<tv><channel id=\"x\">",
            "<rss></rss>",
            "",
            "not xml at all",
            "<tv></tv><tv></tv>",
        ] {
            assert!(filter_xmltv(bad.as_bytes(), &w, win, lim).is_err(), "{bad}");
        }
    }

    #[test]
    fn limits_are_enforced() {
        let mut doc = String::from("<tv>");
        for i in 0..10 {
            doc += &["<channel id=\"c", &i.to_string(), "\"/>"].concat();
        }
        doc.push_str("</tv>");
        let mut w = HashMap::new();
        for i in 0..10 {
            w.insert(format!("c{i}"), vec![format!("p{i}")]);
        }
        let lim = FilterLimits {
            max_channels: 5,
            ..FilterLimits::default()
        };
        assert_eq!(
            filter_xmltv(doc.as_bytes(), &w, Window::seven_days_from(NOW), lim),
            Err(FilterError::Limit("channels"))
        );
        let deep = format!(
            "<tv><channel id=\"c0\">{}</channel></tv>",
            "<a>".repeat(40) + &"</a>".repeat(40)
        );
        let lim = FilterLimits::default();
        assert_eq!(
            filter_xmltv(deep.as_bytes(), &w, Window::seven_days_from(NOW), lim),
            Err(FilterError::Limit("depth"))
        );
    }
}
