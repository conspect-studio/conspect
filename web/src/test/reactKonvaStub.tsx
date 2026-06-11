// A jsdom-friendly stub for `react-konva`, used ONLY under Vitest.
//
// react-konva binds to konva's Node entry, which needs the native `canvas`
// package — irrelevant to our tests, which exercise the ACCESSIBLE non-canvas
// path and the editor's pure logic. This stub renders nothing (the real canvas
// is covered by the running app + Storybook/visual checks, not jsdom). Aliased
// in `vitest.config.ts`; it never ships in the production bundle.
import type { JSX, ReactNode } from 'react';

function Noop({ children }: { readonly children?: ReactNode }): JSX.Element {
  return <>{children}</>;
}

export const Stage = Noop;
export const Layer = Noop;
export const Group = Noop;
export const Rect = Noop;
export const Transformer = Noop;
