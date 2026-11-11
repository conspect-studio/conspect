//! Optional per-source EPG (`EpgSource`) and IPTV playlist presentation
//! metadata (`PlaylistMetadata`) — schema, backward compatibility, and the
//! per-item validation wired into `Source::validate` / `MultiviewConfig::validate`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use multiview_config::{EpgMode, EpgSource, MultiviewConfig, PlaylistMetadata, Source};

/// A minimal legacy source document (no `epg` / `playlist_metadata`) in
/// canonical JSON.
const LEGACY_SOURCE_JSON: &str = r#"{"id":"legacy","kind":"rtsp","url":"rtsp://[2001:db8::1]/x"}"#;

/// The same document in TOML.
const LEGACY_SOURCE_TOML: &str =
    "id = \"legacy\"\nkind = \"rtsp\"\nurl = \"rtsp://[2001:db8::1]/x\"\n";

fn source(json: serde_json::Value) -> Source {
    serde_json::from_value(json).expect("structurally valid source")
}

fn parses(json: serde_json::Value) -> Result<Source, serde_json::Error> {
    serde_json::from_value(json)
}

// ---------------------------------------------------------------------------
// Backward compatibility: absent by default, omitted when unset
// ---------------------------------------------------------------------------

#[test]
fn legacy_source_json_has_no_epg_and_re_serializes_unchanged() {
    let src: Source = serde_json::from_str(LEGACY_SOURCE_JSON).expect("legacy source parses");
    assert!(src.epg.is_none(), "legacy source has no epg");
    assert!(
        src.playlist_metadata.is_none(),
        "legacy source has no playlist_metadata"
    );

    let back = serde_json::to_string(&src).expect("serializes");
    assert_eq!(back, LEGACY_SOURCE_JSON, "no new keys in the wire form");
    assert!(!back.contains("epg"), "{back}");
    assert!(!back.contains("playlist_metadata"), "{back}");
}

#[test]
fn legacy_source_toml_has_no_epg_and_re_serializes_unchanged() {
    let src: Source = toml::from_str(LEGACY_SOURCE_TOML).expect("legacy source parses");
    assert!(src.epg.is_none());
    assert!(src.playlist_metadata.is_none());

    let back = toml::to_string(&src).expect("serializes");
    for key in ["epg", "playlist_metadata", "allow_private_network"] {
        assert!(
            !back.contains(key),
            "{key} must be omitted when unset: {back}"
        );
    }
}

#[test]
fn empty_epg_table_defaults_to_automatic_with_no_url() {
    let src = source(serde_json::json!({ "id": "s", "kind": "bars", "epg": {} }));
    let epg = src.epg.as_ref().expect("epg present");
    assert_eq!(epg.mode, EpgMode::Automatic);
    assert!(epg.url.is_none());
    assert!(epg.channel_id.is_none());
    assert!(epg.secret_ref.is_none());
    assert!(!epg.allow_private_network, "default is false");

    // Defaults are omitted again on the way out.
    let back = serde_json::to_value(&src).expect("serializes");
    let epg_json = &back["epg"];
    assert_eq!(epg_json["mode"], "automatic");
    assert!(epg_json.get("url").is_none(), "{epg_json}");
    assert!(
        epg_json.get("allow_private_network").is_none(),
        "{epg_json}"
    );
}

#[test]
fn full_epg_block_round_trips_through_json_and_toml() {
    let json = serde_json::json!({
        "id": "s",
        "kind": "hls",
        "url": "https://cdn.example.net/live.m3u8",
        "epg": {
            "mode": "manual",
            "url": "https://guide.example.net/xmltv.xml",
            "channel_id": "bbc1.uk",
            "secret_ref": "op://Broadcast/guide/token",
            "allow_private_network": true
        },
        "playlist_metadata": {
            "tvg_id": "bbc1.uk",
            "name": "BBC One",
            "logo": "https://logos.example.net/bbc1.png",
            "group": "UK Terrestrial"
        }
    });
    let src = source(json);
    src.validate().expect("fully configured source is valid");

    let epg: &EpgSource = src.epg.as_ref().expect("epg");
    assert_eq!(epg.mode, EpgMode::Manual);
    assert_eq!(epg.channel_id.as_deref(), Some("bbc1.uk"));
    assert!(epg.allow_private_network);
    let meta: &PlaylistMetadata = src.playlist_metadata.as_ref().expect("metadata");
    assert_eq!(meta.name.as_deref(), Some("BBC One"));

    // TOML round-trip keeps every nested value.
    let toml_text = toml::to_string(&src).expect("serializes to TOML");
    let back: Source = toml::from_str(&toml_text).expect("re-parses");
    assert_eq!(back, src, "TOML round-trip is lossless");
    let json_text = serde_json::to_string(&src).expect("serializes to JSON");
    let back: Source = serde_json::from_str(&json_text).expect("re-parses");
    assert_eq!(back, src, "JSON round-trip is lossless");
}

#[test]
fn unknown_epg_mode_is_rejected() {
    let err =
        parses(serde_json::json!({ "id": "s", "kind": "bars", "epg": { "mode": "sometimes" } }))
            .expect_err("unknown mode must not deserialize");
    assert!(err.to_string().contains("sometimes"), "{err}");
}

#[test]
fn unknown_epg_key_is_rejected() {
    let err = parses(serde_json::json!({
        "id": "s", "kind": "bars", "epg": { "urls": "https://x/y.xml" }
    }))
    .expect_err("unknown epg key must not deserialize");
    let text = err.to_string();
    assert!(text.contains("urls") || text.contains("unknown"), "{text}");
}

// ---------------------------------------------------------------------------
// Validation: mode-dependent requirements
// ---------------------------------------------------------------------------

#[test]
fn manual_mode_requires_url_and_channel_id() {
    let err = source(serde_json::json!({
        "id": "s", "kind": "bars",
        "epg": { "mode": "manual", "channel_id": "bbc1.uk" }
    }))
    .validate()
    .expect_err("manual without url must fail");
    let text = err.to_string();
    assert!(text.contains("manual") && text.contains("url"), "{text}");

    let err = source(serde_json::json!({
        "id": "s", "kind": "bars",
        "epg": { "mode": "manual", "url": "https://guide.example.net/x.xml" }
    }))
    .validate()
    .expect_err("manual without channel_id must fail");
    assert!(err.to_string().contains("channel_id"), "{err}");

    source(serde_json::json!({
        "id": "s", "kind": "bars",
        "epg": {
            "mode": "manual",
            "url": "https://guide.example.net/x.xml",
            "channel_id": "bbc1.uk"
        }
    }))
    .validate()
    .expect("manual with url + channel_id is valid");
}

#[test]
fn automatic_mode_needs_no_url() {
    source(serde_json::json!({ "id": "s", "kind": "bars", "epg": { "mode": "automatic" } }))
        .validate()
        .expect("automatic needs no url");
}

#[test]
fn disabled_mode_retains_previously_configured_settings() {
    // Disabling keeps the authored settings for a later re-enable — never an
    // error for retaining them.
    let src = source(serde_json::json!({
        "id": "s", "kind": "bars",
        "epg": {
            "mode": "disabled",
            "url": "https://guide.example.net/x.xml",
            "channel_id": "bbc1.uk",
            "secret_ref": "op://Broadcast/guide/token"
        }
    }));
    src.validate().expect("disabled may retain settings");
    let epg = src.epg.expect("epg");
    assert_eq!(epg.mode, EpgMode::Disabled);
    assert!(epg.url.is_some() && epg.channel_id.is_some());
}

#[test]
fn disabled_mode_with_nothing_set_is_valid() {
    source(serde_json::json!({ "id": "s", "kind": "bars", "epg": { "mode": "disabled" } }))
        .validate()
        .expect("disabled with nothing set is valid");
}

// ---------------------------------------------------------------------------
// Validation: URL shape and credentials
// ---------------------------------------------------------------------------

#[test]
fn epg_url_must_be_http_or_https() {
    for url in [
        "ftp://guide.example.net/x.xml",
        "file:///etc/passwd",
        "/local/x.xml",
    ] {
        let err = source(serde_json::json!({
            "id": "s", "kind": "bars",
            "epg": { "mode": "automatic", "url": url }
        }))
        .validate()
        .expect_err("non-http(s) url must fail");
        let text = err.to_string();
        assert!(text.contains("http"), "{url}: {text}");
    }
}

#[test]
fn epg_url_embedding_credentials_is_rejected() {
    let err = source(serde_json::json!({
        "id": "s", "kind": "bars",
        "epg": { "mode": "automatic", "url": "https://user:pass@guide.example.net/x.xml" }
    }))
    .validate()
    .expect_err("credentials in the EPG url must fail");
    let text = err.to_string();
    assert!(
        text.contains("credential") || text.contains("userinfo"),
        "{text}"
    );
    assert!(
        text.contains("secret_ref"),
        "must point at secret_ref: {text}"
    );
}

#[test]
fn epg_url_without_host_is_rejected() {
    let err = source(serde_json::json!({
        "id": "s", "kind": "bars",
        "epg": { "mode": "automatic", "url": "https:///x.xml" }
    }))
    .validate()
    .expect_err("url without a host must fail");
    assert!(err.to_string().contains("host"), "{err}");
}

#[test]
fn epg_secret_ref_must_be_present_when_set() {
    let err = source(serde_json::json!({
        "id": "s", "kind": "bars",
        "epg": { "mode": "automatic", "secret_ref": "   " }
    }))
    .validate()
    .expect_err("blank secret_ref must fail");
    assert!(err.to_string().contains("secret_ref"), "{err}");
}

#[test]
fn epg_url_private_network_allowed_flag_defaults_false_and_round_trips() {
    let src = source(serde_json::json!({
        "id": "s", "kind": "bars",
        "epg": { "mode": "automatic", "url": "http://10.0.0.5/x.xml", "allow_private_network": true }
    }));
    src.validate()
        .expect("http on a private host is authorable");
    assert!(src.epg.expect("epg").allow_private_network);
}

// ---------------------------------------------------------------------------
// Validation: bounded lengths and control characters
// ---------------------------------------------------------------------------

#[test]
fn epg_url_over_the_length_bound_is_rejected() {
    let long = format!(
        "https://guide.example.net/{}",
        "a".repeat(EpgSource::URL_MAX_BYTES)
    );
    let err = source(serde_json::json!({
        "id": "s", "kind": "bars",
        "epg": { "mode": "automatic", "url": long }
    }))
    .validate()
    .expect_err("over-long url must fail");
    let text = err.to_string();
    assert!(text.contains("url") && text.contains("bytes"), "{text}");
}

#[test]
fn epg_channel_id_over_the_length_bound_is_rejected() {
    let long = "c".repeat(EpgSource::CHANNEL_ID_MAX_BYTES + 1);
    let err = source(serde_json::json!({
        "id": "s", "kind": "bars",
        "epg": { "mode": "automatic", "channel_id": long }
    }))
    .validate()
    .expect_err("over-long channel_id must fail");
    let text = err.to_string();
    assert!(
        text.contains("channel_id") && text.contains("bytes"),
        "{text}"
    );
}

#[test]
fn epg_secret_ref_over_the_length_bound_is_rejected() {
    let long = "s".repeat(EpgSource::SECRET_REF_MAX_BYTES + 1);
    let err = source(serde_json::json!({
        "id": "s", "kind": "bars",
        "epg": { "mode": "automatic", "secret_ref": long }
    }))
    .validate()
    .expect_err("over-long secret_ref must fail");
    let text = err.to_string();
    assert!(
        text.contains("secret_ref") && text.contains("bytes"),
        "{text}"
    );
}

#[test]
fn epg_fields_reject_control_characters() {
    let cases: [(&str, serde_json::Value); 4] = [
        (
            "url",
            serde_json::json!("https://guide.example.net/x\n.xml"),
        ),
        ("channel_id", serde_json::json!("bbc1\n.uk")),
        ("channel_id", serde_json::json!("bbc1\u{7f}.uk")),
        (
            "secret_ref",
            serde_json::json!("op://Broadcast/guide/token\t"),
        ),
    ];
    for (field, value) in cases {
        let mut epg = serde_json::Map::new();
        epg.insert(field.to_owned(), value.clone());
        let err = source(serde_json::json!({ "id": "s", "kind": "bars", "epg": epg }))
            .validate()
            .expect_err(&format!("{field} = {value} unexpectedly valid"));
        let text = err.to_string();
        assert!(text.contains("control"), "{field} = {value}: {text}");
    }
}

#[test]
fn valid_channel_id_characters_are_accepted() {
    for id in ["bbc1.uk", "BBC One HD", "news-24", "a.b_c-d e"] {
        source(serde_json::json!({
            "id": "s", "kind": "bars",
            "epg": { "mode": "automatic", "channel_id": id }
        }))
        .validate()
        .unwrap_or_else(|e| panic!("{id:?} should be a valid channel id: {e}"));
    }
}

// ---------------------------------------------------------------------------
// Playlist presentation metadata
// ---------------------------------------------------------------------------

#[test]
fn playlist_metadata_validates_logo_url_and_bounds() {
    source(serde_json::json!({
        "id": "s", "kind": "bars",
        "playlist_metadata": {
            "tvg_id": "bbc1.uk",
            "name": "BBC One",
            "logo": "https://logos.example.net/bbc1.png",
            "group": "UK Terrestrial"
        }
    }))
    .validate()
    .expect("well-formed metadata is valid");

    let err = source(serde_json::json!({
        "id": "s", "kind": "bars",
        "playlist_metadata": { "logo": "not a url" }
    }))
    .validate()
    .expect_err("a non-http(s) logo must fail");
    assert!(err.to_string().contains("http"), "{err}");

    let err = source(serde_json::json!({
        "id": "s", "kind": "bars",
        "playlist_metadata": { "logo": "https://user:pass@logos.example.net/x.png" }
    }))
    .validate()
    .expect_err("a logo with embedded credentials must fail");
    let text = err.to_string();
    assert!(
        text.contains("credential") || text.contains("userinfo"),
        "{text}"
    );
}

#[test]
fn playlist_metadata_bounded_lengths_and_control_characters() {
    let long = "n".repeat(PlaylistMetadata::NAME_MAX_BYTES + 1);
    let err = source(serde_json::json!({
        "id": "s", "kind": "bars",
        "playlist_metadata": { "name": long }
    }))
    .validate()
    .expect_err("over-long name must fail");
    assert!(err.to_string().contains("name"), "{err}");

    let long = "g".repeat(PlaylistMetadata::GROUP_MAX_BYTES + 1);
    let err = source(serde_json::json!({
        "id": "s", "kind": "bars",
        "playlist_metadata": { "tvg_id": "x", "group": long }
    }))
    .validate()
    .expect_err("over-long group must fail");
    assert!(err.to_string().contains("group"), "{err}");

    let long = "t".repeat(PlaylistMetadata::TVG_ID_MAX_BYTES + 1);
    let err = source(serde_json::json!({
        "id": "s", "kind": "bars",
        "playlist_metadata": { "tvg_id": long }
    }))
    .validate()
    .expect_err("over-long tvg_id must fail");
    assert!(err.to_string().contains("tvg_id"), "{err}");

    let err = source(serde_json::json!({
        "id": "s", "kind": "bars",
        "playlist_metadata": { "name": "BBC\u{1} One" }
    }))
    .validate()
    .expect_err("control characters in metadata must fail");
    assert!(err.to_string().contains("control"), "{err}");
}

#[test]
fn blank_presentation_fields_are_rejected() {
    for field in ["tvg_id", "name", "group"] {
        let mut meta = serde_json::Map::new();
        meta.insert(field.to_owned(), serde_json::json!("  "));
        let err =
            source(serde_json::json!({ "id": "s", "kind": "bars", "playlist_metadata": meta }))
                .validate()
                .expect_err("a blank presentation field must fail");
        let text = err.to_string();
        assert!(text.contains(field), "{field}: {text}");
    }
}

#[test]
fn presentation_metadata_does_not_change_media_identity_or_kind() {
    let plain = source(serde_json::json!({
        "id": "cam", "kind": "rtsp", "url": "rtsp://[2001:db8::1]/live"
    }));
    let decorated = source(serde_json::json!({
        "id": "cam", "kind": "rtsp", "url": "rtsp://[2001:db8::1]/live",
        "epg": {
            "mode": "manual",
            "url": "https://guide.example.net/x.xml",
            "channel_id": "cam.hd"
        },
        "playlist_metadata": { "tvg_id": "cam.hd", "name": "Studio Cam", "group": "Studio" }
    }));

    // Identity (`id` + ingest payload) is untouched by presentation metadata.
    assert_eq!(decorated.id, plain.id);
    assert_eq!(decorated.kind, plain.kind);
    assert_eq!(decorated.auth, plain.auth);

    // The kind payload itself carries no EPG/presentation leakage.
    let kind_json = serde_json::to_string(&plain.kind).expect("kind serializes");
    for leak in ["tvg_id", "xmltv", "epg", "playlist"] {
        assert!(!kind_json.contains(leak), "{leak} leaked into {kind_json}");
    }
}

// ---------------------------------------------------------------------------
// The document-level path actually wires the new per-source checks
// ---------------------------------------------------------------------------

/// A structurally complete one-source grid document, with `extra_source_body`
/// appended to the `[[sources]]` table (TOML lines).
fn doc_with_source(extra_source_body: &str) -> String {
    format!(
        r##"schema_version = 1

[canvas]
width = 1920
height = 1080
fps = "30000/1001"
pixel_format = "nv12"
background = "#101014"

[canvas.color]
profile = "sdr-bt709-limited"

[layout]
kind = "grid"
columns = ["1fr"]
rows = ["1fr"]
gap = 8
areas = ["a"]

[[sources]]
id = "in_a"
kind = "test"
{extra_source_body}

[[cells]]
id = "cell_a"
area = "a"
fit = "contain"
[cells.source]
input_id = "in_a"

[[outputs]]
kind = "rtsp_server"
mount = "/multiview"
codec = "h264"
"##
    )
}

#[test]
fn document_validate_rejects_a_bad_epg_block() {
    let doc = doc_with_source(
        "\n[sources.epg]\nmode = \"manual\"\nurl = \"https://guide.example.net/x.xml\"\n",
    );
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parses");
    let err = cfg
        .validate()
        .expect_err("document validate must run the per-source EPG checks");
    assert!(err.to_string().contains("channel_id"), "{err}");
}

#[test]
fn document_validate_accepts_a_good_epg_block() {
    let doc = doc_with_source(
        "\n[sources.epg]\nmode = \"manual\"\nurl = \"https://guide.example.net/x.xml\"\nchannel_id = \"in_a\"\n\n[sources.playlist_metadata]\ntvg_id = \"in_a\"\nname = \"Input A\"\n",
    );
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parses");
    cfg.validate().expect("valid document with EPG validates");
}

#[test]
fn document_validate_rejects_a_bad_playlist_metadata_logo() {
    let doc = doc_with_source("\n[sources.playlist_metadata]\nlogo = \"ftp://logos/x.png\"\n");
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parses");
    let err = cfg
        .validate()
        .expect_err("document validate must run the metadata checks");
    assert!(err.to_string().contains("logo"), "{err}");
}

// ---------------------------------------------------------------------------
// Constants are part of the public contract
// ---------------------------------------------------------------------------

#[test]
#[allow(clippy::assertions_on_constants)]
fn public_bounds_are_sane() {
    assert!(EpgSource::URL_MAX_BYTES >= 512);
    assert!(EpgSource::CHANNEL_ID_MAX_BYTES >= 64);
    assert!(EpgSource::SECRET_REF_MAX_BYTES >= 64);
    assert!(PlaylistMetadata::TVG_ID_MAX_BYTES >= 64);
    assert!(PlaylistMetadata::NAME_MAX_BYTES >= 64);
    assert!(PlaylistMetadata::LOGO_URL_MAX_BYTES >= 512);
    assert!(PlaylistMetadata::GROUP_MAX_BYTES >= 64);
    assert_eq!(EpgMode::default(), EpgMode::Automatic);
}
