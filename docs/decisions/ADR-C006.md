# ADR-C006: Always explicitly tag output across encoder + container/protocol, then verify with ffprobe

- **Status:** Proposed
- **Area:** Color
- **Date:** 2026-06-02
- **Source brief:** [color-management.md](../research/color-management.md)

## Decision

On every encode, SET all four color fields explicitly to the canvas values (SDR default: bt709/bt709/bt709 + limited) on the encoder, and ensure pixels match the declared tags. Per encoder: NVENC SDK path sets videoSignalTypePresentFlag=1, colourDescriptionPresentFlag=1, the three CICP fields, and videoFullRangeFlag (and AVCOL_RANGE_JPEG for full-range NV12 via libav); VideoToolbox set color_range explicitly (defaults to MPEG/limited); software/x264/x265 set colorprim/transfer/colormatrix + range; VAAPI verify per driver. Per container: MP4/MOV rely on default nclx colr (prefer nclx over nclc), force +write_colr if needed; HLS fMP4 write colr in init segment AND set playlist VIDEO-RANGE (SDR/HLG/PQ per TC); MPEG-TS/RTMP force a complete in-band VUI (no container box); NDI apply the convention by hand (YUV=limited by-resolution matrix, RGBA=full). For HDR output, author canonical canvas ST 2086 + MaxCLL/MaxFALL as IN-BAND SEI. Run an automated ffprobe assertion gate after every encode AND remux that fails on 'unknown' or policy mismatch.

## Rationale

Encoders write nothing by default (CICP 2 unspecified) and tagging only labels, never converts; untagged or mismatched output re-triggers the player-guessing trap (washed-out/hue-shifted) silently. Each container signals differently (MP4 colr / HLS colr+VIDEO-RANGE / TS VUI-only / NDI convention), and one layer can succeed while another drops, so all must be set and verified. HDR is triggered by the VUI color tags (PQ/HLG transfer + BT.2020 primaries + 10-bit), NOT by static metadata; static metadata is recommended for tone-mapping and must be in-band SEI to survive RTSP/SRT/TS.

## Alternatives considered

Relying on encoder/muxer defaults — rejected (untagged output, guaranteed downstream bug). Setting AVCodecContext color and assuming it lands — rejected (NVENC range/colour-description gating, VT silent limited default, container drops). Tagging without verifying — rejected (request != bitstream; needed for bulletproof 24/7 output). Treating ST 2086/MaxCLL as what makes a stream HDR — rejected per verification (it does not; the VUI tags do).

## Consequences

Adds a mandatory post-encode/remux ffprobe verification gate to the output SLA. NDI ingest/output need manual range+matrix math. HDR output requires in-band SEI injection (x265-style) and canonical canvas metadata authoring. Bitstream-filter relabel (h264/hevc_metadata -c copy) is reserved for relabel-only fixups where pixels are already correct.
