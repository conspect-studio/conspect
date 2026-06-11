// The Multiview management SPA root: providers + router.
//
// Provider order: Query (server state) wraps everything so realtime/REST hooks
// share one cache; Theme + I18n wrap the routed UI; the Toaster mounts once.
// The realtime stream is started inside the routed shell (AppLayout), so it
// lives for the app's lifetime without blocking the initial render.
import type { JSX } from "react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { RouterProvider } from "react-router-dom";

import { router } from "./app/router";
import { Toaster } from "./components/ui/toaster";
import { I18nProvider } from "./i18n/I18nProvider";
import { ThemeProvider } from "./theme/ThemeProvider";

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      // The control plane is best-effort and the WS is the source of truth for
      // live state; do not hammer it with retries or focus refetches.
      retry: 1,
      refetchOnWindowFocus: false,
      staleTime: 30_000,
    },
  },
});

/** The application root. */
export default function App(): JSX.Element {
  return (
    <QueryClientProvider client={queryClient}>
      <ThemeProvider>
        <I18nProvider>
          <RouterProvider router={router} />
          <Toaster />
        </I18nProvider>
      </ThemeProvider>
    </QueryClientProvider>
  );
}
