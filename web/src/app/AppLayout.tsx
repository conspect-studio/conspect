// The application shell: skip link, sidebar (desktop rail + mobile drawer),
// a touch-friendly responsive header, the banner stack, and the routed Outlet.
// Focus is moved to the main heading on route change (SC 2.4.3).
//
// Header status (production-health-contract.md): ONE status line — the
// broadcast ProducerStatus — whose management chip carries the realtime
// connection word (the exact strings ConnectionStatus shows) while the
// PRODUCER verdict stays UNKNOWN: the engine exposes no output-tick feed,
// so a connected management plane is never decoded-media-health proof. The
// old standalone ConnectionStatus chip was folded into that line to avoid a
// duplicate status surface; the words, text+glyph pairing and polite live
// region all remain (MonitoringPage/SystemPage still render the chip).
import { useEffect, useRef, useState } from "react";
import type { JSX } from "react";
import { Trans, useLingui } from "@lingui/react/macro";
import { Menu } from "lucide-react";
import { Outlet, useLocation } from "react-router-dom";

import {
  ConfigLockBanner,
  LadderBanner,
  LicenceChip,
  PendingActionStrip,
} from "../components/account/chrome";
import { ProducerStatus } from "../components/broadcast";
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

  // The management plane's own connection word — exactly the strings the
  // ConnectionStatus badge maps — passed to the producer line as its own
  // SEPARATE chip. It never feeds the producer verdict.
  const managementLabel =
    status === "open"
      ? t`Connected`
      : status === "connecting"
        ? t`Connecting`
        : status === "reconnecting"
          ? t`Reconnecting`
          : t`Disconnected`;

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
          {/* Touch-friendly, responsive header. `min-h-14` keeps every row at
              least 48px while `flex-wrap` lets the producer line take its own
              full-width row at 320–390px instead of clipping; the drawer key
              keeps the broadcast 48px touch floor (`size-12`). */}
          <header className="flex min-h-14 flex-wrap items-center gap-x-2 gap-y-1 border-b bg-card px-4">
            {/* Mobile drawer trigger. */}
            <div className="md:hidden">
              <Dialog open={drawerOpen} onOpenChange={setDrawerOpen}>
                <DialogTrigger asChild>
                  <Button
                    variant="ghost"
                    size="icon"
                    aria-label={t`Open navigation`}
                    className="size-12"
                  >
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

            {/* Header controls cluster. On touch widths it stays on the first
                row (right-aligned); at md+ it sits at the header's end. */}
            <div className="ms-auto flex min-w-0 max-w-full flex-wrap items-center justify-end gap-2 md:order-2">
              {/* Account-side licence chip: links to claim (unclaimed) or the
                  licence screen (claimed); colour paired with text. */}
              <LicenceChip />
              <LocaleSwitcher />
              <ThemeToggle />
            </div>

            {/* The one status line: producer verdict + management chip. No
                output-tick feed exists yet, so the producer input is honestly
                UNKNOWN — no fabricated tick; the line wraps (min-w-0 +
                break-words inside) and never clips at 320/390. */}
            <div className="order-last min-w-0 basis-full md:order-1 md:basis-auto md:flex-1">
              <ProducerStatus
                state="unknown"
                lastTickAt={null}
                managementLabel={managementLabel}
              />
            </div>
          </header>

          {/* Global health-warning banner (SA-0): renders nothing when clean,
              an actionable callout (severity + message + remediation) when a
              capability mismatch is active (e.g. GPU present but CPU compositing). */}
          <HealthBanner />

          {/* Account-side enforcement-ladder banner: quiet when the licence is
              active, an actionable callout (the reason + a link to the licence
              screen) at warning-or-worse. The tile watermark is an engine
              concern; the SPA only explains it. */}
          <LadderBanner />

          {/* Account-side pending-action strip: surfaces remote actions awaiting
              local review; renders nothing when the queue is empty. */}
          <PendingActionStrip />

          {/* Config-lock interceptor: while enforcement locks reconfiguration,
              this explains why and links to the licence screen. The API also
              returns 409 config_locked, so the lock holds if the UI is bypassed. */}
          <ConfigLockBanner />

          <main
            id="main-content"
            ref={mainRef}
            tabIndex={-1}
            className="min-w-0 flex-1 scroll-mt-16 p-4 focus-visible:outline-none md:p-6"
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
