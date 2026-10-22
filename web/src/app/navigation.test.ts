// The primary navigation gains a Devices entry between Outputs and Monitoring
// (managed-devices.md §9 / work-schedule DEV-A6).
import { describe, expect, it } from 'vitest';

import { NAV_ITEMS } from './navigation';

describe('primary navigation', () => {
  it('places /devices between /outputs and /monitoring', () => {
    const paths = NAV_ITEMS.map((item) => item.path);
    const devices = paths.indexOf('/devices');
    expect(devices).toBeGreaterThan(paths.indexOf('/outputs'));
    expect(devices).toBeLessThan(paths.indexOf('/monitoring'));
  });
});
