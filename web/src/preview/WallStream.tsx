// One shared wall stream for the production desk (issue #250): React bindings.
import { createContext, useContext, useEffect, useRef, useState, useSyncExternalStore, type JSX, type ReactNode } from 'react';
import { WallStore, type WallDeps, type WallSnapshot } from './wallStore';
import { cropRect, findCell, type WallCell } from './wallApi';

const WallContext = createContext<WallStore | undefined>(undefined);

/** Mount once on the production desk; owns the single wall lease while mounted. */
export function WallStreamProvider({ children, deps }: { readonly children: ReactNode; readonly deps?: WallDeps | undefined }): JSX.Element {
  const [store] = useState(() => new WallStore(deps));
  useEffect(() => {
    store.start();
    return () => { store.stop(); };
  }, [store]);
  return <WallContext.Provider value={store}>{children}</WallContext.Provider>;
}

const NONE: WallSnapshot = { geometry: undefined };
const noopSubscribe = (): (() => void) => () => undefined;

export interface WallSurface {
  readonly live: boolean;
  readonly cell: WallCell | undefined;
  /** The cell source the engine currently shows on the wall's preview bus. */
  readonly previewSource: string | null;
}

/** Whole-wall state for the desk note: live, or the reason surfaces show stills. */
// eslint-disable-next-line react-refresh/only-export-components -- hook colocated with its context
export function useWallStatus(): { readonly live: boolean; readonly reason: string | undefined } {
  const store = useContext(WallContext);
  const snapshot = useSyncExternalStore(store?.subscribeState ?? noopSubscribe, store?.getSnapshot ?? (() => NONE));
  return { live: snapshot.geometry !== undefined, reason: snapshot.reason };
}

/** Live state of one surface: true only when the wall is streaming and the cell exists. */
// eslint-disable-next-line react-refresh/only-export-components -- hook colocated with its context
export function useWallSurface(source: string | null | undefined): WallSurface {
  const store = useContext(WallContext);
  const snapshot = useSyncExternalStore(store?.subscribeState ?? noopSubscribe, store?.getSnapshot ?? (() => NONE));
  const cell = findCell(snapshot.geometry, source);
  return { live: cell !== undefined, cell, previewSource: snapshot.geometry?.preview_source ?? null };
}

/** Draws one wall cell onto a canvas on every new frame. Render only while `useWallSurface().live`. */
export function WallCrop({ source, label, className, testId }: {
  readonly source: string; readonly label: string; readonly className?: string | undefined; readonly testId?: string | undefined;
}): JSX.Element {
  const store = useContext(WallContext);
  const { cell } = useWallSurface(source);
  const canvas = useRef<HTMLCanvasElement>(null);
  const x = cell?.x, y = cell?.y, w = cell?.w, h = cell?.h;
  useEffect(() => {
    const target = canvas.current;
    if (!store || !target || x === undefined || y === undefined || w === undefined || h === undefined) return;
    const ctx = target.getContext('2d');
    if (!ctx) return;
    // `picture` is the store's canvas, already converted from the decoded frame:
    // a crop from it is a small copy, not another whole-frame conversion.
    return store.subscribeFrames((picture) => {
      const rect = cropRect({ source, x, y, w, h }, picture.width, picture.height);
      if (rect) ctx.drawImage(picture, rect[0], rect[1], rect[2], rect[3], 0, 0, target.width, target.height);
    });
  }, [store, source, x, y, w, h]);
  return <canvas ref={canvas} width={w ?? 0} height={h ?? 0} role="img" aria-label={label} className={className} data-wall-live="1" data-testid={testId ?? 'wall-crop'} />;
}
