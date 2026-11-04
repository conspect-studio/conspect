//! HLS **master** (multivariant) playlist generation.
//!
//! A [`MasterPlaylist`] lists the available renditions; each [`VariantStream`]
//! renders one `#EXT-X-STREAM-INF` tag (peak bandwidth, optional average
//! bandwidth, codecs, resolution, frame rate) followed by its playlist URI.
//!
//! This is the top of the encode-once-mux-many tree (invariant #7): one master
//! references N renditions, and a *separate encode exists only* where codec,
//! resolution, or bitrate differ — never per tile.
use std::fmt::Write as _;

fn escape_attribute(value: &str) -> String {
    value.replace(['"', '\r', '\n'], "")
}

/// One variant (rendition) entry in a master playlist.
#[derive(Debug, Clone)]
pub struct VariantStream {
    uri: String,
    bandwidth: u64,
    average_bandwidth: Option<u64>,
    codecs: Option<String>,
    resolution: Option<(u32, u32)>,
    frame_rate: Option<f64>,
}

impl VariantStream {
    /// Construct a variant with its media-playlist URI and peak bandwidth
    /// (`BANDWIDTH`, bits per second — a required `EXT-X-STREAM-INF` attribute).
    #[must_use]
    pub fn new(uri: impl Into<String>, bandwidth: u64) -> Self {
        Self {
            uri: uri.into(),
            bandwidth,
            average_bandwidth: None,
            codecs: None,
            resolution: None,
            frame_rate: None,
        }
    }

    /// Set the `AVERAGE-BANDWIDTH` (bits per second).
    #[must_use]
    pub fn with_average_bandwidth(mut self, bandwidth: u64) -> Self {
        self.average_bandwidth = Some(bandwidth);
        self
    }

    /// Set the `CODECS` attribute (RFC 6381 codec string, e.g.
    /// `"avc1.640028,mp4a.40.2"`).
    #[must_use]
    pub fn with_codecs(mut self, codecs: impl Into<String>) -> Self {
        self.codecs = Some(codecs.into());
        self
    }

    /// Set the `RESOLUTION` attribute (decimal `WIDTHxHEIGHT`).
    #[must_use]
    pub fn with_resolution(mut self, width: u32, height: u32) -> Self {
        self.resolution = Some((width, height));
        self
    }

    /// Set the `FRAME-RATE` attribute. Rendered with three decimal places.
    #[must_use]
    pub fn with_frame_rate(mut self, fps: f64) -> Self {
        self.frame_rate = Some(fps);
        self
    }

    /// Render the `#EXT-X-STREAM-INF` line (without the following URI line).
    fn render_stream_inf(&self) -> String {
        let mut attrs: Vec<String> = vec![format!("BANDWIDTH={}", self.bandwidth)];
        if let Some(avg) = self.average_bandwidth {
            attrs.push(format!("AVERAGE-BANDWIDTH={avg}"));
        }
        if let Some(codecs) = &self.codecs {
            attrs.push(format!("CODECS=\"{codecs}\""));
        }
        if let Some((w, h)) = self.resolution {
            attrs.push(format!("RESOLUTION={w}x{h}"));
        }
        if let Some(fps) = self.frame_rate {
            attrs.push(format!("FRAME-RATE={fps:.3}"));
        }
        format!("#EXT-X-STREAM-INF:{}", attrs.join(","))
    }
}

/// An HLS master (multivariant) playlist builder + renderer.
#[derive(Debug, Clone, Default)]
pub struct MasterPlaylist {
    variants: Vec<VariantStream>,
    audio: Vec<AudioRendition>,
}

/// A separately playable audio-only rendition in an HLS audio group.
#[derive(Debug, Clone)]
pub struct AudioRendition {
    /// Human-readable player menu label.
    pub name: String,
    /// Media playlist URI relative to the master.
    pub uri: String,
    /// Optional BCP-47 language tag.
    pub language: Option<String>,
    /// Whether this is the selected audio on first playback.
    pub default: bool,
}

impl MasterPlaylist {
    /// Create an empty master playlist.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a variant rendition (renders in insertion order).
    pub fn push_variant(&mut self, variant: VariantStream) {
        self.variants.push(variant);
    }

    /// Add a separately playable audio-only media playlist.
    pub fn push_audio(&mut self, rendition: AudioRendition) {
        self.audio.push(rendition);
    }

    /// Number of variants listed.
    #[must_use]
    pub fn variant_count(&self) -> usize {
        self.variants.len()
    }

    /// Render the master playlist to its exact UTF-8 manifest text.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        // `writeln!` into a `String` is infallible; discard the `Result`.
        let _ = writeln!(out, "#EXTM3U");
        let _ = writeln!(out, "#EXT-X-VERSION:7");
        let _ = writeln!(out, "#EXT-X-INDEPENDENT-SEGMENTS");
        for audio in &self.audio {
            let language = audio.language.as_ref().map_or_else(String::new, |tag| {
                format!(",LANGUAGE=\"{}\"", escape_attribute(tag))
            });
            let _ = writeln!(
                out,
                "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"{}\",DEFAULT={},AUTOSELECT={},URI=\"{}\"{}",
                escape_attribute(&audio.name),
                if audio.default { "YES" } else { "NO" },
                if audio.default { "YES" } else { "NO" },
                escape_attribute(&audio.uri),
                language
            );
        }
        for variant in &self.variants {
            let mut inf = variant.render_stream_inf();
            if !self.audio.is_empty() {
                inf.push_str(",AUDIO=\"audio\"");
            }
            let _ = writeln!(out, "{inf}");
            let _ = writeln!(out, "{}", variant.uri);
        }
        out
    }
}
