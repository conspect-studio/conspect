// App-level provider for the browser-cast controller: one `CastController`
// instance survives page navigation, matching the Cast SDK's own
// one-session-per-tab model. Mount once near the app root (see `App.tsx`).
import { createContext, useContext, useEffect, useMemo, useState, useSyncExternalStore } from 'react';
import type { JSX, ReactNode } from 'react';

import { useCastConfig } from '../api/castStreamsQueries';
import { CastController } from './castController';
import type { CastSnapshot, CastTarget } from './castController';
import { CAST_NAMESPACE } from './castReport';
import { isCastSupportedBrowser } from './support';

const CastControllerContext = createContext<CastController | null>(null);

/** Mount once near the app root; `useCast` reads the shared controller it creates. */
export function CastProvider({
  children,
  controller: injected,
}: {
  readonly children: ReactNode;
  /** Test hook: use this controller instead of a new one. */
  readonly controller?: CastController;
}): JSX.Element {
  // Lazy `useState` initializer: exactly one controller for this provider.
  const [controller] = useState<CastController>(() => injected ?? new CastController());
  useEffect(() => {
    controller.activate();
    return (): void => {
      controller.dispose();
    };
  }, [controller]);
  return <CastControllerContext.Provider value={controller}>{children}</CastControllerContext.Provider>;
}

/** The shared controller (throws outside a `CastProvider`). */
// eslint-disable-next-line react-refresh/only-export-components -- shared hook alongside its provider
export function useCastController(): CastController {
  const controller = useContext(CastControllerContext);
  if (controller === null) {
    throw new Error('useCast must be used within a CastProvider.');
  }
  return controller;
}

/** The controller's live snapshot. */
// eslint-disable-next-line react-refresh/only-export-components -- shared hook alongside its provider
export function useCastSnapshot(): CastSnapshot {
  const controller = useCastController();
  return useSyncExternalStore(controller.subscribe, controller.getSnapshot);
}

/**
 * On a browser that can cast, read `GET /cast/config` and hand the receiver
 * app to the controller (which preloads the SDK so a click opens the picker at
 * once). Returns whether casting is possible here and the receiver app id.
 */
// eslint-disable-next-line react-refresh/only-export-components -- shared hook alongside its provider
export function useCastReceiver(): {
  readonly supported: boolean;
  readonly ready: boolean;
  readonly appId: string | null;
} {
  const controller = useCastController();
  const supported = useMemo(() => isCastSupportedBrowser(), []);
  const config = useCastConfig(supported ? 30_000 : false, supported);
  const appId = config.data?.receiver_app_id ?? null;
  const namespace = config.data?.namespace ?? CAST_NAMESPACE;
  const ready = config.data !== undefined;
  useEffect(() => {
    if (ready) controller.configure({ appId, namespace });
  }, [controller, ready, appId, namespace]);
  return { supported, ready, appId };
}

export interface UseCastResult extends CastSnapshot {
  /** True when this hook's own target is the one currently casting/loading. */
  readonly isThisTarget: boolean;
  readonly start: () => void;
  readonly stop: () => void;
}

const IDLE_VIEW: Omit<CastSnapshot, 'status'> = {
  targetKey: null,
  targetName: null,
  deviceName: null,
  errorText: null,
  errorDetail: null,
  streamId: null,
  output: null,
  receiver: null,
  report: null,
  volume: null,
  muted: null,
  tvUnreachable: false,
};

/** Hooks a component's Cast button into the shared one-per-tab controller. */
// eslint-disable-next-line react-refresh/only-export-components -- shared hook alongside its provider
export function useCast(target: CastTarget): UseCastResult {
  const controller = useCastController();
  const snapshot = useSyncExternalStore(controller.subscribe, controller.getSnapshot);
  const isThisTarget = snapshot.targetKey !== null && snapshot.targetKey === target.key;
  // Another target's button must not show this target's state.
  const effective: CastSnapshot = isThisTarget ? snapshot : { ...IDLE_VIEW, status: 'idle' };
  return {
    ...effective,
    isThisTarget,
    start: (): void => {
      void controller.start(target);
    },
    stop: (): void => {
      void controller.stop();
    },
  };
}

/** Whether some OTHER target is currently casting (replacing it swaps what the TV shows). */
// eslint-disable-next-line react-refresh/only-export-components -- shared hook alongside its provider
export function useCastBusyWithOther(target: CastTarget): boolean {
  const snapshot = useCastSnapshot();
  return snapshot.status !== 'idle' && snapshot.targetKey !== null && snapshot.targetKey !== target.key;
}
