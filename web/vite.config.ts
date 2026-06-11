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
  },
});
