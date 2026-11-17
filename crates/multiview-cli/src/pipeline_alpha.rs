//! Media-player NV12+A: attach a decoded asset's straight alpha to its NV12
//! tile image (ADR-0058, #391; design in ADR-0112 §5 "PR B: alpha decode").
//!
//! The decoder ([`multiview_ffmpeg::StreamVideoDecoder`]) converts every frame
//! to NV12 exactly as before (same libswscale conversion, so the same matrix,
//! range and order) and, for an alpha pixel format, hands back the decoded
//! frame as an [`AlphaSource`]. [`MediaAlpha`] turns that into the image's
//! full-resolution R8 straight-alpha plane:
//!
//! 1. the alpha samples are copied once into a reused `GRAY8` frame at the
//!    decoded size (8-bit, round-to-nearest from 10/12/16-bit);
//! 2. when the tile size differs, that frame is scaled with the **same**
//!    geometry and filter as the luma (`TileScaler`: decoded size → tile size,
//!    Lanczos + accurate rounding) into a reused `GRAY8` frame — `GRAY8` to
//!    `GRAY8` has no colour or range step, so alpha is never range-mapped;
//! 3. the result is written straight into a plane leased from the bounded
//!    [`AlphaPlanePool`] (`attach_with`, one copy, no scratch plane).
//!
//! Opaque assets never reach this module: `DecodedVideoFrame::alpha` is
//! `None`, so no alpha plane, no scratch frame and no scaler exist.
//!
//! Every working buffer is reused across frames (rebuilt only on a geometry
//! change); the only per-frame plane is the pooled one, which returns to the
//! pool when the last holder of the image drops it.

use std::sync::Arc;

use ffmpeg_next::format::Pixel;
use multiview_compositor::pipeline::{AlphaPlanePool, Nv12Image};
use multiview_ffmpeg::alpha::{extract_alpha, AlphaDepth, AlphaLayout, AlphaSource};
use multiview_ffmpeg::frame::ScratchFrame;
use multiview_ffmpeg::{ScaleSpec, Scaler};

/// Per-player alpha attacher: the bounded plane pool plus the reused scratch
/// frames and scaler of the alpha path.
#[derive(Default)]
pub(super) struct MediaAlpha {
    pool: Arc<AlphaPlanePool>,
    /// The decoded-size `GRAY8` copy of the alpha (only when scaling).
    decoded: Option<ScratchFrame>,
    /// The tile-size `GRAY8` scaler output (only when scaling). A checked
    /// [`ScratchFrame`] from the first use on, never an unchecked
    /// `Video::empty()` (whose `av_frame_alloc` result is not null-checked).
    scaled: Option<ScratchFrame>,
    scaler: Option<Scaler>,
}

const GRAY_U8: AlphaLayout = AlphaLayout::planar(0, AlphaDepth::U8);

impl MediaAlpha {
    /// The pool this attacher leases alpha planes from.
    #[cfg(test)]
    pub(super) fn pool(&self) -> &Arc<AlphaPlanePool> {
        &self.pool
    }

    /// Attach `source`'s straight alpha, at `image`'s size, to `image`.
    ///
    /// # Errors
    /// A message when the alpha cannot be copied or scaled; the caller keeps
    /// the frame opaque-free by dropping it (never a panic on the decode
    /// thread).
    pub(super) fn attach(
        &mut self,
        image: Nv12Image,
        source: &AlphaSource,
    ) -> Result<Nv12Image, String> {
        let (in_w, in_h) = (source.frame().width(), source.frame().height());
        let (tile_w, tile_h) = (image.width(), image.height());
        let tile_cols = usize::try_from(tile_w).map_err(|_| "width overflow".to_owned())?;
        if (in_w, in_h) == (tile_w, tile_h) {
            // Same size: one copy from the decoded frame into the pooled plane.
            return self
                .pool
                .attach_with(image, |plane| {
                    source.copy_to(plane, tile_cols).map_err(geometry)
                })
                .map_err(|e| e.to_string());
        }
        // Different size: decoded alpha -> GRAY8 at decoded size, then the
        // luma's scaler geometry to the tile size.
        let decoded = reuse(&mut self.decoded, in_w, in_h)?;
        let (plane, stride) = decoded.plane_mut(0).map_err(|e| e.to_string())?;
        source.copy_to(plane, stride).map_err(|e| e.to_string())?;
        let decoded: &ScratchFrame = decoded;
        let from = ScaleSpec::new(Pixel::GRAY8, in_w, in_h);
        let to = ScaleSpec::new(Pixel::GRAY8, tile_w, tile_h);
        if self
            .scaler
            .as_ref()
            .is_none_or(|s| s.source() != from || s.destination() != to)
        {
            // `Scaler::new` is the luma tile scaler's filter (Lanczos +
            // accurate rounding), so alpha edges resample like luma edges.
            self.scaler = Some(Scaler::new(from, to).map_err(|e| e.to_string())?);
        }
        let scaler = self
            .scaler
            .as_mut()
            .ok_or_else(|| "alpha scaler unexpectedly absent".to_owned())?;
        let out = reuse(&mut self.scaled, tile_w, tile_h)?;
        scaler.run_into(decoded, out).map_err(|e| e.to_string())?;
        let (resized, resized_stride) = out.plane(0).map_err(|e| e.to_string())?;
        let tile_rows = usize::try_from(tile_h).map_err(|_| "height overflow".to_owned())?;
        self.pool
            .attach_with(image, |plane| {
                extract_alpha(
                    GRAY_U8,
                    resized,
                    resized_stride,
                    tile_cols,
                    tile_rows,
                    plane,
                    tile_cols,
                )
                .map_err(geometry)
            })
            .map_err(|e| e.to_string())
    }
}

/// The libav decoder a media player must open for `stream_index` to keep its
/// alpha, when the default decoder would drop it: `libvpx-vp9` for VP9 with
/// the `WebM` `alpha_mode=1` tag (libav's native `vp9` ignores the alpha side
/// stream). `None` for every other stream and for non-player sources (they
/// stay opaque, ADR-0058 §5). An alpha stream whose decoder is not linked
/// also yields `None` but logs a warning (ADR-0112 §5): it plays opaque.
pub(super) fn vp9_alpha_decoder(
    id: &str,
    is_player: bool,
    input: &ffmpeg_next::format::context::Input,
    stream_index: usize,
) -> Option<&'static str> {
    if !is_player {
        return None;
    }
    let stream = input.stream(stream_index)?;
    let vp9 = stream.parameters().id() == ffmpeg_next::codec::Id::VP9;
    let alpha = stream
        .metadata()
        .get("alpha_mode")
        .is_some_and(|v| v.trim() == "1");
    let linked = ffmpeg_next::decoder::find_by_name(LIBVPX_VP9).is_some();
    choose_vp9_alpha(
        id,
        StreamAlpha {
            player: is_player,
            vp9,
            alpha_mode: alpha,
        },
        linked,
    )
}

const LIBVPX_VP9: &str = "libvpx-vp9";

/// Whether a media player's stream needs (and can get) the alpha decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Vp9Alpha {
    /// Not a player VP9 alpha stream: the default decoder, no warning.
    NotAlpha,
    /// VP9 with alpha and `libvpx-vp9` linked: open it.
    Decoder(&'static str),
    /// VP9 with alpha but no alpha-capable decoder: plays opaque.
    NoAlphaDecoder,
}

/// What the demuxer says about a stream, for the alpha-decoder decision.
#[derive(Debug, Clone, Copy)]
struct StreamAlpha {
    /// The source is a media player.
    player: bool,
    /// The stream is VP9.
    vp9: bool,
    /// The `WebM` `alpha_mode=1` tag is present.
    alpha_mode: bool,
}

fn classify_vp9_alpha(stream: StreamAlpha, libvpx_linked: bool) -> Vp9Alpha {
    let alpha_stream = stream.player && stream.vp9 && stream.alpha_mode;
    match (alpha_stream, libvpx_linked) {
        (false, _) => Vp9Alpha::NotAlpha,
        (true, true) => Vp9Alpha::Decoder(LIBVPX_VP9),
        (true, false) => Vp9Alpha::NoAlphaDecoder,
    }
}

/// [`classify_vp9_alpha`] plus the ADR-0112 §5 warning, emitted once per
/// open (this runs once per `open_and_stream`, never per frame).
fn choose_vp9_alpha(id: &str, stream: StreamAlpha, libvpx_linked: bool) -> Option<&'static str> {
    match classify_vp9_alpha(stream, libvpx_linked) {
        Vp9Alpha::NotAlpha => None,
        Vp9Alpha::Decoder(name) => Some(name),
        Vp9Alpha::NoAlphaDecoder => {
            tracing::warn!(
                source = %id, decoder = LIBVPX_VP9,
                "VP9 alpha asset but no alpha-capable decoder is linked; the asset plays opaque"
            );
            None
        }
    }
}

/// Open the software decoder `name`, logging and returning `None` on failure
/// so the caller falls back to the default (opaque) decode.
pub(super) fn open_named(
    id: &str,
    params: ffmpeg_next::codec::Parameters,
    time_base: multiview_core::time::Rational,
    name: &'static str,
) -> Option<multiview_ffmpeg::StreamVideoDecoder> {
    match multiview_ffmpeg::StreamVideoDecoder::new_named(params, time_base, name) {
        Ok(decoder) => Some(decoder),
        Err(err) => {
            tracing::warn!(
                source = %id, decoder = name, error = %err,
                "alpha decoder unavailable; the asset plays opaque"
            );
            None
        }
    }
}

fn geometry(e: impl std::fmt::Display) -> multiview_compositor::error::Error {
    multiview_compositor::error::Error::Geometry(e.to_string())
}

/// The reused `GRAY8` frame in `slot`, reallocated only on a size change.
///
/// The allocation is checked ([`ScratchFrame::alloc`]): a null `AVFrame`, a
/// failed or oversized allocation is an error here, before any plane or
/// accessor could touch a frame without planes.
fn reuse(slot: &mut Option<ScratchFrame>, w: u32, h: u32) -> Result<&mut ScratchFrame, String> {
    if slot
        .as_ref()
        .is_some_and(|f| !f.matches(Pixel::GRAY8, w, h))
    {
        *slot = None;
    }
    if slot.is_none() {
        *slot = Some(ScratchFrame::alloc(Pixel::GRAY8, w, h).map_err(|e| e.to_string())?);
    }
    slot.as_mut()
        .ok_or_else(|| "alpha scratch frame unexpectedly absent".to_owned())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::as_conversions,
        clippy::cast_possible_truncation
    )]

    use super::*;
    use ffmpeg_next::util::frame::Video;
    use multiview_compositor::pipeline::CanvasColor;
    use multiview_ffmpeg::alpha::alpha_layout;

    fn tag() -> multiview_core::color::ColorInfo {
        CanvasColor::default().output_tag()
    }

    /// The decoder's own NV12 step (`StreamVideoDecoder` converts any non-NV12
    /// format to same-size NV12 with `Scaler::new`), then the player's tile
    /// scale, then the alpha attach — the exact player path minus libavcodec.
    fn player_path(
        frame: &Video,
        tile: (u32, u32),
        alpha: &mut MediaAlpha,
        to_tile: &mut super::super::TileScaler,
    ) -> Nv12Image {
        let mut to_nv12 = Scaler::new(
            ScaleSpec::new(frame.format(), frame.width(), frame.height()),
            ScaleSpec::new(Pixel::NV12, frame.width(), frame.height()),
        )
        .unwrap();
        let nv12 = to_nv12.run(frame).unwrap();
        assert_eq!((to_tile_size(to_tile)), tile);
        let image = to_tile.convert(&nv12, tag()).unwrap();
        let layout = alpha_layout(frame.format()).expect("alpha format");
        alpha.attach(image, &checked_source(frame, layout)).unwrap()
    }

    /// The decoder's alpha frame as an [`AlphaSource`]. Outside
    /// `multiview-ffmpeg` an `AlphaSource` can only wrap a checked
    /// [`ScratchFrame`] (review PB-14: an arbitrary `Video`'s geometry may
    /// have been widened over unwritten bytes), and a `ScratchFrame` is
    /// filled only through `plane_mut` (review PB-15: `Scaler::run_into`
    /// no longer takes a `Video` source). So copy every plane's active rows
    /// of the test frame (built here, all rows written) into one: the same
    /// pixels at the same format and size a decoder would hand over.
    fn checked_source(frame: &Video, layout: AlphaLayout) -> AlphaSource {
        let mut scratch =
            ScratchFrame::alloc(frame.format(), frame.width(), frame.height()).unwrap();
        for p in 0..frame.planes() {
            let rows = frame.plane_height(p) as usize;
            let src_stride = frame.stride(p);
            let (dst, dst_stride) = scratch.plane_mut(p).unwrap();
            // `plane_mut` spans `stride * (rows - 1) + active row bytes`.
            let active = dst.len() - dst_stride * (rows - 1);
            for y in 0..rows {
                dst[y * dst_stride..y * dst_stride + active]
                    .copy_from_slice(&frame.data(p)[y * src_stride..y * src_stride + active]);
            }
        }
        AlphaSource::from_checked(scratch, layout)
    }

    fn to_tile_size(t: &super::super::TileScaler) -> (u32, u32) {
        (t.tile_w, t.tile_h)
    }

    /// A `w x h` `yuva420p` frame: Y = 16 + 4x, mid chroma, A = 20 * (x + y).
    fn yuva420p(w: u32, h: u32) -> Video {
        let mut f = Video::new(Pixel::YUVA420P, w, h);
        for plane in 0..4 {
            let stride = f.stride(plane);
            let rows = f.plane_height(plane) as usize;
            let cols = f.plane_width(plane) as usize;
            let data = f.data_mut(plane);
            data.fill(0xEE);
            for y in 0..rows {
                for x in 0..cols {
                    data[y * stride + x] = match plane {
                        0 => 16 + 4 * x as u8,
                        1 | 2 => 128,
                        _ => (20 * (x + y)) as u8,
                    };
                }
            }
        }
        f
    }

    #[test]
    fn yuva420p_same_size_keeps_nv12_exact_and_alpha_exact() {
        let (w, h) = (6, 4);
        let frame = yuva420p(w, h);
        let mut alpha = MediaAlpha::default();
        let mut to_tile = super::super::TileScaler::new(w, h);
        let image = player_path(&frame, (w, h), &mut alpha, &mut to_tile);
        // Colour is exactly today's NV12 path: yuva420p -> NV12 is a repack.
        let y: Vec<u8> = (0..h)
            .flat_map(|_| (0..w).map(|x| 16 + 4 * x as u8))
            .collect();
        assert_eq!(image.y_plane(), &y[..]);
        assert!(image.uv_plane().iter().all(|&c| c == 128));
        let a: Vec<u8> = (0..h)
            .flat_map(|yy| (0..w).map(move |x| (20 * (x + yy)) as u8))
            .collect();
        assert_eq!(image.alpha_plane(), Some(&a[..]));
        assert_eq!(image.color(), tag());
    }

    #[test]
    fn yuva444p10_alpha_rescales_to_8_bit_exactly() {
        let (w, h) = (4, 2);
        let mut f = Video::new(Pixel::YUVA444P10LE, w, h);
        let samples: [u16; 4] = [0, 1023, 512, 256];
        for plane in 0..4 {
            let stride = f.stride(plane);
            let data = f.data_mut(plane);
            for y in 0..h as usize {
                for x in 0..w as usize {
                    let v: u16 = match plane {
                        0 => 64 + 100 * x as u16,
                        1 | 2 => 512,
                        _ => samples[x],
                    };
                    data[y * stride + 2 * x..y * stride + 2 * x + 2]
                        .copy_from_slice(&v.to_le_bytes());
                }
            }
        }
        let mut alpha = MediaAlpha::default();
        let mut to_tile = super::super::TileScaler::new(w, h);
        let image = player_path(&f, (w, h), &mut alpha, &mut to_tile);
        assert_eq!(
            image.alpha_plane(),
            Some(&[0, 255, 128, 64, 0, 255, 128, 64][..])
        );
        // Luma: 10-bit 64 + 100x -> 8-bit ~ 16 + 25x (swscale dither +-1).
        for (x, &yv) in image.y_plane()[..4].iter().enumerate() {
            let expected = 16 + 25 * i32::try_from(x).unwrap();
            assert!((i32::from(yv) - expected).abs() <= 1, "x={x} y={yv}");
        }
    }

    #[test]
    fn argb_and_rgba_alpha_are_exact_and_colour_matches_the_opaque_conversion() {
        let (w, h) = (4, 2);
        for format in [Pixel::ARGB, Pixel::RGBA, Pixel::BGRA] {
            let mut f = Video::new(format, w, h);
            let stride = f.stride(0);
            let alphas = [0_u8, 85, 170, 255];
            let a_off = if format == Pixel::ARGB { 0 } else { 3 };
            let data = f.data_mut(0);
            for y in 0..h as usize {
                for x in 0..w as usize {
                    let px = &mut data[y * stride + 4 * x..y * stride + 4 * x + 4];
                    px.fill(255); // white colour channels
                    px[a_off] = alphas[x];
                }
            }
            let mut alpha = MediaAlpha::default();
            let mut to_tile = super::super::TileScaler::new(w, h);
            let image = player_path(&f, (w, h), &mut alpha, &mut to_tile);
            assert_eq!(
                image.alpha_plane(),
                Some(&[0, 85, 170, 255, 0, 85, 170, 255][..]),
                "{format:?}"
            );
            // Straight alpha: the colour is white everywhere regardless of A
            // (limited-range white = 235, +-1 for swscale rounding).
            for &yv in image.y_plane() {
                assert!((i32::from(yv) - 235).abs() <= 1, "{format:?} y={yv}");
            }
        }
    }

    #[test]
    fn odd_decoded_size_scales_alpha_with_the_luma_geometry() {
        // An odd 7x5 alpha asset onto an even 14x10 tile: alpha left half 0,
        // right half 255; the scaled alpha keeps that split (Lanczos ringing
        // aside) and matches the luma's scaler geometry.
        let (w, h) = (7, 5);
        let mut f = Video::new(Pixel::RGBA, w, h);
        let stride = f.stride(0);
        let data = f.data_mut(0);
        for y in 0..h as usize {
            for x in 0..w as usize {
                let px = &mut data[y * stride + 4 * x..y * stride + 4 * x + 4];
                let on = x >= 4;
                px.copy_from_slice(&[255, 255, 255, if on { 255 } else { 0 }]);
            }
        }
        let tile = (14, 10);
        let mut alpha = MediaAlpha::default();
        let mut to_tile = super::super::TileScaler::new(tile.0, tile.1);
        let image = player_path(&f, tile, &mut alpha, &mut to_tile);
        let a = image.alpha_plane().unwrap();
        assert_eq!(a.len(), 14 * 10);
        for row in a.chunks_exact(14) {
            assert!(row[..6].iter().all(|&v| v <= 8), "left {row:?}");
            assert!(row[9..].iter().all(|&v| v >= 247), "right {row:?}");
        }
    }

    /// A `tracing` writer capturing formatted events into a shared buffer.
    #[derive(Clone, Default)]
    struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
        type Writer = Self;
        fn make_writer(&'a self) -> Self {
            self.clone()
        }
    }

    fn warnings_from(f: impl FnOnce()) -> String {
        let capture = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        let bytes = capture.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    fn stream(player: bool, vp9: bool, alpha_mode: bool) -> StreamAlpha {
        StreamAlpha {
            player,
            vp9,
            alpha_mode,
        }
    }

    /// PB-5: a VP9-alpha asset without `libvpx-vp9` plays opaque *with a
    /// warning* (ADR-0112 §5); ordinary opaque streams stay silent.
    #[test]
    fn vp9_alpha_without_an_alpha_decoder_warns_and_opaque_streams_do_not() {
        let mut chosen = Some("unset");
        let logs = warnings_from(|| {
            chosen = choose_vp9_alpha("stinger", stream(true, true, true), false);
        });
        assert_eq!(chosen, None);
        assert!(
            logs.contains("WARN") && logs.contains("plays opaque") && logs.contains("stinger"),
            "missing warning: {logs:?}"
        );
        for (player, vp9, alpha) in [
            (true, true, false),
            (true, false, true),
            (false, true, true),
            (true, false, false),
        ] {
            let mut chosen = Some("unset");
            let logs = warnings_from(|| {
                chosen = choose_vp9_alpha("plain", stream(player, vp9, alpha), false);
            });
            assert_eq!(chosen, None);
            assert!(logs.is_empty(), "{player} {vp9} {alpha}: {logs:?}");
        }
        let logs = warnings_from(|| {
            chosen = choose_vp9_alpha("stinger", stream(true, true, true), true);
        });
        assert_eq!(chosen, Some(LIBVPX_VP9));
        assert!(logs.is_empty(), "{logs:?}");
    }

    /// PB-2: an alpha scratch frame whose size cannot be allocated is an
    /// error before any plane is touched (`Video::new` ignored the failed
    /// `av_frame_get_buffer`, so `stride(0)` panicked on a planeless frame).
    #[test]
    fn an_unallocatable_scratch_frame_is_an_error_not_a_panic() {
        let mut slot = None;
        assert!(reuse(&mut slot, u32::MAX, 1).is_err());
        assert!(slot.is_none());
        assert!(reuse(&mut slot, 0, 4).is_err());
        let frame = reuse(&mut slot, 7, 5).unwrap();
        assert_eq!(
            (frame.width(), frame.height(), frame.frame().planes()),
            (7, 5, 1)
        );
    }

    /// R3 round 2 #2: the tile-size scaler output is never an unchecked
    /// `Video::empty()` (no null check on `av_frame_alloc`) handed to
    /// `run_into`: before the first scaled frame no scratch exists, and after
    /// it the scratch is a checked `ScratchFrame` at exactly the tile size.
    #[test]
    fn the_scaled_scratch_is_a_checked_tile_size_frame() {
        let mut alpha = MediaAlpha::default();
        assert!(alpha.scaled.is_none() && alpha.decoded.is_none());
        let mut to_tile = super::super::TileScaler::new(4, 4);
        let image = player_path(&yuva420p(8, 6), (4, 4), &mut alpha, &mut to_tile);
        let scaled = alpha.scaled.as_ref().unwrap();
        assert!(scaled.matches(Pixel::GRAY8, 4, 4));
        assert!(scaled.plane(0).is_ok());
        assert_eq!(image.alpha_plane().unwrap().len(), 16);
    }

    #[test]
    fn steady_state_reuses_the_pool_plane_and_scratch_frames() {
        let frame = yuva420p(8, 6);
        let mut alpha = MediaAlpha::default();
        let mut to_tile = super::super::TileScaler::new(4, 4);
        let first = player_path(&frame, (4, 4), &mut alpha, &mut to_tile);
        let plane = first.alpha_plane().unwrap().as_ptr();
        drop(first);
        let decoded = alpha.decoded.as_ref().unwrap().frame().data(0).as_ptr();
        let scaled = alpha.scaled.as_ref().unwrap().frame().data(0).as_ptr();
        assert_eq!(alpha.pool().retained(), 1);
        for _ in 0..16 {
            let image = player_path(&frame, (4, 4), &mut alpha, &mut to_tile);
            assert_eq!(image.alpha_plane().unwrap().as_ptr(), plane);
            assert_eq!(
                alpha.decoded.as_ref().unwrap().frame().data(0).as_ptr(),
                decoded
            );
            assert_eq!(
                alpha.scaled.as_ref().unwrap().frame().data(0).as_ptr(),
                scaled
            );
            drop(image);
            assert_eq!(alpha.pool().retained(), 1);
        }
    }
}

/// End-to-end: a tiny alpha clip generated at test time with the `ffmpeg` CLI
/// (as `media_player_loop_tests` does) is decoded through the real media
/// player executor (`ingest_loop` → `open_and_stream` → `stream_player`) and
/// the published tile's alpha plane is checked at a few pixels. A codec the
/// local `ffmpeg` cannot encode prints `SKIP` and returns.
#[cfg(test)]
mod player_tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]

    use std::path::Path;
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use multiview_compositor::pipeline::Nv12Image;
    use multiview_core::time::{MediaTime, Rational};
    use multiview_framestore::{NoSignalPolicy, TileStore, TileThresholds};

    use crate::player::{EofPolicy, PlayerHandle, PlayoutGeometry, TransportMailbox};

    const W: u32 = 32;
    const H: u32 = 16;
    const FRAMES: u32 = 5;
    /// Four 8-px vertical alpha bands; checked at each band centre.
    const BANDS: [u8; 4] = [0, 85, 170, 255];

    fn encoder_available(name: &str) -> bool {
        Command::new("ffmpeg")
            .args(["-hide_banner", "-encoders"])
            .output()
            .is_ok_and(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .split_whitespace()
                    .any(|w| w == name)
            })
    }

    /// Encode a `W x H` white clip with the band alpha, or `false` (SKIP).
    fn generate(path: &Path, codec: &[&str], alpha: bool) -> bool {
        let source = if alpha {
            format!(
                "color=c=white:s={W}x{H}:r=25:d={},format=rgba,\
                 geq=r=255:g=255:b=255:a='if(lt(X,8),0,if(lt(X,16),85,if(lt(X,24),170,255)))'",
                f64::from(FRAMES) / 25.0
            )
        } else {
            format!("testsrc=s={W}x{H}:r=25:d={}", f64::from(FRAMES) / 25.0)
        };
        let status = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
            ])
            .arg(source)
            .args(codec)
            .arg(path)
            .status();
        status.is_ok_and(|s| s.success()) && path.exists()
    }

    /// Play `clip` through the real player executor and return the first
    /// published tile.
    fn play(clip: &Path) -> Arc<Nv12Image> {
        let cadence = Rational::new(25, 1);
        let geometry =
            PlayoutGeometry::new(0, u64::from(FRAMES), 0, u64::from(FRAMES), cadence).unwrap();
        let handle = PlayerHandle::new(
            "alpha-player".to_owned(),
            geometry,
            EofPolicy::HoldLastFrame,
            false,
            Arc::new(TransportMailbox::new()),
        );
        let store = Arc::new(TileStore::new(
            "alpha-player",
            TileThresholds::default(),
            NoSignalPolicy::HoldForever,
        ));
        let plan = super::super::IngestPlan {
            source_events: None,
            id: "alpha-player".to_owned(),
            location: super::super::SourceLocation::Path(clip.to_path_buf()),
            http_headers: super::super::HttpHeaders::default(),
            player: Some(handle),
            tap: None,
            tile_w: W,
            tile_h: H,
            display: None,
            store: Arc::clone(&store),
            output_origin: crate::timing_status::anchor_slot(),
            live_anchor: Arc::new(multiview_framestore::SwapCell::empty()),
            lifecycle: 0,
            live: false,
            #[cfg(feature = "overlay")]
            incontainer_sub: None,
            #[cfg(feature = "overlay")]
            embedded_cc: None,
            canvas_color: super::super::CanvasColor::default(),
            cadence,
            decode_placement: super::super::DecodePlacement::Default,
            #[cfg(feature = "webrtc-native")]
            webrtc_registry: None,
            #[cfg(feature = "webrtc-native")]
            webrtc_audio_store: None,
            #[cfg(feature = "ndi")]
            ndi_accept_license: false,
            #[cfg(feature = "ndi")]
            ndi_acceptance: multiview_input::ndi::license::LicenseAcceptance {
                accepted_by: String::new(),
                accepted_at: String::new(),
            },
            #[cfg(feature = "youtube")]
            youtube_url_slot: None,
            #[cfg(feature = "youtube")]
            youtube_yt_dlp_path: None,
        };
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let player = std::thread::Builder::new()
            .name("alpha-player-test".to_owned())
            .spawn(move || super::super::ingest_loop(&plan, &stop_thread))
            .unwrap();
        let far = MediaTime::from_nanos(600_000_000_000);
        let deadline = Instant::now() + Duration::from_secs(10);
        let frame = loop {
            if let Some(frame) = store.read_at(far).frame() {
                break Arc::clone(frame);
            }
            assert!(Instant::now() < deadline, "the player published no frame");
            std::thread::sleep(Duration::from_millis(5));
        };
        stop.store(true, Ordering::Release);
        player.join().unwrap();
        frame
    }

    fn assert_bands(image: &Nv12Image, tolerance: u8, what: &str) {
        let alpha = image
            .alpha_plane()
            .unwrap_or_else(|| panic!("{what}: an alpha asset must publish an alpha plane"));
        assert_eq!(alpha.len(), usize::try_from(W * H).unwrap(), "{what}");
        let w = usize::try_from(W).unwrap();
        for row in [1_usize, 8, 14] {
            for (band, &expected) in BANDS.iter().enumerate() {
                let x = band * 8 + 4;
                let got = alpha[row * w + x];
                assert!(
                    got.abs_diff(expected) <= tolerance,
                    "{what}: alpha at ({x},{row}) = {got}, expected {expected} +-{tolerance}"
                );
            }
        }
        // Straight alpha: colour is white even where alpha is 0.
        let y = image.sample(4, 8).unwrap().0;
        assert!(
            y >= 230,
            "{what}: straight-alpha fill colour must stay white, Y={y}"
        );
    }

    fn run_case(file: &str, encoder: &str, codec: &[&str], tolerance: u8) {
        if !encoder_available(encoder) {
            println!("SKIP: ffmpeg encoder `{encoder}` not available for {file}");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join(file);
        if !generate(&clip, codec, true) {
            println!("SKIP: ffmpeg could not encode {file} with `{encoder}`");
            return;
        }
        assert_bands(&play(&clip), tolerance, file);
    }

    #[test]
    fn qtrle_argb_asset_publishes_an_exact_alpha_plane() {
        run_case(
            "alpha_qtrle.mov",
            "qtrle",
            &["-c:v", "qtrle", "-pix_fmt", "argb"],
            0,
        );
    }

    #[test]
    fn prores_4444_asset_publishes_its_alpha_plane() {
        run_case(
            "alpha_prores4444.mov",
            "prores_ks",
            &[
                "-c:v",
                "prores_ks",
                "-profile:v",
                "4444",
                "-pix_fmt",
                "yuva444p10le",
            ],
            1,
        );
    }

    #[test]
    fn png_in_mov_rgba_asset_publishes_an_exact_alpha_plane() {
        run_case(
            "alpha_png.mov",
            "png",
            &["-c:v", "png", "-pix_fmt", "rgba"],
            0,
        );
    }

    #[test]
    fn vp9_alpha_webm_asset_publishes_its_alpha_plane() {
        // VP9 alpha rides a lossy side stream; libvpx-vp9 must be linked.
        if ffmpeg_next::decoder::find_by_name("libvpx-vp9").is_none() {
            println!("SKIP: libav has no libvpx-vp9 decoder (VP9 alpha needs it)");
            return;
        }
        run_case(
            "alpha_vp9.webm",
            "libvpx-vp9",
            &[
                "-c:v",
                "libvpx-vp9",
                "-pix_fmt",
                "yuva420p",
                "-lossless",
                "1",
            ],
            2,
        );
    }

    #[test]
    fn opaque_asset_publishes_no_alpha_plane() {
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("opaque.ts");
        assert!(
            generate(
                &clip,
                &["-pix_fmt", "yuv420p", "-c:v", "mpeg2video", "-f", "mpegts"],
                false
            ),
            "ffmpeg failed to generate the opaque clip"
        );
        let image = play(&clip);
        assert!(
            image.alpha_plane().is_none(),
            "an opaque asset must stay opaque"
        );
        assert_eq!((image.width(), image.height()), (W, H));
    }
}
