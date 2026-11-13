// App-level provider for the browser-cast controller: one `CastController`
// instance survives page navigation, matching the Cast SDK's own
// one-session-per-tab model. Mount once near the app root (see `App.tsx`).
import { createContext, useContext, useEffect, useState } from 'react';
import type { JSX, ReactNode } from 'react';
import { useSyncExternalStore } from 'react';

import { CastController } from './castController';
import type { CastSnapshot, CastTarget } from './castController';

const CastControllerContext = createContext<CastController | null>(null);

/** Mount once near the app root; `useCast` reads the shared controller it creates. */
export function CastProvider({ children }: { readonly children: ReactNode }): JSX.Element {
  // Lazy `useState` initializer (not a ref read during render): exactly one
  // controller for the lifetime of this provider, surviving route changes.
  const [controller] = useState<CastController>(() => new CastController());
  useEffect(() => {
    return (): void => {
      controller.dispose();
    };
  }, [controller]);
  return (
    <CastControllerContext.Provider value={controller}>
      {children}
    </CastControllerContext.Provider>
  );
}

function useCastController(): CastController {
  const controller = useContext(CastControllerContext);
  if (controller === null) {
    throw new Error('useCast must be used within a CastProvider.');
  }
  return controller;
}

export interface UseCastResult extends CastSnapshot {
  /** True when this hook's own target is the one currently casting/loading. */
  readonly isThisTarget: boolean;
  readonly start: () => void;
  readonly stop: () => void;
}

/** Hooks a component's Cast button into the shared one-per-tab controller. */
// eslint-disable-next-line react-refresh/only-export-components -- shared hook alongside its provider
export function useCast(target: CastTarget): UseCastResult {
  const controller = useCastController();
  const snapshot = useSyncExternalStore(controller.subscribe, controller.getSnapshot);

  const isThisTarget = snapshot.targetKey !== null && snapshot.targetKey === target.key;

  // A different target's button must not show this target's casting/error
  // state — only the currently-active target (or none) owns the shared
  // controller's snapshot.
  const effective: CastSnapshot = isThisTarget
    ? snapshot
    : { status: 'idle', targetKey: null, targetName: null, deviceName: null, errorText: null };

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

/** Whether some OTHER target is currently casting/loading/connecting (replacing it would swap media). */
// eslint-disable-next-line react-refresh/only-export-components -- shared hook alongside its provider
export function useCastBusyWithOther(target: CastTarget): boolean {
  const controller = useCastController();
  const snapshot = useSyncExternalStore(controller.subscribe, controller.getSnapshot);
  return snapshot.targetKey !== null && snapshot.targetKey !== target.key;
}
