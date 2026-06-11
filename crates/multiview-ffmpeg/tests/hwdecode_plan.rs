//! Host-side hardware-decode planning tests (GPU-free).
//!
//! GPU-6 turns the hardware backends from detection-only into real decode
//! paths. The *real* device-resident decode/composite/encode runs only on the
//! GPU-tagged self-hosted runners (no GPU/SDK here), so these tests pin the
//! **host-side** seam that the real path consumes and that IS verifiable on a
//! GPU-free box:
//!
//! * the per-backend decode-resize strategy (NVIDIA fused `cuvid -resize` to the
//!   largest consuming tile vs. Intel/AMD/software full-res — efficiency §2.1),
//! * the NVDEC decode-surface pool sizing discipline that keeps a 1080p decoder
//!   at tens of MB rather than ballooning to ~542 MB (efficiency §1 footgun),
//! * the hardware software-format choice (NV12 8-bit / P010 10-bit — inv #5).
//!
//! None of this opens a device; it is pure arithmetic over the plan inputs, so
//! it runs on shared CI. The libav-backed `*_cuvid` decoder-name resolution is a
//! separate, `ffmpeg`-feature-gated test below (it only inspects the linked
//! registry — still no GPU needed to open).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp
)]

use multiview_ffmpeg::hwdecode::{
    decode_surface_pool, plan_decode_resize, DecodeResizeStrategy, HwBitDepth, HwDecodePlan,
    PoolInputs, ResizeInputs, TileSize,
};
use multiview_ffmpeg::HwDeviceKind;

#[test]
fn nvidia_fuses_decode_resize_to_the_largest_consuming_tile() {
    // One deduplicated 4K source feeding three tile sizes. On NVIDIA the NVDEC
    // ASIC resizes for free at decode time, so the decoder is driven to the
    // LARGEST consuming tile (then scale_cuda fans the smaller tiles on-die) —
    // decode-at-display-resolution (inv #6) without a host round-trip.
    let plan = plan_decode_resize(ResizeInputs {
        kind: HwDeviceKind::Cuda,
        source: TileSize::new(3840, 2160),
        consuming_tiles: vec![
            TileSize::new(640, 360),
            TileSize::new(1280, 720),
            TileSize::new(960, 540),
        ],
    });

    assert_eq!(plan.strategy, DecodeResizeStrategy::Fused);
    // Resize target is the largest consuming tile (1280x720), NOT the 4K source
    // and NOT the smallest tile.
    assert_eq!(plan.decode_target, TileSize::new(1280, 720));
    // Even fused, the bitstream is entropy-decoded at SOURCE resolution, so the
    // decode-engine MP/s budget is charged at the source size (efficiency §2.1).
    assert_eq!(plan.bitstream_resolution, TileSize::new(3840, 2160));
    // A fused NVIDIA decode is a whole on-device island — no host copy.
    assert!(plan.device_resident);
}

#[test]
fn nvidia_with_a_single_consuming_tile_resizes_straight_to_it() {
    // The common case: one source -> one tile. The fused resize targets exactly
    // that tile and no extra scale pass is implied.
    let plan = plan_decode_resize(ResizeInputs {
        kind: HwDeviceKind::Cuda,
        source: TileSize::new(1920, 1080),
        consuming_tiles: vec![TileSize::new(640, 360)],
    });
    assert_eq!(plan.strategy, DecodeResizeStrategy::Fused);
    assert_eq!(plan.decode_target, TileSize::new(640, 360));
}

#[test]
fn intel_and_amd_decode_full_res_then_scale_separately() {
    // VAAPI/QSV do NOT fuse decode-time scaling: the decoder reconstructs
    // full-resolution reference frames and a separate VPP/SFC pass scales after
    // (efficiency §2.1). So the decode target is the SOURCE size, strategy is a
    // separate on-die scale, and the engine must budget a full-res surface set.
    for kind in [HwDeviceKind::Vaapi, HwDeviceKind::Qsv] {
        let plan = plan_decode_resize(ResizeInputs {
            kind,
            source: TileSize::new(1920, 1080),
            consuming_tiles: vec![TileSize::new(640, 360)],
        });
        assert_eq!(
            plan.strategy,
            DecodeResizeStrategy::SeparateOnDie,
            "{kind:?} has no fused decode resize"
        );
        assert_eq!(
            plan.decode_target,
            TileSize::new(1920, 1080),
            "{kind:?} decodes at full source resolution"
        );
        assert_eq!(plan.bitstream_resolution, TileSize::new(1920, 1080));
        assert!(
            plan.device_resident,
            "{kind:?} keeps the scale pass on the media block"
        );
    }
}

#[test]
fn videotoolbox_scales_on_die_after_decode() {
    // Apple decodes full-res into an IOSurface then scales with scale_vt — a
    // separate on-die pass, still device-resident (unified memory).
    let plan = plan_decode_resize(ResizeInputs {
        kind: HwDeviceKind::VideoToolbox,
        source: TileSize::new(1920, 1080),
        consuming_tiles: vec![TileSize::new(960, 540)],
    });
    assert_eq!(plan.strategy, DecodeResizeStrategy::SeparateOnDie);
    assert_eq!(plan.decode_target, TileSize::new(1920, 1080));
    assert!(plan.device_resident);
}

#[test]
fn an_empty_consuming_set_decodes_at_source_resolution() {
    // No tile consumes this source yet (e.g. a just-added input). The plan must
    // not panic or pick a zero target; it decodes at source until a tile binds.
    let plan = plan_decode_resize(ResizeInputs {
        kind: HwDeviceKind::Cuda,
        source: TileSize::new(1280, 720),
        consuming_tiles: Vec::new(),
    });
    assert_eq!(plan.decode_target, TileSize::new(1280, 720));
    assert_eq!(plan.strategy, DecodeResizeStrategy::Fused);
}

#[test]
fn fused_resize_never_upscales_past_the_source() {
    // A consuming tile larger than the source must clamp the decode target to
    // the source: NVDEC resize only downscales meaningfully, and decoding above
    // source res wastes the ASIC. The largest *clamped* tile wins.
    let plan = plan_decode_resize(ResizeInputs {
        kind: HwDeviceKind::Cuda,
        source: TileSize::new(1280, 720),
        consuming_tiles: vec![TileSize::new(1920, 1080), TileSize::new(640, 360)],
    });
    assert_eq!(plan.decode_target, TileSize::new(1280, 720));
}

#[test]
fn nvdec_pool_is_minimal_and_sized_to_real_content_not_a_balloon() {
    // NVDEC: ulNumDecodeSurfaces = dpb_min + 3..4, ulNumOutputSurfaces = 1..2,
    // ulMaxWidth/Height = REAL content size. The footgun is leaving the max
    // inflated, which balloons a 1080p decoder from ~tens of MB to ~542 MB
    // (efficiency §1). The pool must therefore size to the source, not a
    // hard-coded 8K ceiling.
    let dpb = 4; // H.264 high-profile DPB depth, say.
    let pool = decode_surface_pool(PoolInputs {
        kind: HwDeviceKind::Cuda,
        max_resolution: TileSize::new(1920, 1080),
        dpb_frames: dpb,
    });
    // Decode surfaces are the small DPB + a fixed slack of 3..4 (we use 4),
    // never an open-ended count.
    assert_eq!(pool.decode_surfaces, dpb + 4);
    // Output surfaces are 1..2 — we hold the minimum that keeps the pipe fed.
    assert_eq!(pool.output_surfaces, 2);
    // The frames context is sized to the SOURCE, so VRAM scales with content.
    assert_eq!(pool.max_resolution, TileSize::new(1920, 1080));
    // A 1080p NV12 surface is 1920*1080*3/2 bytes; the whole pool must stay in
    // the tens-of-MB band, NOT the half-gigabyte balloon.
    let bytes = pool.estimated_vram_bytes(HwBitDepth::Eight);
    assert!(
        bytes < 80 * 1024 * 1024,
        "1080p NV12 pool must stay well under 80 MiB, got {bytes} bytes"
    );
    assert!(bytes > 0);
}

#[test]
fn vaapi_pool_reserves_codec_reference_frames_plus_work_and_output() {
    // VAAPI/QSV init_pool_size must cover codec reference frames + work + output;
    // some drivers cannot resize the pool, so undersizing stalls. For H.264 the
    // reference reserve is +16 (efficiency §1). The pool must be at least that
    // big plus the decode-in-flight + output slack.
    let pool = decode_surface_pool(PoolInputs {
        kind: HwDeviceKind::Vaapi,
        max_resolution: TileSize::new(1920, 1080),
        dpb_frames: 16,
    });
    // VAAPI cannot fuse-resize, so it reserves a full-res surface set: the DPB
    // (16) plus the same decode+output slack.
    assert!(
        pool.decode_surfaces >= 16 + 2,
        "VAAPI pool must cover the +16 reference reserve plus slack, got {}",
        pool.decode_surfaces
    );
    assert_eq!(pool.max_resolution, TileSize::new(1920, 1080));
}

#[test]
fn p010_pool_costs_twice_the_per_pixel_bytes_of_nv12() {
    // Inv #5: frames stay NV12 (1.5 B/px) or P010 (3 B/px for 10-bit) — never
    // RGBA (4 B/px). The 10-bit pool must cost exactly 2x the 8-bit pool for the
    // same geometry (P010 is 16-bit-per-sample packing of the same 1.5 sample/px).
    let pool = decode_surface_pool(PoolInputs {
        kind: HwDeviceKind::Cuda,
        max_resolution: TileSize::new(1920, 1080),
        dpb_frames: 4,
    });
    let eight = pool.estimated_vram_bytes(HwBitDepth::Eight);
    let ten = pool.estimated_vram_bytes(HwBitDepth::Ten);
    assert_eq!(ten, eight * 2, "P010 surfaces are 2x NV12 bytes");
}

#[test]
fn the_plan_round_trips_its_inputs_for_telemetry() {
    // HwDecodePlan is the telemetry record the engine logs to prove inv #6
    // (decode-at-display-resolution) held: it must carry back the kind it was
    // planned for so a format-trace can assert no host round-trip was inserted.
    let plan = HwDecodePlan {
        strategy: DecodeResizeStrategy::Fused,
        decode_target: TileSize::new(1280, 720),
        bitstream_resolution: TileSize::new(3840, 2160),
        device_resident: true,
        kind: HwDeviceKind::Cuda,
    };
    assert_eq!(plan.kind, HwDeviceKind::Cuda);
    assert!(plan.is_decode_at_display_resolution());
    // A separate full-res-intermediate strategy would NOT be
    // decode-at-display-resolution (it decodes big then scales).
    let full = HwDecodePlan {
        strategy: DecodeResizeStrategy::SeparateFullResIntermediate,
        decode_target: TileSize::new(3840, 2160),
        bitstream_resolution: TileSize::new(3840, 2160),
        device_resident: false,
        kind: HwDeviceKind::Software,
    };
    assert!(!full.is_decode_at_display_resolution());
    assert!(!full.device_resident);
}
