//! A small, bounded, panic-free XML scanner specialised for the MPEG-DASH MPD
//! subset Multiview consumes.
//!
//! This is **not** a general XML parser. It tokenises start tags, end tags, and
//! self-closing tags (with attributes), skips the `<?xml …?>` declaration,
//! comments, CDATA, and DOCTYPE, ignores text nodes, and bounds both the total
//! element count and nesting depth so a hostile manifest cannot exhaust memory or
//! blow the stack. Attribute values are unescaped for the five predefined XML
//! entities only. Everything it cannot represent surfaces as
//! [`DashError::MalformedXml`].

use super::{
    parse_iso8601_duration, AdaptationSet, DashError, Mpd, Period, PresentationType,
    Representation, SegmentTemplate,
};

/// Maximum number of element start-tags the scanner will process. A real MPD has
/// at most a few thousand elements; this bounds work on adversarial input.
const MAX_ELEMENTS: usize = 100_000;

/// Maximum element nesting depth. MPD nesting is shallow (MPD → `Period` →
/// `AdaptationSet` → `Representation` → `SegmentTemplate` ≈ 5).
const MAX_DEPTH: usize = 64;

/// A scanned XML token.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    /// `<name attr="v" …>`
    Start {
        name: String,
        attrs: Vec<(String, String)>,
    },
    /// `<name attr="v" …/>`
    Empty {
        name: String,
        attrs: Vec<(String, String)>,
    },
    /// `</name>`
    End { name: String },
}

/// A cursor over the manifest characters.
struct Scanner<'a> {
    bytes: &'a [u8],
    pos: usize,
    elements_seen: usize,
}

impl<'a> Scanner<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            bytes: text.as_bytes(),
            pos: 0,
            elements_seen: 0,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.bytes.get(self.pos).copied();
        if b.is_some() {
            self.pos = self.pos.saturating_add(1);
        }
        b
    }

    /// Advance past ASCII whitespace.
    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.pos = self.pos.saturating_add(1);
        }
    }

    /// Advance until `needle` (a short ASCII marker) is consumed; returns whether
    /// it was found.
    fn skip_until(&mut self, needle: &[u8]) -> bool {
        if needle.is_empty() {
            return true;
        }
        while self.pos < self.bytes.len() {
            if self
                .bytes
                .get(self.pos..self.pos.saturating_add(needle.len()))
                == Some(needle)
            {
                self.pos = self.pos.saturating_add(needle.len());
                return true;
            }
            self.pos = self.pos.saturating_add(1);
        }
        false
    }

    /// Read the next token, skipping text, declarations, comments, CDATA, and
    /// DOCTYPE. Returns `Ok(None)` at clean end of input.
    fn next_token(&mut self) -> Result<Option<Token>, DashError> {
        loop {
            // Advance to the next '<'.
            while let Some(b) = self.peek() {
                if b == b'<' {
                    break;
                }
                self.pos = self.pos.saturating_add(1);
            }
            let Some(_lt) = self.bump() else {
                return Ok(None);
            };
            match self.peek() {
                Some(b'?') => {
                    // <?xml …?> — skip and loop for the next token.
                    if !self.skip_until(b"?>") {
                        return Err(DashError::MalformedXml("unterminated <? …?>"));
                    }
                }
                Some(b'!') => {
                    // Comment, CDATA, or DOCTYPE — skip and loop for the next token.
                    if self.bytes.get(self.pos..self.pos.saturating_add(3)) == Some(b"!--") {
                        self.pos = self.pos.saturating_add(3);
                        if !self.skip_until(b"-->") {
                            return Err(DashError::MalformedXml("unterminated comment"));
                        }
                    } else if !self.skip_until(b">") {
                        return Err(DashError::MalformedXml("unterminated <! …>"));
                    }
                }
                Some(b'/') => {
                    self.pos = self.pos.saturating_add(1);
                    let name = self.read_name()?;
                    self.skip_ws();
                    if self.bump() != Some(b'>') {
                        return Err(DashError::MalformedXml("end tag not closed with '>'"));
                    }
                    return Ok(Some(Token::End { name }));
                }
                Some(_) => {
                    self.elements_seen = self.elements_seen.saturating_add(1);
                    if self.elements_seen > MAX_ELEMENTS {
                        return Err(DashError::MalformedXml("element budget exceeded"));
                    }
                    let name = self.read_name()?;
                    let attrs = self.read_attributes()?;
                    self.skip_ws();
                    match self.bump() {
                        Some(b'>') => return Ok(Some(Token::Start { name, attrs })),
                        Some(b'/') => {
                            if self.bump() != Some(b'>') {
                                return Err(DashError::MalformedXml(
                                    "self-closing tag missing '>'",
                                ));
                            }
                            return Ok(Some(Token::Empty { name, attrs }));
                        }
                        _ => {
                            return Err(DashError::MalformedXml("start tag not closed"));
                        }
                    }
                }
                None => return Err(DashError::MalformedXml("'<' at end of input")),
            }
        }
    }

    /// Read an element / attribute name (`[A-Za-z0-9_:.-]+`, possibly with a
    /// namespace prefix which we strip).
    fn read_name(&mut self) -> Result<String, DashError> {
        let start = self.pos;
        while let Some(b) = self.peek() {
            if b.is_ascii_alphanumeric() || matches!(b, b'_' | b':' | b'.' | b'-') {
                self.pos = self.pos.saturating_add(1);
            } else {
                break;
            }
        }
        let raw = self
            .bytes
            .get(start..self.pos)
            .ok_or(DashError::MalformedXml("name slice out of range"))?;
        if raw.is_empty() {
            return Err(DashError::MalformedXml("empty element/attribute name"));
        }
        let name = core::str::from_utf8(raw)
            .map_err(|_e| DashError::MalformedXml("name is not valid utf-8"))?;
        // Strip an `ns:` prefix.
        let local = name.rsplit(':').next().unwrap_or(name);
        Ok(local.to_owned())
    }

    /// Read zero or more `name="value"` (or `name='value'`) attributes.
    fn read_attributes(&mut self) -> Result<Vec<(String, String)>, DashError> {
        let mut attrs = Vec::new();
        loop {
            self.skip_ws();
            match self.peek() {
                Some(b'>' | b'/') | None => break,
                Some(_) => {}
            }
            let name = self.read_name()?;
            self.skip_ws();
            if self.bump() != Some(b'=') {
                return Err(DashError::MalformedXml("attribute missing '='"));
            }
            self.skip_ws();
            let quote = self
                .bump()
                .filter(|q| *q == b'"' || *q == b'\'')
                .ok_or(DashError::MalformedXml("attribute value not quoted"))?;
            let start = self.pos;
            while let Some(b) = self.peek() {
                if b == quote {
                    break;
                }
                self.pos = self.pos.saturating_add(1);
            }
            let raw = self
                .bytes
                .get(start..self.pos)
                .ok_or(DashError::MalformedXml(
                    "attribute value slice out of range",
                ))?;
            if self.bump() != Some(quote) {
                return Err(DashError::MalformedXml("unterminated attribute value"));
            }
            let value = core::str::from_utf8(raw)
                .map_err(|_e| DashError::MalformedXml("attribute value not utf-8"))?;
            attrs.push((name, unescape(value)));
        }
        Ok(attrs)
    }
}

/// Unescape the five predefined XML entities in an attribute value.
fn unescape(value: &str) -> String {
    if !value.contains('&') {
        return value.to_owned();
    }
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Find an attribute value by (already namespace-stripped) name.
fn attr<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _v)| k == name)
        .map(|(_k, v)| v.as_str())
}

/// Parse a `u64` attribute, mapping a parse failure to [`DashError::BadAttribute`].
fn attr_u64(attrs: &[(String, String)], name: &'static str) -> Result<Option<u64>, DashError> {
    match attr(attrs, name) {
        None => Ok(None),
        Some(v) => v
            .parse::<u64>()
            .map(Some)
            .map_err(|_e| DashError::BadAttribute {
                attr: name,
                value: v.to_owned(),
            }),
    }
}

/// Parse a `u32` attribute.
fn attr_u32(attrs: &[(String, String)], name: &'static str) -> Result<Option<u32>, DashError> {
    match attr(attrs, name) {
        None => Ok(None),
        Some(v) => v
            .parse::<u32>()
            .map(Some)
            .map_err(|_e| DashError::BadAttribute {
                attr: name,
                value: v.to_owned(),
            }),
    }
}

/// Parse an MPD manifest by walking the token stream and maintaining an explicit
/// element-depth counter (no recursion).
pub(super) fn parse_mpd(manifest: &str) -> Result<Mpd, DashError> {
    let mut scanner = Scanner::new(manifest);
    let mut mpd: Option<Mpd> = None;
    let mut depth: usize = 0;

    // Work-in-progress containers for the element currently being filled.
    let mut current_period: Option<Period> = None;
    let mut current_set: Option<AdaptationSet> = None;
    let mut current_repr: Option<Representation> = None;

    let mut saw_root = false;

    while let Some(token) = scanner.next_token()? {
        match token {
            Token::Start { name, attrs } => {
                if !saw_root {
                    if name != "MPD" {
                        return Err(DashError::NotMpd);
                    }
                    saw_root = true;
                }
                depth = depth
                    .checked_add(1)
                    .ok_or(DashError::MalformedXml("depth overflow"))?;
                if depth > MAX_DEPTH {
                    return Err(DashError::MalformedXml("nesting too deep"));
                }
                handle_open(
                    &name,
                    &attrs,
                    &mut mpd,
                    &mut current_period,
                    &mut current_set,
                    &mut current_repr,
                )?;
            }
            Token::Empty { name, attrs } => {
                if !saw_root {
                    if name != "MPD" {
                        return Err(DashError::NotMpd);
                    }
                    saw_root = true;
                }
                // A self-closing element: apply its attributes, then immediately
                // fold it into its parent — it opens and closes in one token.
                handle_open(
                    &name,
                    &attrs,
                    &mut mpd,
                    &mut current_period,
                    &mut current_set,
                    &mut current_repr,
                )?;
                close_element(
                    &name,
                    &mut mpd,
                    &mut current_period,
                    &mut current_set,
                    &mut current_repr,
                );
            }
            Token::End { name } => {
                close_element(
                    &name,
                    &mut mpd,
                    &mut current_period,
                    &mut current_set,
                    &mut current_repr,
                );
                depth = depth.saturating_sub(1);
            }
        }
    }

    mpd.ok_or(DashError::NotMpd)
}

/// Apply an element's attributes to the work-in-progress model.
fn handle_open(
    name: &str,
    attrs: &[(String, String)],
    mpd: &mut Option<Mpd>,
    period: &mut Option<Period>,
    set: &mut Option<AdaptationSet>,
    repr: &mut Option<Representation>,
) -> Result<(), DashError> {
    match name {
        "MPD" => {
            let presentation_type = match attr(attrs, "type") {
                Some("dynamic") => PresentationType::Dynamic,
                _ => PresentationType::Static,
            };
            let min_buffer_time = match attr(attrs, "minBufferTime") {
                Some(v) => Some(parse_iso8601_duration(v)?),
                None => None,
            };
            let media_presentation_duration = match attr(attrs, "mediaPresentationDuration") {
                Some(v) => Some(parse_iso8601_duration(v)?),
                None => None,
            };
            *mpd = Some(Mpd {
                presentation_type,
                min_buffer_time,
                media_presentation_duration,
                periods: Vec::new(),
            });
        }
        "Period" => {
            *period = Some(Period {
                id: attr(attrs, "id").map(ToOwned::to_owned),
                start: opt_duration(attrs, "start")?,
                duration: opt_duration(attrs, "duration")?,
                adaptation_sets: Vec::new(),
            });
        }
        "AdaptationSet" => {
            *set = Some(AdaptationSet {
                content_type: attr(attrs, "contentType").map(ToOwned::to_owned),
                mime_type: attr(attrs, "mimeType").map(ToOwned::to_owned),
                segment_template: None,
                representations: Vec::new(),
            });
        }
        "Representation" => {
            *repr = Some(Representation {
                id: attr(attrs, "id").unwrap_or_default().to_owned(),
                bandwidth: attr_u64(attrs, "bandwidth")?.unwrap_or(0),
                width: attr_u32(attrs, "width")?,
                height: attr_u32(attrs, "height")?,
                codecs: attr(attrs, "codecs").map(ToOwned::to_owned),
                segment_template: None,
            });
        }
        "SegmentTemplate" => {
            let template = SegmentTemplate {
                initialization: attr(attrs, "initialization").map(ToOwned::to_owned),
                media: attr(attrs, "media").map(ToOwned::to_owned),
                timescale: attr_u64(attrs, "timescale")?.unwrap_or(1),
                duration: attr_u64(attrs, "duration")?,
                start_number: attr_u64(attrs, "startNumber")?.unwrap_or(1),
            };
            // Attach to the innermost open container.
            if let Some(r) = repr.as_mut() {
                r.segment_template = Some(template);
            } else if let Some(s) = set.as_mut() {
                s.segment_template = Some(template);
            }
        }
        _ => {}
    }
    Ok(())
}

/// Close an element, folding the finished work-in-progress container into its
/// parent.
fn close_element(
    name: &str,
    mpd: &mut Option<Mpd>,
    period: &mut Option<Period>,
    set: &mut Option<AdaptationSet>,
    repr: &mut Option<Representation>,
) {
    match name {
        "Representation" => {
            if let (Some(r), Some(s)) = (repr.take(), set.as_mut()) {
                s.representations.push(r);
            }
        }
        "AdaptationSet" => {
            if let (Some(s), Some(p)) = (set.take(), period.as_mut()) {
                p.adaptation_sets.push(s);
            }
        }
        "Period" => {
            if let (Some(p), Some(m)) = (period.take(), mpd.as_mut()) {
                m.periods.push(p);
            }
        }
        _ => {}
    }
}

/// Parse an optional ISO 8601 duration attribute.
fn opt_duration(
    attrs: &[(String, String)],
    name: &str,
) -> Result<Option<core::time::Duration>, DashError> {
    match attr(attrs, name) {
        Some(v) => parse_iso8601_duration(v).map(Some),
        None => Ok(None),
    }
}
