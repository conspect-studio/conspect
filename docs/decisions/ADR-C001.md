# ADR-C001: Canvas working & output color space defaults to SDR BT.709 limited, with opt-in HDR canvas

- **Status:** Proposed
- **Area:** Color
- **Date:** 2026-06-02
- **Source brief:** [color-management.md](../research/color-management.md)

## Decision

Fix the compositor canvas to ONE configurable color space. Default: BT.709 primaries (CICP 1) + BT.709/BT.1886 transfer (CICP 1 / display EOTF 2.4) + BT.709-ncl matrix (CICP 1) + LIMITED range (full_range_flag=0), composited internally in a BT.709-primaries LINEAR Rgba16Float buffer. Provide opt-in 'hdr-pq-bt2020' (BT.2020/PQ) and 'hdr-hlg-bt2020' (BT.2020/HLG) canvas modes.

## Rationale

BT.709 limited is the universally and correctly-rendered SDR lingua franca across every output protocol (RTSP/HLS/RTMP/SRT/NDI HD) and every HW encoder's no-fuss path (NVENC writes full_range_flag=0 cleanly for NV12 limited; VideoToolbox defaults to limited; TS/HLS expect it). Downstream HDR rendering is fragile (tags dropped, players misrender), so HDR is opt-in. A single fixed canvas makes the GPU kernel deterministic and the output unambiguously taggable.

## Alternatives considered

Full-range output (1 extra bit) — rejected for interop: NVENC needs JPEG/YUVJ, VT HW-decode squeezes it, NV12 assumed limited downstream. Inheriting a single input's tags — rejected: breaks on heterogeneous inputs. Always BT.2020/PQ — rejected: forces tone-mapping for the common all-SDR case and risks broken downstream rendering.

## Consequences

Requires per-tile HDR->SDR tone-mapping when HDR sources hit the default canvas (see ADR-C005). All SDR inputs of differing primaries converge to BT.709 (identity if already 709). Drives the output-tag policy (ADR-C006) and the encode-side OETF/matrix/range compression.
