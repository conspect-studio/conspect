// Lazy loader for the Google Cast Web Sender SDK. Injects the gstatic
// `cast_sender.js` script tag at most once per page load, resolves once
// `window.__onGCastApiAvailable` fires, and initialises `CastContext` with the
// Default Media Receiver + origin-scoped auto-join policy (ADR-M011 posture:
// nominative use only, no custom receiver).
//
// Google Cast and Chromecast are trademarks of Google LLC; this project is
// independent and not affiliated with or endorsed by Google.
import './castTypes';
import type { CastContext } from './castTypes';

const SDK_URL = 'https://www.gstatic.com/cv/js/sender/v1/cast_sender.js?loadCastFramework=1';

let sdkPromise: Promise<CastContext> | null = null;

/** Reset the cached loader promise — test-only hook. */
export function resetCastSdkLoaderForTests(): void {
  sdkPromise = null;
}

/**
 * Load the Cast Web Sender SDK (idempotent) and return the initialised
 * `CastContext` singleton. Rejects if the SDK reports itself unavailable or
 * the script fails to load.
 */
export function loadCastSdk(): Promise<CastContext> {
  if (sdkPromise !== null) {
    return sdkPromise;
  }
  sdkPromise = new Promise<CastContext>((resolve, reject) => {
    const existingCast = window.cast;
    if (existingCast !== undefined) {
      resolve(initializeContext(existingCast.framework));
      return;
    }

    const previousCallback = window.__onGCastApiAvailable;
    window.__onGCastApiAvailable = (isAvailable: boolean): void => {
      previousCallback?.(isAvailable);
      if (!isAvailable) {
        reject(new Error('Cast SDK reported unavailable.'));
        return;
      }
      const framework = window.cast?.framework;
      if (framework === undefined) {
        reject(new Error('Cast SDK loaded without cast.framework.'));
        return;
      }
      resolve(initializeContext(framework));
    };

    const script = document.createElement('script');
    script.src = SDK_URL;
    script.async = true;
    script.onerror = (): void => {
      reject(new Error('Could not load the Cast Web Sender SDK.'));
    };
    document.head.appendChild(script);
  });
  // A failed load must not permanently poison future attempts (e.g. a
  // transient network blip): clear the cache on rejection.
  sdkPromise.catch(() => {
    sdkPromise = null;
  });
  return sdkPromise;
}

function initializeContext(framework: { CastContext: { getInstance: () => CastContext } }): CastContext {
  const context = framework.CastContext.getInstance();
  const chromeCast = window.chrome?.cast;
  if (chromeCast === undefined) {
    throw new Error('Cast SDK loaded without chrome.cast.');
  }
  context.setOptions({
    receiverApplicationId: chromeCast.media.DEFAULT_MEDIA_RECEIVER_APP_ID,
    autoJoinPolicy: chromeCast.AutoJoinPolicy.ORIGIN_SCOPED,
  });
  return context;
}
