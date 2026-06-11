// useHealth: the SA-0 health-warning binding over the `alerts` topic. These
// tests cover the pure, deterministic core:
//   - parseHealthWarning: defensive narrowing of an unknown `data` body.
//   - HealthWarningMap: the bounded, code-keyed fold (raise upserts, clear
//     removes, latched codes never stack) the banner reads.
import { describe, expect, it } from 'vitest';

import {
  HEALTH_WARNING_CLEARED,
  HEALTH_WARNING_RAISED,
  HealthWarningMap,
  parseHealthWarning,
} from './useHealth';
import type { HealthWarning } from './useHealth';

function warning(over: Partial<HealthWarning> = {}): HealthWarning {
  return {
    code: 'gpu-present-no-vulkan-adapter',
    severity: 'warning',
    subsystem: 'compositor',
    message: 'GPU RTX 4060 detected but compositing fell back to CPU.',
    remediation: 'Set NVIDIA_DRIVER_CAPABILITIES to include `graphics`.',
    since: 1_700_000_000_000_000_000,
    active: true,
    ...over,
  };
}

describe('parseHealthWarning', () => {
  it('returns undefined for non-object data', () => {
    expect(parseHealthWarning(null)).toBeUndefined();
    expect(parseHealthWarning('nope')).toBeUndefined();
    expect(parseHealthWarning(42)).toBeUndefined();
  });

  it('returns undefined when required fields are missing or mistyped', () => {
    expect(parseHealthWarning({ code: 'x' })).toBeUndefined();
    expect(
      parseHealthWarning({
        code: 'x',
        subsystem: 'compositor',
        message: 'm',
        remediation: 'r',
        since: 'soon', // mistyped
        active: true,
      }),
    ).toBeUndefined();
  });

  it('parses a well-formed warning with all actionable fields', () => {
    const parsed = parseHealthWarning(warning());
    expect(parsed).not.toBeUndefined();
    expect(parsed?.code).toBe('gpu-present-no-vulkan-adapter');
    expect(parsed?.severity).toBe('warning');
    expect(parsed?.subsystem).toBe('compositor');
    expect(parsed?.remediation).toContain('graphics');
    expect(parsed?.active).toBe(true);
  });

  it('coerces an unknown severity to "warning" (forward-compatible)', () => {
    const parsed = parseHealthWarning(warning({ severity: 'apocalyptic' as never }));
    expect(parsed?.severity).toBe('warning');
  });
});

describe('HealthWarningMap', () => {
  it('is empty by default (a clean host renders nothing)', () => {
    expect(new HealthWarningMap().active()).toEqual([]);
  });

  it('upserts a raised warning into the active set', () => {
    const map = new HealthWarningMap();
    expect(map.applyEnvelope(HEALTH_WARNING_RAISED, warning())).toBe(true);
    const active = map.active();
    expect(active).toHaveLength(1);
    expect(active[0]?.code).toBe('gpu-present-no-vulkan-adapter');
  });

  it('coalesces a re-raised latched code to ONE entry (cannot stack)', () => {
    const map = new HealthWarningMap();
    map.applyEnvelope(HEALTH_WARNING_RAISED, warning());
    map.applyEnvelope(HEALTH_WARNING_RAISED, warning());
    expect(map.active()).toHaveLength(1);
  });

  it('removes a warning on a cleared event (drops out of the active set)', () => {
    const map = new HealthWarningMap();
    map.applyEnvelope(HEALTH_WARNING_RAISED, warning());
    expect(map.active()).toHaveLength(1);
    // A cleared event removes regardless of the carried `active` flag.
    map.applyEnvelope(HEALTH_WARNING_CLEARED, warning({ active: true }));
    expect(map.active()).toEqual([]);
  });

  it('also removes on a raised event carrying active: false', () => {
    const map = new HealthWarningMap();
    map.applyEnvelope(HEALTH_WARNING_RAISED, warning());
    map.applyEnvelope(HEALTH_WARNING_RAISED, warning({ active: false }));
    expect(map.active()).toEqual([]);
  });

  it('ignores non-health-warning event types and malformed bodies', () => {
    const map = new HealthWarningMap();
    expect(map.applyEnvelope('system.metrics', { cpu_util: 0.5 })).toBe(false);
    expect(map.applyEnvelope(HEALTH_WARNING_RAISED, { code: 'x' })).toBe(false);
    expect(map.active()).toEqual([]);
  });
});
