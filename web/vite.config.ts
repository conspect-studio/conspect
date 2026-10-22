import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import { lingui } from "@lingui/vite-plugin";

// During development the SPA proxies the API; in production it is embedded into
// the `multiview` binary via rust-embed (see docs/web/management-app.md).
//
// Lingui compiles ICU catalogs to plain JS at build time; the macro plugin
// transforms `t`/`<Trans>` macros (see docs/web/internationalization.md). We
// wire the macro through @vitejs/plugin-react's Babel pipeline.
export default defineConfig({
  plugins: [
    react({
      babel: {
        plugins: ["@lingui/babel-plugin-lingui-macro"],
      },
    }),
    tailwindcss(),
    lingui(),
  ],
  server: {
    port: 5173,
    proxy: {
      // IPv6-first (operator directive): proxy the API to the IPv6 loopback of
      // the local `multiview run` daemon (its default control listener is
      // `[::]:8080`). Override the target if your daemon binds elsewhere.
      "/api": "http://[::1]:8080",
    },
  },
  build: {
    outDir: "dist",
    // Raise the JS target to es2022: the SPA is served from localhost and embedded
    // in the multiview daemon, so there is no legacy-browser requirement. es2022 is
    // supported by all browsers released since late 2021 (Chrome 94+, Firefox 93+,
    // Safari 15+). This is required because esbuild ≥ 0.28 cannot downgrade certain
    // modern syntax patterns (e.g. lingui destructuring) to the older default target
    // set ("chrome87", "edge88", "es2020", "firefox78", "safari14").
    target: "es2022",
  },
});
