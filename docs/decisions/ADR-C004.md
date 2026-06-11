# ADR-C004: Range handled explicitly in-shader exactly once; expand on input, compress on output

- **Status:** Proposed
- **Area:** Color
- **Date:** 2026-06-02
- **Source brief:** [color-management.md](../research/color-management.md)

## Decision

Treat range as a first-class independent axis. Apply range expansion (limited->normalized or full->normalized) in code-value space BEFORE the YUV->RGB matrix, exactly once per tile on input; apply range compression exactly once on output to the canvas range (limited for the default SDR canvas). Use exact ITU numerics (8-bit limited Y/(219), C/(224); full Y/255, C/255; bit-depth-tied chroma center 128/512/32768; P010 high-bit descale). Source range from AVColorRange (with policy fallback) and, on macOS, from the bitstream/container VUI rather than the VideoToolbox decoded surface. Never rely on legacy yuvj* pixfmt names as the source of truth.

## Rationale

Range mismatch is one of the highest-impact and most common color bugs (elevated/grey or crushed/clipped levels). Because we bypass swscale we lose its implicit range handling and MUST do it ourselves. Doing it exactly once avoids the double-conversion bug (HW decoder or auto-inserted swscale scale converting before our code runs). The explicit AVColorRange field is authoritative; yuvj* is a deprecated full-range hint only.

## Alternatives considered

Inferring range from pixfmt name (yuvj*) — rejected (deprecated, conflicts with explicit field, flips full/limited). Trusting the HW-decoded surface range on macOS — rejected (VT HW-decode range bug #6546). Letting an implicit filter convert — rejected (we own the composite; double conversion).

## Consequences

Detection layer must surface range per tile with policy fallback (YUV=limited, RGB=full). Output encode must compress to canvas range and the encoder/container must be tagged to match (ADR-C006). NV12 full-range output is explicitly avoided by the default limited canvas, sidestepping the NVENC YUVJ trap.
