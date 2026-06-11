# ADR-C002: Untagged-input default policy: resolution heuristic for matrix/primaries, format class for range, never auto-HDR

- **Status:** Proposed
- **Area:** Color
- **Date:** 2026-06-02
- **Source brief:** [color-management.md](../research/color-management.md)

## Decision

Resolve each of the four axes per tile via precedence frame > AVCodecContext > container colr > POLICY. POLICY for genuinely unspecified axes: RANGE = limited for YUV / full for RGB; MATRIX+PRIMARIES = (width>=1280 OR height>576 => BT.709; height==576 => BT.601-625/BT.470BG; height==480 or 486 => BT.601-525/SMPTE-170M; else BT.709); TRC = BT.709/BT.1886 SDR default (sRGB for graphics/RGB sources). NEVER auto-promote to BT.2020/PQ/HLG from resolution. Always trust an explicitly tagged axis over the heuristic. Log every policy fallback with tile id + resolved tuple. Provide per-source overrides.

## Rationale

This reproduces libplacebo/mpv/player behavior, so the multiview renders untagged content the same way a correct player would, and it avoids swscale's flat BT.601-for-everything mistake. Refusing to auto-guess wide-gamut/HDR prevents catastrophic SDR->HDR mis-promotion. Per-axis independence (each can be unspecified separately) means each must be resolved separately. Storing the resolved never-UNSPECIFIED tuple keeps the kernel deterministic.

## Alternatives considered

swscale's flat BT.601 default — rejected (wrong for HD, disagrees with players). Hard-code BT.709 for everything — rejected (shifts skin tones on genuine SD/NTSC/PAL). Refuse to guess and require explicit tags — rejected (too brittle for live RTSP/TS/SRT ingest).

## Consequences

Heuristic will sometimes be wrong (SD authored as 709, full-range YUV mislabeled) — mitigated by mandatory per-source override config. Requires a detection module producing the 4-tuple + chroma siting + bit depth per frame. Precedence (frame>ctx>container>policy) must be locked and tested with fixtures where container colr disagrees with VUI.
