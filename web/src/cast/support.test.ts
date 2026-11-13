import { describe, expect, it } from 'vitest';

import { isCastSupportedBrowser } from './support';

describe('isCastSupportedBrowser', () => {
  it('is true for Chrome/Edge (window.chrome present) on desktop', () => {
    const nav = { userAgent: 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) Chrome/120.0' };
    expect(isCastSupportedBrowser(nav, { chrome: {} })).toBe(true);
  });

  it('is false for Safari (no window.chrome)', () => {
    const nav = { userAgent: 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15) AppleWebKit/605 Safari/605' };
    expect(isCastSupportedBrowser(nav, {})).toBe(false);
  });

  it('is false for Firefox (no window.chrome)', () => {
    const nav = { userAgent: 'Mozilla/5.0 (X11; Linux x86_64; rv:109.0) Gecko/20100101 Firefox/119.0' };
    expect(isCastSupportedBrowser(nav, {})).toBe(false);
  });

  it('is false on iOS even if window.chrome were somehow present', () => {
    const nav = {
      userAgent:
        'Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/120.0 Mobile/15E148',
    };
    expect(isCastSupportedBrowser(nav, { chrome: {} })).toBe(false);
  });
});
