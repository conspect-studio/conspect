// Lazy loader for the Google Cast Web Sender SDK. Injects the gstatic
// `cast_sender.js` script tag at most once per page load, resolves once
// `window.__onGCastApiAvailable` fires, and points `CastContext` at the
// receiver app: the registered Conspect receiver (`receiver_app_id` from
// `GET /api/v1/cast/config`) or, when none is registered, Google's Default
// Media Receiver. Auto-join is origin-scoped so a reload re-attaches.
//
// Google Cast and Chromecast are trademarks of Google LLC; this project is
// independent and not affiliated with or endorsed by Google.
import './castTypes';
import type { CastContext } from './castTypes';

const SDK_URL = 'https://www.gstatic.com/cv/js/sender/v1/cast_sender.js?loadCastFramework=1';

let sdkPromise: Promise<CastContext> | null = null;
/** The receiver app id last passed to `setOptions` (CAF wants it set once per change). */
let configuredAppId: string | null = null;

/** Reset the cached loader promise — test-only hook. */
export function resetCastSdkLoaderForTests(): void {
  sdkPromise = null;
  configuredAppId = null;
}

function loadContext(): Promise<CastContext> {
  if (sdkPromise !== null) {
    return sdkPromise;
  }
  sdkPromise = new Promise<CastContext>((resolve, reject) => {
    const existingCast = window.cast;
    if (existingCast !== undefined) {
      resolve(existingCast.framework.CastContext.getInstance());
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
      resolve(framework.CastContext.getInstance());
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

/**
 * Load the Cast Web Sender SDK (idempotent) and return the `CastContext`
 * singleton, set to `receiverAppId` (`null` = Google's Default Media
 * Receiver). Rejects if the SDK reports itself unavailable or the script
 * fails to load.
 */
export async function loadCastSdk(receiverAppId: string | null): Promise<CastContext> {
  const context = await loadContext();
  const chromeCast = window.chrome?.cast;
  if (chromeCast === undefined) {
    throw new Error('Cast SDK loaded without chrome.cast.');
  }
  const appId = receiverAppId ?? chromeCast.media.DEFAULT_MEDIA_RECEIVER_APP_ID;
  if (configuredAppId !== appId) {
    context.setOptions({
      receiverApplicationId: appId,
      autoJoinPolicy: chromeCast.AutoJoinPolicy.ORIGIN_SCOPED,
    });
    configuredAppId = appId;
  }
  return context;
}
