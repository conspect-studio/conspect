// Unit tests for the pure form-state <-> config-body mapping of the Probes
// management form. The body shapes asserted here mirror the Rust config schema
// EXACTLY (crates/multiview-config/src/probe.rs: `Probe`, `ProbeKind`,
// `DetectionZone`, `Dwell`, `LoudnessTarget`; severity is the X.733
// `PerceivedSeverity` PascalCase wire form) — the control plane 422s anything
// else (ADR-W015). No DOM, no React.
import { describe, expect, it } from 'vitest';

import {
  cellIdsFromLayouts,
  emptyProbeForm,
  parseProbeFormKind,
  PROBE_SEVERITIES,
  probeFormFromRecord,
  probeFormToBody,
  validateProbeForm,
  withProbeKind,
  withProbeLoudnessStandard,
} from './forms';
import type { ProbeFormState } from './forms';

/** Narrow a parsed form (`probeFormFromRecord` refuses unknown kinds). */
function defined<T>(value: T | undefined): T {
  if (value === undefined) {
    throw new Error('expected a parsed form for a known kind');
  }
  return value;
}

function probeForm(over: Partial<ProbeFormState> = {}): ProbeFormState {
  return { ...emptyProbeForm(), id: 'p1', name: 'Probe 1', cell: 'cell-a', ...over };
}

describe('probeFormToBody', () => {
  it('writes the exact black-probe body with the default full-frame zone omitted', () => {
    const body = probeFormToBody(
      probeForm({ kind: 'black', lumaThreshold: '16' }),
    );
    expect(body).toEqual({
      id: 'p1',
      cell: 'cell-a',
      kind: 'black',
      luma_threshold: 16,
      dwell: { up_ms: 1000, down_ms: 1000 },
      severity: 'Warning',
      latched: false,
    });
  });

  it('writes the zone block when enabled (black + freeze)', () => {
    const body = probeFormToBody(
      probeForm({
        kind: 'freeze',
        differenceThreshold: '5',
        zoneEnabled: true,
        zoneX: '0.25',
        zoneY: '0.25',
        zoneW: '0.5',
        zoneH: '0.5',
      }),
    );
    expect(body.kind).toBe('freeze');
    expect(body.difference_threshold).toBe(5);
    expect(body.zone).toEqual({ x: 0.25, y: 0.25, w: 0.5, h: 0.5 });
  });

  it('writes the silence body with a decimal dBFS level', () => {
    const body = probeFormToBody(
      probeForm({ kind: 'silence', levelDbfs: '-60.5', severity: 'Critical', latched: true }),
    );
    expect(body).toEqual({
      id: 'p1',
      cell: 'cell-a',
      kind: 'silence',
      level_dbfs: -60.5,
      dwell: { up_ms: 1000, down_ms: 1000 },
      severity: 'Critical',
      latched: true,
    });
  });

  it('writes the loudness body with the standard-specific target field', () => {
    const r128 = probeFormToBody(
      probeForm({ kind: 'loudness', loudnessStandard: 'r128', loudnessTarget: '-23', loudnessTruePeak: '-1' }),
    );
    expect(r128.target).toEqual({ kind: 'r128', target_lufs: -23, max_true_peak_dbtp: -1 });

    const a85 = probeFormToBody(
      probeForm({ kind: 'loudness', loudnessStandard: 'a85', loudnessTarget: '-24', loudnessTruePeak: '-2' }),
    );
    expect(a85.target).toEqual({ kind: 'a85', target_lkfs: -24, max_true_peak_dbtp: -2 });
  });

  it('writes the dwell windows from the form', () => {
    const body = probeFormToBody(
      probeForm({ kind: 'black', lumaThreshold: '16', dwellUpMs: '5000', dwellDownMs: '250' }),
    );
    expect(body.dwell).toEqual({ up_ms: 5000, down_ms: 250 });
  });

  it('preserves unmanaged body fields verbatim across an edit', () => {
    const record = {
      id: 'p1',
      name: 'P',
      body: {
        id: 'p1',
        cell: 'cell-a',
        kind: 'black',
        luma_threshold: 16,
        future_field: { nested: true },
      },
    };
    const form = defined(probeFormFromRecord(record));
    const body = probeFormToBody(form);
    expect(body.future_field).toEqual({ nested: true });
  });
});

describe('probeFormFromRecord', () => {
  it('round-trips a black probe with a zone', () => {
    const record = {
      id: 'p1',
      name: 'P',
      body: {
        id: 'p1',
        cell: 'cell-a',
        kind: 'black',
        luma_threshold: 32,
        zone: { x: 0.1, y: 0.2, w: 0.3, h: 0.4 },
        dwell: { up_ms: 2000, down_ms: 500 },
        severity: 'Major',
        latched: true,
      },
    };
    const form = defined(probeFormFromRecord(record));
    expect(form.kind).toBe('black');
    expect(form.cell).toBe('cell-a');
    expect(form.lumaThreshold).toBe('32');
    expect(form.zoneEnabled).toBe(true);
    expect(form.zoneX).toBe('0.1');
    expect(form.zoneH).toBe('0.4');
    expect(form.dwellUpMs).toBe('2000');
    expect(form.dwellDownMs).toBe('500');
    expect(form.severity).toBe('Major');
    expect(form.latched).toBe(true);
    expect(probeFormToBody(form)).toEqual(record.body);
  });

  it('defaults dwell/severity/latched to the schema defaults when absent', () => {
    const record = {
      id: 'p1',
      name: 'P',
      body: { id: 'p1', cell: 'c', kind: 'silence', level_dbfs: -60 },
    };
    const form = defined(probeFormFromRecord(record));
    expect(form.zoneEnabled).toBe(false);
    expect(form.dwellUpMs).toBe('1000');
    expect(form.dwellDownMs).toBe('1000');
    expect(form.severity).toBe('Cleared');
    expect(form.latched).toBe(false);
  });

  it('parses a loudness probe onto the standard fields', () => {
    const record = {
      id: 'p1',
      name: 'P',
      body: {
        id: 'p1',
        cell: 'c',
        kind: 'loudness',
        target: { kind: 'a85', target_lkfs: -24, max_true_peak_dbtp: -2 },
      },
    };
    const form = defined(probeFormFromRecord(record));
    expect(form.kind).toBe('loudness');
    expect(form.loudnessStandard).toBe('a85');
    expect(form.loudnessTarget).toBe('-24');
    expect(form.loudnessTruePeak).toBe('-2');
  });

  it('refuses an unknown kind with undefined (never a fold)', () => {
    const record = {
      id: 'p1',
      name: 'P',
      body: { id: 'p1', cell: 'c', kind: 'psnr', threshold: 30 },
    };
    expect(probeFormFromRecord(record)).toBeUndefined();
    expect(parseProbeFormKind('psnr')).toBeUndefined();
    expect(parseProbeFormKind('black')).toBe('black');
  });
});

describe('validateProbeForm', () => {
  it('requires id (create), name, and cell', () => {
    const errors = validateProbeForm(
      { ...emptyProbeForm(), lumaThreshold: '16' },
      true,
    );
    expect(errors.id).toBe('required');
    expect(errors.name).toBe('required');
    expect(errors.cell).toBe('required');
  });

  it('bounds the black luma threshold to 0..=255', () => {
    expect(validateProbeForm(probeForm({ kind: 'black', lumaThreshold: '256' }), true).lumaThreshold).toBe('int-range');
    expect(validateProbeForm(probeForm({ kind: 'black', lumaThreshold: '-1' }), true).lumaThreshold).toBe('int-range');
    expect(validateProbeForm(probeForm({ kind: 'black', lumaThreshold: '255' }), true).lumaThreshold).toBeUndefined();
  });

  it('bounds the freeze difference threshold to 0..=1000 per-mille', () => {
    expect(
      validateProbeForm(probeForm({ kind: 'freeze', differenceThreshold: '1001' }), true)
        .differenceThreshold,
    ).toBe('int-range');
    expect(
      validateProbeForm(probeForm({ kind: 'freeze', differenceThreshold: '1000' }), true)
        .differenceThreshold,
    ).toBeUndefined();
  });

  it('rejects a zone outside the unit square or with non-positive extent', () => {
    const outside = validateProbeForm(
      probeForm({
        kind: 'black',
        lumaThreshold: '16',
        zoneEnabled: true,
        zoneX: '0.6',
        zoneY: '0',
        zoneW: '0.5',
        zoneH: '1',
      }),
      true,
    );
    expect(outside.zoneW).toBe('zone-extent');

    const flat = validateProbeForm(
      probeForm({
        kind: 'black',
        lumaThreshold: '16',
        zoneEnabled: true,
        zoneX: '0',
        zoneY: '0',
        zoneW: '0',
        zoneH: '1',
      }),
      true,
    );
    expect(flat.zoneW).toBe('zone-extent');

    const garbage = validateProbeForm(
      probeForm({
        kind: 'black',
        lumaThreshold: '16',
        zoneEnabled: true,
        zoneX: 'left',
        zoneY: '0',
        zoneW: '1',
        zoneH: '1',
      }),
      true,
    );
    expect(garbage.zoneX).toBe('number');
  });

  it('requires finite decimal levels for silence and loudness', () => {
    expect(validateProbeForm(probeForm({ kind: 'silence', levelDbfs: 'quiet' }), true).levelDbfs).toBe('number');
    expect(validateProbeForm(probeForm({ kind: 'silence', levelDbfs: '-60' }), true).levelDbfs).toBeUndefined();
    expect(
      validateProbeForm(probeForm({ kind: 'loudness', loudnessTarget: '' }), true).loudnessTarget,
    ).toBe('number');
    expect(
      validateProbeForm(probeForm({ kind: 'loudness', loudnessTruePeak: 'x' }), true)
        .loudnessTruePeak,
    ).toBe('number');
  });

  it('requires whole-number, non-negative dwell windows', () => {
    expect(validateProbeForm(probeForm({ kind: 'black', lumaThreshold: '16', dwellUpMs: '-1' }), true).dwellUpMs).toBe('int-range');
    expect(validateProbeForm(probeForm({ kind: 'black', lumaThreshold: '16', dwellDownMs: '1.5' }), true).dwellDownMs).toBe('int-range');
    expect(validateProbeForm(probeForm({ kind: 'black', lumaThreshold: '16', dwellUpMs: '0' }), true).dwellUpMs).toBeUndefined();
  });
});

describe('withProbeKind / withProbeLoudnessStandard', () => {
  it('keeps the shared fields and resets kind-specific parameters', () => {
    const black = probeForm({
      kind: 'black',
      lumaThreshold: '99',
      zoneEnabled: true,
      zoneX: '0.5',
      dwellUpMs: '5000',
      severity: 'Critical',
      latched: true,
    });
    const silence = withProbeKind(black, 'silence');
    expect(silence.kind).toBe('silence');
    expect(silence.cell).toBe('cell-a');
    expect(silence.dwellUpMs).toBe('5000');
    expect(silence.severity).toBe('Critical');
    expect(silence.latched).toBe(true);
    // Kind-scoped parameters reset to defaults.
    expect(silence.lumaThreshold).toBe(emptyProbeForm().lumaThreshold);
    expect(silence.zoneEnabled).toBe(false);
    expect(silence.zoneX).toBe(emptyProbeForm().zoneX);
  });

  it('is identity for the same kind', () => {
    const form = probeForm({ kind: 'freeze' });
    expect(withProbeKind(form, 'freeze')).toBe(form);
  });

  it('switching the loudness standard resets the target defaults', () => {
    const form = probeForm({ kind: 'loudness' });
    const a85 = withProbeLoudnessStandard(form, 'a85');
    expect(a85.loudnessStandard).toBe('a85');
    expect(a85.loudnessTarget).toBe('-24');
    expect(a85.loudnessTruePeak).toBe('-2');
    const back = withProbeLoudnessStandard(a85, 'r128');
    expect(back.loudnessTarget).toBe('-23');
    expect(back.loudnessTruePeak).toBe('-1');
    expect(withProbeLoudnessStandard(form, 'r128')).toBe(form);
  });
});

describe('PROBE_SEVERITIES', () => {
  it('lists the X.733 wire values in ascending urgency', () => {
    expect(PROBE_SEVERITIES).toEqual([
      'Cleared',
      'Indeterminate',
      'Warning',
      'Minor',
      'Major',
      'Critical',
    ]);
  });
});

describe('cellIdsFromLayouts', () => {
  it('collects cell ids from canvas-bearing layouts only (drafts would poison export)', () => {
    const layouts = [
      // No canvas: a preset/draft — its cells still count, after canvas layouts.
      { body: { cells: [{ id: 'draft-cell' }] } },
      // The working layout (carries a canvas).
      {
        body: {
          canvas: { width: 64 },
          cells: [{ id: 'cell-a' }, { id: 'cell-b' }, { id: 'cell-a' }],
        },
      },
    ];
    expect(cellIdsFromLayouts(layouts)).toEqual(['cell-a', 'cell-b']);
  });

  it('tolerates malformed bodies', () => {
    expect(cellIdsFromLayouts([{ body: null }, { body: 'x' }, { body: { cells: 'no' } }])).toEqual(
      [],
    );
    expect(cellIdsFromLayouts([])).toEqual([]);
  });
});
