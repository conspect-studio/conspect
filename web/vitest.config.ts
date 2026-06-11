// Vitest config for the management SPA component + logic tests.
//
// Tests run in jsdom (DOM + Testing Library), with a single setup file wiring
// jest-dom matchers and MSW lifecycle. Reuses the app's Vite plugin pipeline so
// the Lingui macros (`t`/`<Trans>`) compile in tests exactly as in the build.
import { fileURLToPath } from 'node:url';
import { defineConfig } from 'vitest/config';
import react from '@vitejs/plugin-react';

export default defineConfig({
  plugins: [
    react({
      babel: {
        plugins: ['@lingui/babel-plugin-lingui-macro'],
      },
    }),
  ],
  resolve: {
    alias: {
      // react-konva binds to konva's native-`canvas` Node entry, which jsdom
      // cannot load. Tests exercise the accessible non-canvas path; stub the
      // konva renderer so importing the editor does not pull in `canvas`.
      'react-konva': fileURLToPath(
        new URL('./src/test/reactKonvaStub.tsx', import.meta.url),
      ),
    },
  },
  test: {
    environment: 'jsdom',
    globals: true,
    setupFiles: ['./src/test/setup.ts'],
    include: ['src/**/*.{test,spec}.{ts,tsx}'],
    css: false,
  },
});
