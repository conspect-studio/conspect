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
fn observation_local_utc_instants_must_match() {
    let bad = OBS.replace("2026-01-01T11:00:00+11:00", "2026-01-01T11:00:00+10:00");
    assert!(matches!(
        parse_observations(bad.as_bytes()),
        Err(ParseError::InvalidValue { .. })
    ));
}

#[test]
fn observation_categorical_and_qualified_values_are_preserved() {
    let xml = OBS.replace(
        "<element type=\"air_temperature\" units=\"Celsius\">22.6</element>",
        "<element type=\"air_temperature\" units=\"Celsius\">22.6</element><element type=\"weather\">Fine</element><element type=\"sea_height\" units=\"m\">&lt;0.5</element><element type=\"unknown_field\">opaque</element><element type=\"rainfall\" units=\"mm\"></element>",
    );
    let p = parse_observations(xml.as_bytes()).unwrap();
    let values = &p.stations[0].periods[0].elements;
    assert_eq!(values[1].value.as_deref(), Some("Fine"));
    assert_eq!(values[2].value.as_deref(), Some("<0.5"));
    assert_eq!(values[3].value.as_deref(), Some("opaque"));
    assert_eq!(values[4].value, None);
}

#[test]
fn forecast_local_utc_instants_must_match() {
    let bad = FORECAST.replace("2026-01-01T10:00:00+11:00", "2026-01-01T10:00:00+10:00");
    assert!(matches!(
        parse_forecast(bad.as_bytes()),
        Err(ParseError::InvalidValue { .. })
    ));
    let parsed = parse_forecast(FORECAST.as_bytes()).unwrap();
    assert_eq!(
        parsed.areas[0].periods[0].temperature(),
        Some(("Celsius", 25.0))
    );
}

#[test]
fn documented_regional_product_ids_are_accepted_and_wrong_kind_rejected() {
    for id in [
        "IDV10753", "IDQ11295", "IDS10044", "IDW14199", "IDT16710", "IDD10207",
    ] {
        let xml = FORECAST.replace("IDN11060", id);
        assert_eq!(parse_forecast(xml.as_bytes()).unwrap().product_id, id);
    }
    for id in [
        "IDV60920", "IDQ60920", "IDS60920", "IDW60920", "IDT60920", "IDD60920",
    ] {
        let xml = OBS.replace("IDN60920", id);
        assert_eq!(
            parse_observations(xml.as_bytes())
                .unwrap()
                .provenance
                .product_id,
            id
        );
    }
    assert!(parse_observations(FORECAST.as_bytes()).is_err());
}

#[test]
fn known_numeric_observations_reject_non_numeric_but_allow_zero() {
    let zero = OBS.replace(">22.6</element>", ">0</element>");
    let parsed = parse_observations(zero.as_bytes()).unwrap();
    assert_eq!(
        parsed.stations[0].periods[0].elements[0].value.as_deref(),
        Some("0")
    );
    assert_eq!(
        parsed.stations[0].periods[0].temperature(),
        Some(("Celsius", 0.0))
    );
    let missing = OBS.replace(
        "<element type=\"air_temperature\" units=\"Celsius\">22.6</element>",
        "<element type=\"air_temperature\" units=\"Celsius\"></element>",
    );
    assert_eq!(
        parse_observations(missing.as_bytes()).unwrap().stations[0].periods[0].temperature(),
        None
    );
    let invalid = OBS.replace(">22.6</element>", ">wat</element>");
    assert!(matches!(
        parse_observations(invalid.as_bytes()),
        Err(ParseError::InvalidValue { .. })
    ));
}

#[test]
fn spoofed_root_element_is_rejected() {
    let bad = FORECAST.replacen("<product", "<spoofed", 1);
    let bad = bad.replacen("</product>", "</spoofed>", 1);
    assert!(matches!(
        parse_forecast(bad.as_bytes()),
        Err(ParseError::Product("unexpected root element"))
    ));
}

#[test]
fn duplicate_measurement_elements_are_all_preserved_first_wins() {
    let xml = OBS.replace(
        "<element type=\"air_temperature\" units=\"Celsius\">22.6</element>",
        "<element type=\"air_temperature\" units=\"Celsius\">22.6</element><element type=\"air_temperature\" units=\"Celsius\">99.9</element>",
    );
    let p = parse_observations(xml.as_bytes()).unwrap();
    let elements = &p.stations[0].periods[0].elements;
    assert_eq!(elements.len(), 2);
    assert_eq!(elements[0].value.as_deref(), Some("22.6"));
    assert_eq!(elements[1].value.as_deref(), Some("99.9"));
    // measurement() documents first-match-wins when a kind is duplicated.
    assert_eq!(
        p.stations[0].periods[0].temperature(),
        Some(("Celsius", 22.6))
    );
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

#[test]
fn icon_codes_map_to_families_and_unknown_is_none() {
    use crate::{icon_for_code, WeatherIcon as I};
    let expect = [
        (1, Some(I::Sun)),
        (2, Some(I::Night)),
        (3, Some(I::PartCloud)),
        (4, Some(I::Cloud)),
        (5, None),
        (6, Some(I::Fog)),
        (7, None),
        (8, Some(I::Rain)),
        (9, Some(I::Wind)),
        (10, Some(I::Fog)),
        (11, Some(I::Showers)),
        (12, Some(I::Rain)),
        (13, Some(I::Wind)),
        (14, Some(I::Sun)),
        (15, Some(I::Showers)),
        (16, Some(I::Storm)),
        (17, Some(I::Showers)),
        (0, None),
        (18, None),
        (255, None),
    ];
    for (code, want) in expect {
        assert_eq!(icon_for_code(code), want, "code {code}");
    }
}

#[test]
#[allow(clippy::many_single_char_names)]
fn normalized_forecast_keeps_absent_values_absent() {
    let f = parse_forecast(FORECAST.as_bytes()).unwrap();
    let s = crate::normalize_forecast(&f, "NSW_PT131").unwrap();
    assert_eq!(s.days.len(), 1);
    let d = &s.days[0];
    assert_eq!(d.weekday, "Thu");
    assert_eq!(d.label, "Thu 1 Jan");
    assert_eq!(d.max_c, Some(25.0));
    assert_eq!(d.min_c, None);
    assert_eq!(d.rain_chance, None);
    assert_eq!(d.icon_code, None, "998 does not fit a u8 code");
    assert_eq!(s.issued_local.as_deref(), Some("11:00 am Thu 1 Jan"));
    assert!(crate::normalize_forecast(&f, "NSW_PT999").is_none());
    let o = parse_observations(OBS.as_bytes()).unwrap();
    let n = crate::normalize_observation(&o, "94768").unwrap();
    assert_eq!(n.air_temp_c, Some(22.6));
    assert_eq!(n.rel_humidity_pct, None);
    assert_eq!(n.time_display, "11:00 am");
}
