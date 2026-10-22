//! The REAL pure-Rust NV12 -> JPEG encoder: known synthetic patterns must encode
//! to a genuine, decodable JPEG of the right geometry and approximately the right
//! colours (lossy DCT => a tolerance, never bit-exact).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_preview::{JpegEncoder, JpegError, Nv12JpegEncoder};
use zune_jpeg::zune_core::bytestream::ZCursor;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::JpegDecoder;

/// Build a flat NV12 plane of a single (Y, Cb, Cr) triple at `width`x`height`.
fn solid_nv12(width: u32, height: u32, y: u8, cb: u8, cr: u8) -> Vec<u8> {
    let w = usize::try_from(width).unwrap();
    let h = usize::try_from(height).unwrap();
    let mut plane = vec![y; w * h];
    // Interleaved CbCr plane: (w/2 * h/2) chroma samples, 2 bytes each => w*h/2.
    let mut chroma = Vec::with_capacity(w * h / 2);
    for _ in 0..(w * h / 4) {
        chroma.push(cb);
        chroma.push(cr);
    }
    plane.extend_from_slice(&chroma);
    plane
}

/// Decode a JPEG to packed RGB plus its decoded dimensions.
fn decode_rgb(jpeg: &[u8]) -> (Vec<u8>, usize, usize) {
    let mut dec = JpegDecoder::new(ZCursor::new(jpeg));
    let pixels = dec
        .decode()
        .expect("emitted bytes must be a decodable JPEG");
    let info = dec.info().expect("decoded JPEG must expose its dimensions");
    assert_eq!(
        dec.output_colorspace(),
        Some(ColorSpace::RGB),
        "default zune output is packed RGB"
    );
    (pixels, usize::from(info.width), usize::from(info.height))
}

/// Average decoded RGB over the whole image (the encoder is lossy; we compare
/// against a tolerance on the mean, never bit-exact).
fn mean_rgb(rgb: &[u8]) -> (f64, f64, f64) {
    assert_eq!(rgb.len() % 3, 0);
    let n = f64::from(u32::try_from(rgb.len() / 3).unwrap());
    let mut r = 0.0;
    let mut g = 0.0;
    let mut b = 0.0;
    for px in rgb.chunks_exact(3) {
        r += f64::from(px[0]);
        g += f64::from(px[1]);
        b += f64::from(px[2]);
    }
    (r / n, g / n, b / n)
}

#[test]
fn encodes_a_real_decodable_jpeg_of_the_right_size() {
    let enc = Nv12JpegEncoder::default();
    // Mid-grey: Y=128, Cb=Cr=128 (neutral chroma) => grey RGB ~128.
    let plane = solid_nv12(64, 32, 128, 128, 128);
    let jpeg = enc.encode_nv12(&plane, 64, 32, 80).expect("encodes");

    // Real JPEG framing markers.
    assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "SOI");
    assert_eq!(&jpeg[jpeg.len() - 2..], &[0xFF, 0xD9], "EOI");

    let (rgb, w, h) = decode_rgb(&jpeg);
    assert_eq!((w, h), (64, 32), "decoded geometry matches the request");
    assert_eq!(rgb.len(), 64 * 32 * 3);
}

#[test]
fn neutral_grey_decodes_to_grey() {
    let enc = Nv12JpegEncoder::default();
    let plane = solid_nv12(32, 32, 128, 128, 128);
    let jpeg = enc.encode_nv12(&plane, 32, 32, 90).expect("encodes");
    let (rgb, _, _) = decode_rgb(&jpeg);
    let (r, g, b) = mean_rgb(&rgb);
    // BT.601 limited-range Y=128 maps to ~mid grey; channels near-equal.
    for c in [r, g, b] {
        assert!((100.0..=160.0).contains(&c), "grey channel {c} off range");
    }
    assert!((r - g).abs() < 12.0 && (g - b).abs() < 12.0, "near-neutral");
}

#[test]
fn red_chroma_decodes_reddish() {
    // BT.601 limited-range pure-ish red: high Cr, low Cb, mid luma.
    // Y~82, Cb~90, Cr~240 is a saturated red in limited-range YCbCr.
    let enc = Nv12JpegEncoder::default();
    let plane = solid_nv12(32, 32, 82, 90, 240);
    let jpeg = enc.encode_nv12(&plane, 32, 32, 90).expect("encodes");
    let (rgb, _, _) = decode_rgb(&jpeg);
    let (r, g, b) = mean_rgb(&rgb);
    assert!(r > g + 40.0, "red dominates green: r={r} g={g}");
    assert!(r > b + 40.0, "red dominates blue: r={r} b={b}");
}

#[test]
fn blue_chroma_decodes_blueish() {
    // BT.601 limited-range saturated blue: high Cb, low Cr.
    let enc = Nv12JpegEncoder::default();
    let plane = solid_nv12(32, 32, 41, 240, 110);
    let jpeg = enc.encode_nv12(&plane, 32, 32, 90).expect("encodes");
    let (rgb, _, _) = decode_rgb(&jpeg);
    let (r, g, b) = mean_rgb(&rgb);
    assert!(b > r + 40.0, "blue dominates red: b={b} r={r}");
    assert!(b > g + 20.0, "blue dominates green: b={b} g={g}");
}

#[test]
fn rejects_malformed_geometry_like_the_stub() {
    let enc = Nv12JpegEncoder::default();
    // Buffer too small.
    assert!(matches!(
        enc.encode_nv12(&[0u8; 3], 4, 2, 80),
        Err(JpegError::BufferTooSmall { .. })
    ));
    // Odd dimensions.
    assert!(matches!(
        enc.encode_nv12(&[0u8; 100], 3, 2, 80),
        Err(JpegError::OddDimensions { .. })
    ));
    // Quality out of range.
    assert!(matches!(
        enc.encode_nv12(&solid_nv12(4, 2, 16, 128, 128), 4, 2, 0),
        Err(JpegError::InvalidQuality(0))
    ));
    assert!(matches!(
        enc.encode_nv12(&solid_nv12(4, 2, 16, 128, 128), 4, 2, 101),
        Err(JpegError::InvalidQuality(101))
    ));
}

#[test]
fn quality_changes_output_size() {
    let enc = Nv12JpegEncoder::default();
    // A patterned plane so quality actually matters (a flat plane compresses to
    // near-nothing at any quality). Gradient luma.
    let dim: u32 = 64;
    let w = usize::try_from(dim).unwrap();
    let h = w;
    let mut plane = vec![0u8; w * h + w * h / 2];
    for (i, p) in plane[..w * h].iter_mut().enumerate() {
        let x = i % w;
        let y = i / w;
        *p = u8::try_from((x * 4 + y * 4) % 256).unwrap();
    }
    for c in plane[w * h..].chunks_exact_mut(2) {
        c[0] = 100;
        c[1] = 150;
    }
    let low = enc.encode_nv12(&plane, dim, dim, 20).expect("low q");
    let high = enc.encode_nv12(&plane, dim, dim, 95).expect("high q");
    assert!(
        high.len() > low.len(),
        "higher quality => more bytes: high={} low={}",
        high.len(),
        low.len()
    );
    // Both remain decodable at the right size.
    let (_, lw, lh) = decode_rgb(&low);
    assert_eq!((lw, lh), (w, h));
}
