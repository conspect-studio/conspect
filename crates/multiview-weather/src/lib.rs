//! Standalone bounded data models and parsers for selected BOM XML products.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use chrono::{DateTime, FixedOffset, Utc};
use quick_xml::{events::Event, Reader, XmlVersion};
use std::collections::BTreeMap;
use thiserror::Error;

/// Resource ceilings applied before and during XML parsing.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Maximum input bytes.
    pub max_bytes: usize,
    /// Maximum element nesting depth.
    pub max_depth: usize,
    /// Maximum total XML nodes.
    pub max_nodes: usize,
    /// Maximum decoded character data per node.
    pub max_text_bytes: usize,
    /// Maximum stations or areas.
    pub max_items: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_bytes: 2_000_000,
            max_depth: 32,
            max_nodes: 100_000,
            max_text_bytes: 16_384,
            max_items: 5_000,
        }
    }
}

/// Parser failures, including resource, XML, and data validation errors.
#[derive(Debug, Error)]
pub enum ParseError {
    /// Input exceeded a configured ceiling.
    #[error("limit exceeded: {0}")]
    LimitExceeded(&'static str),
    /// Invalid XML structure or syntax.
    #[error("malformed XML: {0}")]
    Xml(String),
    /// Required product field missing or unsupported.
    #[error("invalid product: {0}")]
    Product(&'static str),
    /// Invalid typed value.
    #[error("invalid {field}: {value}")]
    InvalidValue {
        /// Value name.
        field: String,
        /// Rejected source representation.
        value: String,
    },
}

/// Shared product provenance.
#[derive(Clone, Debug)]
pub struct Provenance {
    /// BOM product identifier.
    pub product_id: String,
    /// Product issue instant in UTC.
    pub issued: DateTime<Utc>,
    /// Optional expiry instant in UTC.
    pub expires: Option<DateTime<Utc>>,
}
/// A parsed forecast product.
#[derive(Clone, Debug)]
pub struct Forecast {
    /// Product metadata.
    pub provenance: Provenance,
    /// Product identifier (convenience copy).
    pub product_id: String,
    /// Forecast areas.
    pub areas: Vec<Area>,
}
/// Forecast area identity and validity periods.
#[derive(Clone, Debug)]
pub struct Area {
    /// BOM area code.
    pub aac: String,
    /// Display name.
    pub description: String,
    /// BOM area category.
    pub area_type: String,
    /// Periods.
    pub periods: Vec<ForecastPeriod>,
}
/// Bounded validity window and values.
#[derive(Clone, Debug)]
pub struct ForecastPeriod {
    /// Sequential product index.
    pub index: u32,
    /// UTC validity start.
    pub start_utc: DateTime<Utc>,
    /// UTC validity end.
    pub end_utc: DateTime<Utc>,
    /// Corresponding local time strings, including offsets.
    pub start_local: String,
    /// Local end time.
    pub end_local: String,
    /// Preserved icon code, including unrecognized values.
    pub icon_code: Option<String>,
    /// Optional forecast elements and text.
    pub elements: Vec<Value>,
}
/// An element or text value: unknown types and units are retained.
#[derive(Clone, Debug)]
pub struct Value {
    /// BOM type key.
    pub kind: String,
    /// Declared unit, if present.
    pub units: Option<String>,
    /// Absent stays `None`.
    pub value: Option<String>,
}
/// State observations grouped by station.
#[derive(Clone, Debug)]
pub struct Observations {
    /// Product metadata.
    pub provenance: Provenance,
    /// Stations, not forecast areas.
    pub stations: Vec<Station>,
}
/// Station identity and observation periods.
#[derive(Clone, Debug)]
pub struct Station {
    /// BOM station ID.
    pub bom_id: String,
    /// WMO ID, if present.
    pub wmo_id: Option<String>,
    /// Station name.
    pub name: String,
    /// IANA timezone ID.
    pub timezone: Option<String>,
    /// Periods.
    pub periods: Vec<ObservationPeriod>,
}
/// Instant and observed level values.
#[derive(Clone, Debug)]
pub struct ObservationPeriod {
    /// UTC instant.
    pub time_utc: DateTime<Utc>,
    /// Local timestamp with offset retained.
    pub time_local: String,
    /// Data values.
    pub elements: Vec<Value>,
}

#[derive(Clone, Debug)]
struct Node {
    name: String,
    attrs: BTreeMap<String, String>,
    text: String,
    children: Vec<Node>,
}
#[allow(clippy::too_many_lines)]
fn tree(bytes: &[u8], l: Limits) -> Result<Node, ParseError> {
    if bytes.len() > l.max_bytes {
        return Err(ParseError::LimitExceeded("bytes"));
    }
    let mut r = Reader::from_reader(bytes);
    // Text arrives in fragments split around entity references, so trimming
    // happens once per element (at its end), never per fragment.
    let mut buf = Vec::new();
    let mut stack: Vec<Node> = Vec::new();
    let mut root = None;
    let mut count = 0;
    loop {
        let event = r
            .read_event_into(&mut buf)
            .map_err(|e| ParseError::Xml(e.to_string()))?;
        if matches!(event, Event::Decl(_)) && count == 0 && root.is_none() {
            // XML declarations are permitted only before any root node.
        }
        match event {
            Event::Start(e) => {
                count += 1;
                if count > l.max_nodes {
                    return Err(ParseError::LimitExceeded("nodes"));
                }
                if stack.len() + 1 > l.max_depth {
                    return Err(ParseError::LimitExceeded("depth"));
                }
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                let mut attrs = BTreeMap::new();
                for a in e.attributes() {
                    let a = a.map_err(|e| ParseError::Xml(e.to_string()))?;
                    attrs.insert(
                        String::from_utf8_lossy(a.key.as_ref()).into_owned(),
                        a.decoded_and_normalized_value(XmlVersion::Implicit1_0, r.decoder())
                            .map_err(|e| ParseError::Xml(e.to_string()))?
                            .into_owned(),
                    );
                }
                stack.push(Node {
                    name,
                    attrs,
                    text: String::new(),
                    children: Vec::new(),
                });
            }
            Event::Empty(e) => {
                count += 1;
                if count > l.max_nodes {
                    return Err(ParseError::LimitExceeded("nodes"));
                }
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                let mut attrs = BTreeMap::new();
                for a in e.attributes() {
                    let a = a.map_err(|e| ParseError::Xml(e.to_string()))?;
                    attrs.insert(
                        String::from_utf8_lossy(a.key.as_ref()).into_owned(),
                        a.decoded_and_normalized_value(XmlVersion::Implicit1_0, r.decoder())
                            .map_err(|e| ParseError::Xml(e.to_string()))?
                            .into_owned(),
                    );
                }
                let n = Node {
                    name,
                    attrs,
                    text: String::new(),
                    children: Vec::new(),
                };
                if let Some(p) = stack.last_mut() {
                    p.children.push(n);
                } else if root.replace(n).is_some() {
                    return Err(ParseError::Xml("multiple roots".into()));
                }
            }
            Event::Text(t) => {
                if let Some(n) = stack.last_mut() {
                    let s = t.decode().map_err(|e| ParseError::Xml(e.to_string()))?;
                    if s.len() > l.max_text_bytes.saturating_sub(n.text.len()) {
                        return Err(ParseError::LimitExceeded("text"));
                    }
                    n.text.push_str(&s);
                }
            }
            Event::GeneralRef(reference) => {
                if let Some(n) = stack.last_mut() {
                    let resolved = reference
                        .resolve_char_ref()
                        .map_err(|e| ParseError::Xml(e.to_string()))?;
                    let ch = if let Some(ch) = resolved {
                        ch
                    } else {
                        let name = reference
                            .decode()
                            .map_err(|e| ParseError::Xml(e.to_string()))?;
                        predefined_entity(&name)
                            .ok_or_else(|| ParseError::Xml(format!("undeclared entity &{name};")))?
                    };
                    if ch.len_utf8() > l.max_text_bytes.saturating_sub(n.text.len()) {
                        return Err(ParseError::LimitExceeded("text"));
                    }
                    n.text.push(ch);
                }
            }
            Event::CData(t) => {
                if let Some(n) = stack.last_mut() {
                    let s = std::str::from_utf8(t.as_ref())
                        .map_err(|e| ParseError::Xml(e.to_string()))?;
                    if s.len() > l.max_text_bytes.saturating_sub(n.text.len()) {
                        return Err(ParseError::LimitExceeded("text"));
                    }
                    n.text.push_str(s);
                }
            }
            Event::End(_) => {
                let mut n = stack
                    .pop()
                    .ok_or_else(|| ParseError::Xml("unmatched end".into()))?;
                let trimmed = n.text.trim();
                if trimmed.len() != n.text.len() {
                    let trimmed = trimmed.to_owned();
                    n.text = trimmed;
                }
                if let Some(p) = stack.last_mut() {
                    p.children.push(n);
                } else if root.replace(n).is_some() {
                    return Err(ParseError::Xml("multiple roots".into()));
                }
            }
            Event::DocType(_) => return Err(ParseError::Xml("DTD forbidden".into())),
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    if !stack.is_empty() {
        return Err(ParseError::Xml("unclosed element".into()));
    }
    root.ok_or_else(|| ParseError::Xml("missing root".into()))
}
fn child<'a>(n: &'a Node, k: &str) -> Option<&'a Node> {
    n.children.iter().find(|c| c.name == k)
}
fn descendants<'a>(n: &'a Node, k: &str, out: &mut Vec<&'a Node>) {
    if n.name == k {
        out.push(n);
    }
    for c in &n.children {
        descendants(c, k, out);
    }
}
fn attr(n: &Node, k: &str) -> Option<String> {
    n.attrs.get(k).cloned()
}
fn date(s: &str, field: &str) -> Result<DateTime<Utc>, ParseError> {
    DateTime::<FixedOffset>::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|_| ParseError::InvalidValue {
            field: field.into(),
            value: s.into(),
        })
}
fn provenance(root: &Node, id: &str) -> Result<Provenance, ParseError> {
    let a = child(root, "amoc").ok_or(ParseError::Product("amoc missing"))?;
    let get = |k: &str| child(a, k).map(|n| n.text.as_str());
    let product_id = get("identifier")
        .ok_or(ParseError::Product("identifier missing"))?
        .to_owned();
    if product_id != id {
        return Err(ParseError::Product("unexpected product identifier"));
    }
    let issued = date(
        get("issue-time-utc").ok_or(ParseError::Product("issue time missing"))?,
        "issue-time-utc",
    )?;
    let expires = get("expiry-time")
        .map(|x| date(x, "expiry-time"))
        .transpose()?;
    if let Some(expiry) = expires.as_ref() {
        if *expiry <= issued {
            return Err(ParseError::InvalidValue {
                field: "expiry-time".into(),
                value: expiry.to_rfc3339(),
            });
        }
    }
    Ok(Provenance {
        product_id,
        issued,
        expires,
    })
}
/// Parse BOM IDN11060 forecast XML with conservative bounds.
///
/// # Errors
/// Returns an error for malformed XML, invalid product fields, or configured bounds.
pub fn parse_forecast(b: &[u8]) -> Result<Forecast, ParseError> {
    parse_forecast_with_limits(b, Limits::default())
}
/// Parse forecast XML using caller-selected bounds.
///
/// # Errors
/// Returns an error for malformed XML, invalid product fields, or configured bounds.
pub fn parse_forecast_with_limits(b: &[u8], l: Limits) -> Result<Forecast, ParseError> {
    let r = tree(b, l)?;
    let p = provenance(&r, "IDN11060")?;
    let forecast = child(&r, "forecast").ok_or(ParseError::Product("forecast missing"))?;
    let mut nodes = Vec::new();
    descendants(forecast, "area", &mut nodes);
    if nodes.len() > l.max_items {
        return Err(ParseError::LimitExceeded("items"));
    }
    let mut areas = Vec::new();
    for a in nodes {
        let mut periods = Vec::new();
        for n in a.children.iter().filter(|n| n.name == "forecast-period") {
            let ix = attr(n, "index").unwrap_or_default().parse().map_err(|_| {
                ParseError::InvalidValue {
                    field: "period index".into(),
                    value: attr(n, "index").unwrap_or_default(),
                }
            })?;
            let su =
                attr(n, "start-time-utc").ok_or(ParseError::Product("forecast start missing"))?;
            let eu = attr(n, "end-time-utc").ok_or(ParseError::Product("forecast end missing"))?;
            let start = date(&su, "forecast start")?;
            let end = date(&eu, "forecast end")?;
            if end <= start {
                return Err(ParseError::InvalidValue {
                    field: "forecast validity".into(),
                    value: format!("{su}..{eu}"),
                });
            }
            let sl =
                attr(n, "start-time-local").ok_or(ParseError::Product("local start missing"))?;
            let el = attr(n, "end-time-local").ok_or(ParseError::Product("local end missing"))?;
            date(&sl, "local start")?;
            date(&el, "local end")?;
            let mut vals = Vec::new();
            for c in &n.children {
                if c.name == "element" || c.name == "text" {
                    let kind = attr(c, "type").unwrap_or_default();
                    if c.name == "element"
                        && kind != "forecast_icon_code"
                        && !c.text.is_empty()
                        && kind != "precipitation_range"
                    {
                        let number =
                            c.text
                                .parse::<f64>()
                                .map_err(|_| ParseError::InvalidValue {
                                    field: kind.clone(),
                                    value: c.text.clone(),
                                })?;
                        if !number.is_finite() {
                            return Err(ParseError::InvalidValue {
                                field: kind.clone(),
                                value: c.text.clone(),
                            });
                        }
                    }
                    vals.push(Value {
                        kind: kind.clone(),
                        units: attr(c, "units"),
                        value: if c.text.is_empty() {
                            None
                        } else {
                            Some(c.text.clone())
                        },
                    });
                }
            }
            let icon = vals
                .iter()
                .find(|v| v.kind == "forecast_icon_code")
                .and_then(|v| v.value.clone());
            periods.push(ForecastPeriod {
                index: ix,
                start_utc: start,
                end_utc: end,
                start_local: sl,
                end_local: el,
                icon_code: icon,
                elements: vals,
            });
        }
        areas.push(Area {
            aac: attr(a, "aac").ok_or(ParseError::Product("area aac missing"))?,
            description: attr(a, "description").unwrap_or_default(),
            area_type: attr(a, "type").unwrap_or_default(),
            periods,
        });
    }
    Ok(Forecast {
        product_id: p.product_id.clone(),
        provenance: p,
        areas,
    })
}
/// Parse BOM IDN60920 state observations XML with explicit caller bounds.
///
/// # Errors
/// Returns an error for malformed XML, invalid product fields, or configured bounds.
pub fn parse_observations(b: &[u8]) -> Result<Observations, ParseError> {
    parse_observations_with_limits(b, Limits::default())
}

/// Parse IDN60920 observations using caller-selected bounds.
///
/// # Errors
/// Returns an error for malformed XML, invalid product fields, or configured bounds.
pub fn parse_observations_with_limits(b: &[u8], l: Limits) -> Result<Observations, ParseError> {
    let r = tree(b, l)?;
    let p = provenance(&r, "IDN60920")?;
    let observations =
        child(&r, "observations").ok_or(ParseError::Product("observations missing"))?;
    let mut ns = Vec::new();
    descendants(observations, "station", &mut ns);
    if ns.len() > l.max_items {
        return Err(ParseError::LimitExceeded("items"));
    }
    let mut stations = Vec::new();
    for s in ns {
        let mut periods = Vec::new();
        for n in s.children.iter().filter(|n| n.name == "period") {
            let tu = attr(n, "time-utc").ok_or(ParseError::Product("observation time missing"))?;
            let tl = attr(n, "time-local")
                .ok_or(ParseError::Product("local observation time missing"))?;
            let time = date(&tu, "observation time")?;
            date(&tl, "local observation time")?;
            let mut vals = Vec::new();
            let mut levels = Vec::new();
            descendants(n, "level", &mut levels);
            for level in levels {
                for e in level.children.iter().filter(|e| e.name == "element") {
                    let kind =
                        attr(e, "type").ok_or(ParseError::Product("element type missing"))?;
                    if !e.text.is_empty()
                        && attr(e, "units").as_deref() != Some("knots")
                        && !kind.ends_with("_dir")
                        && kind != "cloud"
                    {
                        let number =
                            e.text
                                .parse::<f64>()
                                .map_err(|_| ParseError::InvalidValue {
                                    field: kind.clone(),
                                    value: e.text.clone(),
                                })?;
                        if !number.is_finite() {
                            return Err(ParseError::InvalidValue {
                                field: kind.clone(),
                                value: e.text.clone(),
                            });
                        }
                    }
                    vals.push(Value {
                        kind,
                        units: attr(e, "units"),
                        value: if e.text.is_empty() {
                            None
                        } else {
                            Some(e.text.clone())
                        },
                    });
                }
            }
            periods.push(ObservationPeriod {
                time_utc: time,
                time_local: tl,
                elements: vals,
            });
        }
        stations.push(Station {
            bom_id: attr(s, "bom-id").ok_or(ParseError::Product("station bom-id missing"))?,
            wmo_id: attr(s, "wmo-id"),
            name: attr(s, "stn-name").unwrap_or_default(),
            timezone: attr(s, "tz"),
            periods,
        });
    }
    Ok(Observations {
        provenance: p,
        stations,
    })
}
#[cfg(test)]
mod tests;

/// The five entities XML predefines; every other entity needs a DTD, which is
/// rejected, so an unknown reference is an error rather than silently dropped.
fn predefined_entity(name: &str) -> Option<char> {
    match name {
        "lt" => Some('<'),
        "gt" => Some('>'),
        "amp" => Some('&'),
        "apos" => Some('\''),
        "quot" => Some('"'),
        _ => None,
    }
}
