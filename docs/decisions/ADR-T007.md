# ADR-T007: Codec edge-case & decode/encode policy: one bad input never stalls the multiview

- **Status:** Proposed
- **Area:** Streaming/Timing
- **Date:** 2026-06-02
- **Source brief:** [streaming-gotchas.md](../research/streaming-gotchas.md)

## Decision

Per-input isolated DecoderWorker: err_recognition WITHOUT AV_EF_EXPLODE (+ optional AV_EF_IGNORE_ERR), error_concealment on, do NOT set AV_CODEC_FLAG_OUTPUT_CORRUPT or AV_CODEC_FLAG2_SHOW_ALL. Gate compositing on AV_FRAME_FLAG_CORRUPT==0 AND decode_error_flags==0 AND format sanity. Keep a per-input error counter + no-output watchdog; on threshold/reference-loss: drain(NULL)->avcodec_flush_buffers->request IDR/reconnect->hold last-good. Normalize every frame to one canonical fmt/size; reconfigure swscale/zscale (never the encoder) on input changes; handle yuvj* as full-range, HDR->SDR via zscale+tonemap (swscale cannot do gamut), deinterlace via bwdif=send_frame. HW decode with per-input software fallback via get_format. Encode: fixed canvas + closed fixed GOP + -fps_mode cfr + forced keyframes.

## Rationale

No-EXPLODE relaxes only MINOR errors; ENOMEM/EINVAL/AVERROR_EXTERNAL/HW faults still hard-fail or return no frames (perpetual EAGAIN), so the watchdog+flush+reconnect+hold path is mandatory. Default wait-for-IDR is a heuristic not a guarantee (CHUNKS/raw/recovery-point SEI emit early/partial frames), so gate on corrupt+error flags. Reconfiguring the OUTPUT encoder for an input change is forbidden (NVENC can't reconfig GOP).

## Alternatives considered

Letting the decoder explode + restart the process (full output stall); HW-only decode (brittle on unsupported profiles/driver hiccups); resizing the output canvas to match an input (costly encoder reconfig / discontinuity).

## Consequences

Per-tile freeze duration (last-good hold) before blanking/placeholder is a configurable policy. Some HW decoders need explicit close/reopen on mid-stream resolution change (validate per VAAPI/QSV/VideoToolbox). GDR/intra-refresh streams have no IDR - gate on CORRUPT clearing, and still force real IDRs on OUTPUT for segmentation.
