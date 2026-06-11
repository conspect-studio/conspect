// The application shell: skip link, sidebar (desktop rail + mobile drawer),
// header with connection status + theme/locale controls, and the routed Outlet.
// Focus is moved to the main heading on route change (SC 2.4.3).
import { useEffect, useRef, useState } from "react";
import type { JSX } from "react";
import { Trans, useLingui } from "@lingui/react/macro";
import { Menu } from "lucide-react";
import { Outlet, useLocation } from "react-router-dom";

import { ConnectionStatus } from "../components/ConnectionStatus";
import { HealthBanner } from "../components/HealthBanner";
import { LocaleSwitcher } from "../components/LocaleSwitcher";
import { SystemFooter } from "../components/SystemFooter";
import { ThemeToggle } from "../components/ThemeToggle";
import { Button } from "../components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogTitle,
  DialogTrigger,
} from "../components/ui/dialog";
import { useEngineEvents } from "../realtime/useEngineEvents";
import { Sidebar } from "./Sidebar";

/** The persistent app shell wrapping every route. */
export function AppLayout(): JSX.Element {
  const { t } = useLingui();
  const { status } = useEngineEvents();
  const location = useLocation();
  const [drawerOpen, setDrawerOpen] = useState(false);
  const mainRef = useRef<HTMLElement>(null);

  // Move focus to the main region on navigation so keyboard/SR users land on
  // the new view (SC 2.4.3). The route's <h1> is the first focusable target.
  useEffect(() => {
    mainRef.current?.focus();
  }, [location.pathname]);

  return (
    <div className="min-h-dvh bg-background text-foreground">
      <a
        href="#main-content"
        className="sr-only focus:not-sr-only focus:absolute focus:start-2 focus:top-2 focus:z-50 focus:rounded-md focus:bg-primary focus:px-3 focus:py-2 focus:text-primary-foreground"
      >
        <Trans>Skip to content</Trans>
      </a>

      <div className="flex min-h-dvh">
        {/* Desktop navigation rail. */}
        <aside className="hidden w-60 shrink-0 border-e bg-card md:block">
          <div className="sticky top-0">
            <Sidebar />
          </div>
        </aside>

        <div className="flex min-w-0 flex-1 flex-col">
          <header className="flex h-14 items-center gap-2 border-b bg-card px-4">
            {/* Mobile drawer trigger. */}
            <div className="md:hidden">
              <Dialog open={drawerOpen} onOpenChange={setDrawerOpen}>
                <DialogTrigger asChild>
                  <Button variant="ghost" size="icon" aria-label={t`Open navigation`}>
                    <Menu aria-hidden="true" />
                  </Button>
                </DialogTrigger>
                <DialogContent className="start-2 top-2 h-[calc(100dvh-1rem)] max-w-xs translate-x-0 translate-y-0 p-0 rtl:translate-x-0">
                  <DialogTitle className="sr-only">
                    <Trans>Navigation</Trans>
                  </DialogTitle>
                  <Sidebar
                    onNavigate={(): void => {
                      setDrawerOpen(false);
                    }}
                  />
                </DialogContent>
              </Dialog>
            </div>

            <div className="ms-auto flex items-center gap-2">
              <ConnectionStatus status={status} />
              <LocaleSwitcher />
              <ThemeToggle />
            </div>
          </header>

          {/* Global health-warning banner (SA-0): renders nothing when clean,
              an actionable callout (severity + message + remediation) when a
              capability mismatch is active (e.g. GPU present but CPU compositing). */}
          <HealthBanner />

          <main
            id="main-content"
            ref={mainRef}
            tabIndex={-1}
            className="flex-1 scroll-mt-16 p-4 focus-visible:outline-none md:p-6"
          >
            <Outlet />
          </main>

          {/* The live system-metrics status bar (desktop-only). */}
          <SystemFooter />
        </div>
      </div>
    </div>
  );
}
