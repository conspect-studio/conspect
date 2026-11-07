use crate::{parse_forecast, parse_forecast_with_limits, parse_observations, ParseError};

// Synthetic minimal documents shaped from inspected BOM v1.7 examples; not actual BOM output.
const FORECAST: &str = r#"<product version="1.7"><amoc><identifier>IDN11060</identifier><issue-time-utc>2026-01-01T00:00:00Z</issue-time-utc><expiry-time>2026-01-02T00:00:00Z</expiry-time></amoc><forecast><area aac="NSW_PT131" description="Sydney" type="location"><forecast-period index="0" start-time-local="2026-01-01T10:00:00+11:00" end-time-local="2026-01-02T00:00:00+11:00" start-time-utc="2025-12-31T23:00:00Z" end-time-utc="2026-01-01T13:00:00Z"><element type="forecast_icon_code">998</element><element type="air_temperature_maximum" units="Celsius">25</element><text type="precis">Sunny.</text></forecast-period></area></forecast></product>"#;
const OBS: &str = r#"<product version="v1.7.1"><amoc><identifier>IDN60920</identifier><issue-time-utc>2026-01-01T00:00:00Z</issue-time-utc></amoc><observations><station bom-id="066214" wmo-id="94768" tz="Australia/Sydney" stn-name="Sydney" lat="-33.8" lon="151.2"><period index="0" time-utc="2026-01-01T00:00:00Z" time-local="2026-01-01T11:00:00+11:00"><level index="0" type="surface"><element type="air_temperature" units="Celsius">22.6</element></level></period></station></observations></product>"#;

#[test]
fn forecast_preserves_identity_expiry_values_and_unknown_icon() {
    let p = parse_forecast(FORECAST.as_bytes()).unwrap();
    assert_eq!(p.product_id, "IDN11060");
    assert_eq!(p.areas[0].aac, "NSW_PT131");
    assert_eq!(p.areas[0].periods[0].icon_code.as_deref(), Some("998"));
    assert_eq!(
        p.areas[0].periods[0].elements[1].value.as_deref(),
        Some("25")
    );
    assert!(p.provenance.expires.is_some());
}

#[test]
fn observation_preserves_station_identity_and_values() {
    let p = parse_observations(OBS.as_bytes()).unwrap();
    assert_eq!(p.stations[0].bom_id, "066214");
    assert_eq!(
        p.stations[0].periods[0].elements[0].value.as_deref(),
        Some("22.6")
    );
}

#[test]
fn invalid_numeric_time_dtd_and_malformed_xml_are_rejected() {
    for invalid in ["wat", "NaN"] {
        let bad = FORECAST.replace(">25</element>", &format!(">{invalid}</element>"));
        assert!(matches!(
            parse_forecast(bad.as_bytes()),
            Err(ParseError::InvalidValue { .. })
        ));
    }
    let bad_time = FORECAST.replace("2025-12-31T23:00:00Z", "yesterday");
    assert!(matches!(
        parse_forecast(bad_time.as_bytes()),
        Err(ParseError::InvalidValue { .. })
    ));
    assert!(parse_forecast(
        b"<!DOCTYPE product [<!ENTITY x SYSTEM 'file:///etc/passwd'>]><product/>"
    )
    .is_err());
    assert!(parse_forecast(b"<product><amoc></product>").is_err());
}

#[test]
fn parser_enforces_each_resource_limit() {
    for limits in [
        crate::Limits {
            max_bytes: 4,
            ..crate::Limits::default()
        },
        crate::Limits {
            max_depth: 2,
            ..crate::Limits::default()
        },
        crate::Limits {
            max_nodes: 3,
            ..crate::Limits::default()
        },
        crate::Limits {
            max_text_bytes: 2,
            ..crate::Limits::default()
        },
        crate::Limits {
            max_items: 0,
            ..crate::Limits::default()
        },
    ] {
        assert!(matches!(
            parse_forecast_with_limits(FORECAST.as_bytes(), limits),
            Err(ParseError::LimitExceeded(_))
        ));
    }
}

#[test]
fn text_entities_resolve_and_undeclared_entities_are_rejected() {
    let doc = FORECAST.replace("Sunny.", "Sun &amp; cloud &#x2013; 30&#176;C &lt;5%");
    let p = parse_forecast(doc.as_bytes()).unwrap();
    let texts = format!("{:?}", p.areas[0].periods[0]);
    assert!(
        texts.contains("Sun & cloud \u{2013} 30\u{b0}C <5%"),
        "{texts}"
    );
    let bad = FORECAST.replace("Sunny.", "Sunny &bogus;");
    assert!(matches!(
        parse_forecast(bad.as_bytes()),
        Err(ParseError::Xml(_))
    ));
}
