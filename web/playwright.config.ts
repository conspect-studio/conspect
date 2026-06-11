import { defineConfig, devices } from "@playwright/test";

// Playwright e2e config. These specs drive the REAL built SPA in a real browser,
// because the failure class they guard (e.g. the ResourceTable re-render-loop
// renderer OOM) only reproduces with real browser effects (Radix Dialog focus /
// scroll-lock + ResizeObserver) that jsdom does not implement. The webServer
// builds the app and serves the static `dist` over `vite preview`; the API is
// mocked per-test with `page.route`, so no backend is required.
export default defineConfig({
  testDir: "./e2e",
  timeout: 30_000,
  fullyParallel: false,
  forbidOnly: !!process.env.CI,
  retries: 0,
  reporter: [["list"]],
  use: {
    baseURL: "http://localhost:4173",
    // Containers/CI: chromium needs the sandbox disabled.
    launchOptions: {
      args: ["--no-sandbox", "--disable-setuid-sandbox", "--disable-dev-shm-usage"],
    },
  },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
  webServer: {
    command: "npm run build && npx vite preview --port 4173 --strictPort",
    url: "http://localhost:4173",
    reuseExistingServer: !process.env.CI,
    timeout: 180_000,
  },
});
