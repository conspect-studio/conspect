// Browser support detection for the Cast Web Sender SDK: Chrome and Edge
// (Chromium-based desktop) ship it; Safari and Firefox never will, and iOS
// Chrome/Edge are still WebKit under the hood and do not carry it either.
// AirPlay is a separate, out-of-scope sender path.

/** True on a desktop browser where the Cast Web Sender SDK can load. */
export function isCastSupportedBrowser(
  nav: Pick<Navigator, 'userAgent'> = navigator,
  win: { chrome?: unknown } = window,
): boolean {
  const ua = nav.userAgent;
  const isIos = /iPad|iPhone|iPod/.test(ua) || (ua.includes('Macintosh') && ua.includes('Mobile'));
  if (isIos) {
    return false;
  }
  // `window.chrome` is present on both Chrome and Edge (Chromium); Safari and
  // Firefox never define it.
  return win.chrome !== undefined;
}
