//! Local capture device sources (docs/architecture/local-capture-devices.md):
//! parse, structural validation, and backend lowering.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use multiview_config::{DeviceBackend, MultiviewConfig, Source, SourceKind};

fn config_with(source_toml: &str) -> String {
    format!(
        r##"
schema_version = 1

[canvas]
width = 1280
height = 720
fps = "30000/1001"
pixel_format = "nv12"
background = "#000000"

[canvas.color]
profile = "sdr-bt709-limited"

[layout]
kind = "grid"
columns = ["1fr"]
rows = ["1fr"]
gap = 0
areas = ["main"]

[[sources]]
{source_toml}

[[cells]]
id = "main"
area = "main"
fit = "contain"
[cells.source]
input_id = "cam"

[[outputs]]
kind = "hls"
path = "/tmp/device-test/index.m3u8"
codec = "mpeg2video"
"##
    )
}

#[test]
fn a_device_source_parses_with_all_options() {
    let src: Source = toml::from_str(
        r#"
id = "cam"
kind = "device"
backend = "avfoundation"
device = "0"
size = "1920x1080"
fps = "30000/1001"
pixel_format = "nv12"
"#,
    )
    .expect("valid device source");
    match &src.kind {
        SourceKind::Device {
            backend,
            device,
            size,
            fps,
            pixel_format,
        } => {
            assert_eq!(*backend, DeviceBackend::Avfoundation);
            assert_eq!(device, "0");
            assert_eq!(size.as_deref(), Some("1920x1080"));
            assert_eq!(fps.as_deref(), Some("30000/1001"));
            assert_eq!(pixel_format.as_deref(), Some("nv12"));
        }
        other => panic!("expected Device, got {other:?}"),
    }
}

#[test]
fn a_minimal_device_config_validates() {
    let cfg = MultiviewConfig::load_from_toml(&config_with(
        "id = \"cam\"\nkind = \"device\"\nbackend = \"dshow\"\ndevice = \"Integrated Camera\"",
    ))
    .expect("parses");
    cfg.validate().expect("a minimal device source is valid");
}

#[test]
fn malformed_device_options_are_rejected_at_config_time() {
    for (bad, why) in [
        ("device = \"\"", "empty device"),
        ("device = \"0:1\"", "':' would smuggle an audio selector"),
        ("device = \"0\"\nsize = \"1920\"", "size without height"),
        ("device = \"0\"\nsize = \"8x8\"", "size below the floor"),
        ("device = \"0\"\nfps = \"29.97\"", "float fps"),
        ("device = \"0\"\nfps = \"30000/0\"", "zero denominator"),
        (
            "device = \"0\"\npixel_format = \"nv12;rm\"",
            "non-token pixel format",
        ),
    ] {
        let toml = config_with(&format!(
            "id = \"cam\"\nkind = \"device\"\nbackend = \"avfoundation\"\n{bad}"
        ));
        let cfg = MultiviewConfig::load_from_toml(&toml).expect("parses");
        assert!(cfg.validate().is_err(), "must reject: {why}");
    }
}

#[test]
fn backends_lower_to_their_libavdevice_format_and_device_string() {
    assert_eq!(DeviceBackend::Avfoundation.input_format(), "avfoundation");
    assert_eq!(DeviceBackend::Avfoundation.device_url("0"), "0:none");
    assert_eq!(DeviceBackend::Dshow.input_format(), "dshow");
    assert_eq!(
        DeviceBackend::Dshow.device_url("USB Camera"),
        "video=USB Camera"
    );
}
