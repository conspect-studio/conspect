import { describe, expect, it } from 'vitest';
import { deskControlReason, tallyForSource } from './deskSafety';

describe('Production desk safety', () => {
  it('never derives tally from source order or requested selection', () => {
    expect(tallyForSource('cam-b', undefined, true)).toBe('unknown');
    expect(tallyForSource('cam-b', { program_source: 'cam-a', preview_source: 'cam-b' }, true)).toBe('preview');
    expect(tallyForSource('cam-a', { program_source: 'cam-a', preview_source: 'cam-b' }, false)).toBe('unknown');
  });
  it('blocks unavailable, disconnected, stale, in-flight and unresolved operations', () => {
    expect(deskControlReason(false, true, true, false, false)).not.toBeUndefined();
    expect(deskControlReason(true, false, true, false, false)).not.toBeUndefined();
    expect(deskControlReason(true, true, false, false, false)).not.toBeUndefined();
    expect(deskControlReason(true, true, true, true, false)).not.toBeUndefined();
    expect(deskControlReason(true, true, true, false, true)).not.toBeUndefined();
    expect(deskControlReason(true, true, true, false, false)).toBeUndefined();
  });
});
