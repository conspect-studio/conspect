// Capture full-page screenshots of every Multiview management UI page against a
// running `multiview run --features web` server.
//
// Usage:
//   BASE=http://[::1]:8099 TOKEN=admin.<secret> OUT=../demo-output/screens \
//     node scripts/screenshots.mjs
//
// The token is seeded into localStorage before any page script runs, so every
// page authenticates its API calls (and the live-preview fetches succeed).
import { chromium } from "@playwright/test";
import { mkdirSync } from "node:fs";

// IPv6-first (operator directive): default to the IPv6 loopback of a local
// `multiview run` daemon. Override BASE for a different host.
const BASE = process.env.BASE ?? "http://[::1]:8099";
const TOKEN = process.env.TOKEN ?? "";
const OUT = process.env.OUT ?? "../demo-output/screens";

// [filename, route, optional settle-ms for live content like previews]
const PAGES = [
  ["dashboard", "/", 800],
  ["layouts", "/layouts", 600],
  ["layout-editor", "/layouts/new", 600],
  ["sources", "/sources", 600],
  ["outputs", "/outputs", 600],
  ["overlays", "/overlays", 600],
  ["monitoring", "/monitoring", 2500],
  ["tally", "/tally", 800],
  ["salvos", "/salvos", 800],
  ["alarms", "/alarms", 800],
  ["audit", "/audit", 800],
  ["settings", "/settings", 400],
  ["help-overview", "/help", 400],
  ["help-containers", "/help/containers", 400],
  ["help-compose", "/help/compose", 400],
  ["help-config", "/help/config", 400],
  ["help-api", "/help/api", 400],
  ["help-features", "/help/features", 400],
  ["help-concepts-transports", "/help/concepts/transports", 600],
  ["help-concepts-timing", "/help/concepts/timing-sync", 600],
  ["help-concepts-codecs", "/help/concepts/codecs", 600],
  ["help-concepts-color", "/help/concepts/color", 600],
  ["help-concepts-resilience", "/help/concepts/resilience", 600],
  ["help-concepts-latency", "/help/concepts/latency", 600],
  ["help-concepts-glossary", "/help/concepts/glossary", 600],
];

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function main() {
  mkdirSync(OUT, { recursive: true });
  const browser = await chromium.launch();
  const context = await browser.newContext({
    viewport: { width: 1440, height: 900 },
    deviceScaleFactor: 2,
  });
  // Seed the bearer token before any app script runs.
  await context.addInitScript((token) => {
    try {
      window.localStorage.setItem("multiview.apiToken", token);
    } catch {
      /* ignore */
    }
  }, TOKEN);

  const page = await context.newPage();
  const results = [];
  for (const [name, route, settle] of PAGES) {
    const url = `${BASE}${route}`;
    try {
      const resp = await page.goto(url, { waitUntil: "networkidle", timeout: 15000 });
      await sleep(settle ?? 500);
      await page.screenshot({ path: `${OUT}/${name}.png`, fullPage: true });
      results.push(`OK   ${name.padEnd(18)} ${route} (${resp?.status() ?? "?"})`);
    } catch (err) {
      results.push(`FAIL ${name.padEnd(18)} ${route} — ${String(err).split("\n")[0]}`);
    }
  }
  await browser.close();
  console.log(results.join("\n"));
}

main().catch((err) => {
  console.error(err);
  process.exitCode = 1;
});
