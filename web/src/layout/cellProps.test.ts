// Unit tests for the shared per-cell properties model (the full config Cell
// schema beyond placement): on_loss failover slate, border, QoS, and the
// appearance scalars — including the lossless round-trip discipline for
// anything the editor does not render. No DOM, no React.
import { describe, expect, it } from 'vitest';

import {
  CELL_PROPERTY_KEYS,
  DEGRADATION_MODES,
  emptyCellProperties,
  FAILOVER_SLATES,
  onLossOf,
  parseCellProperties,
  SCALER_MODES,
  serializeCellProperties,
  validateCellProperties,
} from './cellProps';

describe('constants mirror the Rust schema', () => {
  it('failover slates are exactly the FailoverSlate variants (snake_case)', () => {
    // crates/multiview-config/src/failover.rs: Bars | NoSignal | Black.
    expect(FAILOVER_SLATES).toEqual(['bars', 'no_signal', 'black']);
  });

  it('scaler and degradation tokens match the schema docs', () => {
    expect(SCALER_MODES).toEqual(['auto', 'bilinear', 'lanczos']);
    expect(DEGRADATION_MODES).toEqual([
      'maintain-fps',
      'maintain-resolution',
      'balanced',
    ]);
  });

  it('the managed keys cover every Cell property field', () => {
    expect([...CELL_PROPERTY_KEYS].sort()).toEqual(
      [
        'align',
        'opacity',
        'corner_radius',
        'scaler',
        'visible',
        'static_friendly',
        'border',
        'qos',
        'on_loss',
      ].sort(),
    );
  });
});

describe('parse / serialize round-trip', () => {
  it('an empty record parses to empty properties and serializes to {}', () => {
    const props = parseCellProperties({});
    expect(props).toEqual(emptyCellProperties());
    expect(serializeCellProperties(props)).toEqual({});
  });

  it('round-trips every rendered field losslessly', () => {
    const record = {
      align: 'top_left',
      opacity: 0.5,
      corner_radius: 8,
      scaler: 'lanczos',
      visible: false,
      static_friendly: true,
      border: { width_px: 2, color: '#ff0000', style: 'solid' },
      qos: { priority: 10, degradation: 'maintain-fps' },
      on_loss: { slate: 'black' },
    };
    const props = parseCellProperties(record);
    expect(props.align).toBe('top_left');
    expect(props.opacity).toBe(0.5);
    expect(props.cornerRadius).toBe(8);
    expect(props.scaler).toBe('lanczos');
    expect(props.visible).toBe(false);
    expect(props.staticFriendly).toBe(true);
    expect(props.border?.widthPx).toBe(2);
    expect(props.border?.color).toBe('#ff0000');
    expect(props.border?.style).toBe('solid');
    expect(props.qos?.priority).toBe(10);
    expect(props.qos?.degradation).toBe('maintain-fps');
    expect(props.onLoss?.slate).toBe('black');
    expect(serializeCellProperties(props)).toEqual(record);
  });

  it('preserves unknown sub-fields on border / qos / on_loss verbatim', () => {
    const record = {
      border: { width_px: 1, glow: 'soft' },
      qos: { priority: 1, future_knob: { a: 1 } },
      on_loss: { slate: 'hold_card', card: 'be-right-back' },
    };
    const props = parseCellProperties(record);
    // The unknown slate tag is surfaced so the UI can show it, and the whole
    // record is re-emitted verbatim (#[non_exhaustive] forward-compat).
    expect(props.onLoss?.slate).toBe('hold_card');
    expect(serializeCellProperties(props)).toEqual(record);
  });

  it('preserves managed keys whose values do not parse (never drops data)', () => {
    const record = {
      opacity: 'half',
      visible: 'yes',
      border: 'thick',
      on_loss: 'bars',
    };
    const props = parseCellProperties(record);
    expect(props.opacity).toBeUndefined();
    expect(props.visible).toBeUndefined();
    expect(props.border).toBeUndefined();
    expect(props.onLoss).toBeUndefined();
    expect(serializeCellProperties(props)).toEqual(record);
  });

  it('an empty border record stays an empty record (present, not dropped)', () => {
    const props = parseCellProperties({ border: {} });
    expect(props.border).toBeDefined();
    expect(serializeCellProperties(props)).toEqual({ border: {} });
  });
});

describe('onLossOf', () => {
  it('builds the internally-tagged record for a known slate', () => {
    const model = onLossOf('no_signal');
    expect(model?.slate).toBe('no_signal');
    expect(model?.raw).toEqual({ slate: 'no_signal' });
  });

  it('clears the policy with undefined (omitted => engine default bars)', () => {
    expect(onLossOf(undefined)).toBeUndefined();
  });
});

describe('validateCellProperties', () => {
  it('accepts empty properties and a fully-valid set', () => {
    expect(validateCellProperties(emptyCellProperties(), 'cells.0')).toEqual([]);
    const props = parseCellProperties({
      opacity: 1,
      corner_radius: 0,
      border: { width_px: 4, color: '#abc' },
      qos: { priority: -5 },
    });
    expect(validateCellProperties(props, 'cells.0')).toEqual([]);
  });

  it('flags opacity outside 0..1', () => {
    const props = parseCellProperties({ opacity: 1.5 });
    expect(validateCellProperties(props, 'cells.0')).toEqual([
      { path: 'cells.0.opacity', code: 'opacity-range', severity: 'error' },
    ]);
  });

  it('flags a negative or fractional corner radius', () => {
    const props = parseCellProperties({ corner_radius: -1 });
    expect(
      validateCellProperties(props, 'c').map((issue) => issue.code),
    ).toContain('corner-radius-invalid');
    const fractional = parseCellProperties({ corner_radius: 2.5 });
    expect(
      validateCellProperties(fractional, 'c').map((issue) => issue.code),
    ).toContain('corner-radius-invalid');
  });

  it('flags a bad border width and a non-hex border colour', () => {
    const props = parseCellProperties({
      border: { width_px: -2, color: 'red' },
    });
    const issues = validateCellProperties(props, 'c');
    const codes = issues.map((issue) => issue.code);
    expect(codes).toContain('border-width-invalid');
    expect(codes).toContain('border-color-hex');
    // Rust never validates Border.color, so a bad colour is advisory only;
    // the width is a u32 in the schema, so that one stays an error.
    expect(issues.find((i) => i.code === 'border-color-hex')?.severity).toBe('warning');
    expect(issues.find((i) => i.code === 'border-width-invalid')?.severity).toBe('error');
  });

  it('flags a non-integer qos priority', () => {
    const props = parseCellProperties({ qos: { priority: 1.5 } });
    expect(validateCellProperties(props, 'cells.2')).toEqual([
      { path: 'cells.2.qos.priority', code: 'qos-priority-int', severity: 'error' },
    ]);
  });
});
