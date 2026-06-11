// useSystemMetrics: the system-topic binding. The hook subscribes to the
// `system` realtime topic and folds each `system.metrics` sample into a BOUNDED
// client-side ring buffer (last ~120 samples), exposing the latest value plus
// per-metric series for the footer sparklines.
//
// These tests cover the pure, deterministic core of that behaviour:
//   - parseSystemMetrics: defensive narrowing of an unknown `data` body.
//   - pushSample / SystemMetricsRing: the bounded ring buffer that conflates to
//     the latest value and never grows past its capacity.
import { describe, expect, it } from 'vitest';

import {
  METRICS_RING_CAPACITY,
  SystemMetricsRing,
  parseSystemMetrics,
} from './useSystemMetrics';
import type { SystemMetrics } from './useSystemMetrics';

function sample(over: Partial<SystemMetrics> = {}): SystemMetrics {
  return {
    cpu_util: 0.25,
    gpus: [],
    sampled_hz: 2,
    ...over,
  };
}

describe('parseSystemMetrics', () => {
  it('returns undefined for non-object data', () => {
    expect(parseSystemMetrics(null)).toBeUndefined();
    expect(parseSystemMetrics('nope')).toBeUndefined();
    expect(parseSystemMetrics(42)).toBeUndefined();
  });

  it('returns undefined when required fields are missing or mistyped', () => {
    expect(parseSystemMetrics({ gpus: [], sampled_hz: 1 })).toBeUndefined();
    expect(parseSystemMetrics({ cpu_util: 'high', sampled_hz: 1 })).toBeUndefined();
  });

  it('parses a minimal sample (cpu + sampled_hz only)', () => {
    const parsed = parseSystemMetrics({ cpu_util: 0.5, sampled_hz: 2 });
    expect(parsed).not.toBeUndefined();
    expect(parsed?.cpu_util).toBe(0.5);
    expect(parsed?.sampled_hz).toBe(2);
    expect(parsed?.gpus).toEqual([]);
    expect(parsed?.program_fps).toBeUndefined();
  });

  it('parses optional host-memory + program_fps fields', () => {
    const parsed = parseSystemMetrics({
      cpu_util: 0.5,
      mem_used_bytes: 1000,
      mem_total_bytes: 4000,
      program_fps: 50,
      sampled_hz: 2,
    });
    expect(parsed?.mem_used_bytes).toBe(1000);
    expect(parsed?.mem_total_bytes).toBe(4000);
    expect(parsed?.program_fps).toBe(50);
  });

  it('parses our-process self_* host fields when present', () => {
    const parsed = parseSystemMetrics({
      cpu_util: 0.5,
      self_cpu_util: 0.2,
      mem_used_bytes: 4000,
      self_mem_used_bytes: 1500,
      mem_total_bytes: 8000,
      sampled_hz: 2,
    });
    expect(parsed?.self_cpu_util).toBe(0.2);
    expect(parsed?.self_mem_used_bytes).toBe(1500);
  });

  it('leaves self_* host fields undefined when absent (never a false 0)', () => {
    const parsed = parseSystemMetrics({ cpu_util: 0.5, sampled_hz: 2 });
    expect(parsed?.self_cpu_util).toBeUndefined();
    expect(parsed?.self_mem_used_bytes).toBeUndefined();
  });

  it('ignores a mistyped self_* host field (undefined, not 0)', () => {
    const parsed = parseSystemMetrics({
      cpu_util: 0.5,
      self_cpu_util: 'lots',
      self_mem_used_bytes: null,
      sampled_hz: 2,
    });
    expect(parsed?.self_cpu_util).toBeUndefined();
    expect(parsed?.self_mem_used_bytes).toBeUndefined();
  });

  it('parses a GPU entry and drops malformed ones', () => {
    const parsed = parseSystemMetrics({
      cpu_util: 0.5,
      sampled_hz: 2,
      gpus: [
        {
          id: 'gpu-0',
          vendor: 'nvidia',
          name: 'RTX 4060',
          compute_util: 0.8,
          mem_used_bytes: 2_000,
          mem_total_bytes: 8_000,
          encoder_util: 0.4,
          decoder_util: 0.1,
          encoder_sessions: 3,
          encoder_session_ceiling: 5,
        },
        { id: 'bad' }, // missing required numeric fields — dropped
        { vendor: 'amd', compute_util: 0.2, mem_used_bytes: 1, mem_total_bytes: 2 }, // missing id — dropped
      ],
    });
    expect(parsed?.gpus).toHaveLength(1);
    const gpu = parsed?.gpus[0];
    expect(gpu?.id).toBe('gpu-0');
    expect(gpu?.vendor).toBe('nvidia');
    expect(gpu?.compute_util).toBe(0.8);
    expect(gpu?.encoder_sessions).toBe(3);
    expect(gpu?.encoder_session_ceiling).toBe(5);
  });

  it('parses our-process self_* GPU fields when present', () => {
    const parsed = parseSystemMetrics({
      cpu_util: 0.5,
      sampled_hz: 2,
      gpus: [
        {
          id: 'gpu-0',
          vendor: 'nvidia',
          compute_util: 0.8,
          self_compute_util: 0.3,
          mem_used_bytes: 6_000,
          self_mem_used_bytes: 2_000,
          mem_total_bytes: 8_000,
          encoder_util: 0.4,
          self_encoder_util: 0.2,
          decoder_util: 0.1,
          self_decoder_util: 0.05,
          encoder_sessions: 6,
          self_encoder_sessions: 2,
        },
      ],
    });
    const gpu = parsed?.gpus[0];
    expect(gpu?.self_compute_util).toBe(0.3);
    expect(gpu?.self_mem_used_bytes).toBe(2_000);
    expect(gpu?.self_encoder_util).toBe(0.2);
    expect(gpu?.self_decoder_util).toBe(0.05);
    expect(gpu?.self_encoder_sessions).toBe(2);
  });

  it('leaves self_* GPU fields undefined when absent (never a false 0)', () => {
    const parsed = parseSystemMetrics({
      cpu_util: 0.5,
      sampled_hz: 2,
      gpus: [
        {
          id: 'gpu-0',
          vendor: 'nvidia',
          compute_util: 0.8,
          mem_used_bytes: 6_000,
          mem_total_bytes: 8_000,
        },
      ],
    });
    const gpu = parsed?.gpus[0];
    expect(gpu?.self_compute_util).toBeUndefined();
    expect(gpu?.self_mem_used_bytes).toBeUndefined();
    expect(gpu?.self_encoder_util).toBeUndefined();
    expect(gpu?.self_decoder_util).toBeUndefined();
    expect(gpu?.self_encoder_sessions).toBeUndefined();
  });

  it('coerces an unknown vendor string to "other"', () => {
    const parsed = parseSystemMetrics({
      cpu_util: 0.1,
      sampled_hz: 1,
      gpus: [
        {
          id: 'g',
          vendor: 'wibble',
          compute_util: 0.1,
          mem_used_bytes: 1,
          mem_total_bytes: 2,
        },
      ],
    });
    expect(parsed?.gpus[0]?.vendor).toBe('other');
  });
});

describe('SystemMetricsRing', () => {
  it('exposes the latest sample as `current` (conflates)', () => {
    const ring = new SystemMetricsRing();
    expect(ring.snapshot().current).toBeUndefined();
    ring.push(sample({ cpu_util: 0.1 }));
    ring.push(sample({ cpu_util: 0.9 }));
    expect(ring.snapshot().current?.cpu_util).toBe(0.9);
  });

  it('builds parallel series in arrival order', () => {
    const ring = new SystemMetricsRing();
    ring.push(sample({ cpu_util: 0.1, program_fps: 10 }));
    ring.push(sample({ cpu_util: 0.2, program_fps: 20 }));
    ring.push(sample({ cpu_util: 0.3, program_fps: 30 }));
    const { series } = ring.snapshot();
    expect(series.cpu).toEqual([0.1, 0.2, 0.3]);
    expect(series.fps).toEqual([10, 20, 30]);
  });

  it('derives gpu0 util, vram fraction, nvenc sessions and dec util from the first GPU', () => {
    const ring = new SystemMetricsRing();
    ring.push(
      sample({
        gpus: [
          {
            id: 'g0',
            vendor: 'nvidia',
            compute_util: 0.5,
            mem_used_bytes: 2_000,
            mem_total_bytes: 8_000,
            encoder_sessions: 4,
            decoder_util: 0.3,
          },
        ],
      }),
    );
    const { series } = ring.snapshot();
    expect(series.gpu0Util).toEqual([0.5]);
    // VRAM fraction = used / total = 0.25.
    expect(series.vram).toEqual([0.25]);
    expect(series.nvenc).toEqual([4]);
    expect(series.dec).toEqual([0.3]);
  });

  it('builds a per-GPU series keyed by device id for every GPU', () => {
    const ring = new SystemMetricsRing();
    ring.push(
      sample({
        gpus: [
          {
            id: 'g0',
            vendor: 'nvidia',
            compute_util: 0.5,
            self_compute_util: 0.2,
            mem_used_bytes: 2_000,
            mem_total_bytes: 8_000,
            encoder_sessions: 6,
            self_encoder_sessions: 2,
            decoder_util: 0.3,
          },
          {
            id: 'g1',
            vendor: 'nvidia',
            compute_util: 0.9,
            mem_used_bytes: 4_000,
            mem_total_bytes: 8_000,
          },
        ],
      }),
    );
    const { gpuSeries } = ring.snapshot();
    expect(gpuSeries).toHaveLength(2);
    const [s0, s1] = gpuSeries;
    expect(s0?.id).toBe('g0');
    expect(s0?.compute).toEqual([0.5]);
    expect(s0?.selfCompute).toEqual([0.2]);
    expect(s0?.nvenc).toEqual([6]);
    expect(s0?.selfNvenc).toEqual([2]);
    expect(s1?.id).toBe('g1');
    expect(s1?.compute).toEqual([0.9]);
    // No self_* on g1: the self series carry undefined gaps, never a false 0.
    expect(s1?.selfCompute).toEqual([undefined]);
    expect(s1?.selfNvenc).toEqual([undefined]);
  });

  it('aligns a per-GPU series across samples even if a GPU disappears for one tick', () => {
    const ring = new SystemMetricsRing();
    const gpu = (id: string, compute: number): SystemMetrics['gpus'][number] => ({
      id,
      vendor: 'nvidia',
      compute_util: compute,
      mem_used_bytes: 1,
      mem_total_bytes: 2,
    });
    ring.push(sample({ gpus: [gpu('g0', 0.1), gpu('g1', 0.2)] }));
    ring.push(sample({ gpus: [gpu('g0', 0.3)] })); // g1 missing this tick
    ring.push(sample({ gpus: [gpu('g0', 0.4), gpu('g1', 0.5)] }));
    const { gpuSeries } = ring.snapshot();
    const g1 = gpuSeries.find((s) => s.id === 'g1');
    // g1's compute series stays length-3 and aligned, with a 0 where it was absent.
    expect(g1?.compute).toEqual([0.2, 0, 0.5]);
  });

  it('substitutes 0 for an absent metric so series stay aligned', () => {
    const ring = new SystemMetricsRing();
    // A sample with no GPUs and no program_fps: every derived series falls to 0.
    ring.push(sample({ gpus: [] }));
    const { series } = ring.snapshot();
    expect(series.gpu0Util).toEqual([0]);
    expect(series.vram).toEqual([0]);
    expect(series.nvenc).toEqual([0]);
    expect(series.dec).toEqual([0]);
    expect(series.fps).toEqual([0]);
  });

  it('is bounded: never grows past the ring capacity', () => {
    const ring = new SystemMetricsRing();
    const overflow = METRICS_RING_CAPACITY + 50;
    for (let i = 0; i < overflow; i += 1) {
      ring.push(sample({ cpu_util: i / overflow }));
    }
    const { series, current } = ring.snapshot();
    expect(series.cpu).toHaveLength(METRICS_RING_CAPACITY);
    // The oldest samples were dropped; the newest sample is retained.
    expect(current?.cpu_util).toBeCloseTo((overflow - 1) / overflow, 10);
    // The first retained sample is from index `overflow - CAPACITY`, not 0.
    const firstRetainedIndex = overflow - METRICS_RING_CAPACITY;
    expect(series.cpu[0]).toBeCloseTo(firstRetainedIndex / overflow, 10);
  });
});
