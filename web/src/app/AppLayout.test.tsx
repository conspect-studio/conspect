// AppLayout tests — the shell contract, unchanged where it must be and new
// where the broadcast shell landed:
// - skip link, focus moved to the main region on route change (SC 2.4.3),
// - desktop rail + mobile drawer (48px touch key, closes on navigate),
// - the account/licence/health/config banner surfaces mount (and stay quiet
//   when clean),
// - the routed Outlet and the desktop-only SystemFooter,
// - the touch-friendly header: ≥48px rows that wrap instead of clipping, a
//   48px drawer key, and the producer line on its own row at touch widths.
// The status truth-separation is pinned hardest: the MANAGEMENT connection
// word renders as its own chip (exactly once — no duplicate status surface),
// while the PRODUCER verdict stays UNKNOWN for every management status,
// because the engine exposes no output-tick feed (a connected management
// plane is never decoded-media-health proof).
import type { JSX } from "react";
import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { screen, waitFor, within } from "@testing-library/react";
import type { RenderResult } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { http, HttpResponse } from "msw";
import { setupServer } from "msw/node";
import { Link, MemoryRouter, Route, Routes } from "react-router-dom";

import { AppLayout } from "./AppLayout";
import { I18nProvider } from "../i18n/I18nProvider";
import { ThemeProvider } from "../theme/ThemeProvider";
import { renderWithProviders } from "../test/render";
import type { EngineEvents } from "../realtime/useEngineEvents";
import type * as engineEventsModule from "../realtime/useEngineEvents";
import type { HealthState, HealthWarning } from "../realtime/useHealth";
import type * as healthModule from "../realtime/useHealth";
import type { SystemMetricsState } from "../realtime/useSystemMetrics";
import type * as systemMetricsModule from "../realtime/useSystemMetrics";
import type {
  EnforcementLevel,
  LicenceResource,
  LicenceStatusDoc,
  PendingAction,
} from "../api/conspect";

// Drive the three realtime hooks off stubs so the shell test never opens a
// WebSocket; the MSW handlers below keep the account-side HTTP real.
const mockEngineEvents = vi.hoisted(() => {
  const value: { current: EngineEvents } = {
    current: { status: "open", lastSeq: 0, gaps: 0 },
  };
  return value;
});
const mockHealth = vi.hoisted(() => {
  const value: { current: HealthState } = {
    current: { warnings: [], status: "open" },
  };
  return value;
});

vi.mock("../realtime/useEngineEvents", async (importOriginal) => {
  const actual = await importOriginal<typeof engineEventsModule>();
  return {
    ...actual,
    useEngineEvents: (): EngineEvents => mockEngineEvents.current,
  };
});

vi.mock("../realtime/useSystemMetrics", async (importOriginal) => {
  const actual = await importOriginal<typeof systemMetricsModule>();
  const emptySeries = {
    cpu: [],
    selfCpu: [],
    gpu0Util: [],
    vram: [],
    nvenc: [],
    dec: [],
    fps: [],
  };
  return {
    ...actual,
    useSystemMetrics: (): SystemMetricsState => ({
      status: "open",
      current: undefined,
      series: emptySeries,
      gpuSeries: [],
    }),
  };
});

vi.mock("../realtime/useHealth", async (importOriginal) => {
  const actual = await importOriginal<typeof healthModule>();
  return { ...actual, useHealth: (): HealthState => mockHealth.current };
});

let licenceBody: LicenceResource = { licensed: false, status: null };
let pending: PendingAction[] = [];

const server = setupServer(
  http.get("*/api/v1/licence", () => HttpResponse.json(licenceBody)),
  http.get("*/api/v1/actions/pending", () => HttpResponse.json(pending)),
);

beforeAll(() => {
  server.listen();
});

afterEach(() => {
  licenceBody = { licensed: false, status: null };
  pending = [];
  server.resetHandlers();
  mockEngineEvents.current = { status: "open", lastSeq: 0, gaps: 0 };
  mockHealth.current = { warnings: [], status: "open" };
});

afterAll(() => {
  server.close();
});

function statusWith(enforcement: EnforcementLevel): LicenceStatusDoc {
  return {
    tier: "studio",
    state: "compliant",
    enforcement,
    hardware_class: { licensed: "standard", detected: "standard" },
    gpu_limit: { kind: "unlimited" },
    gpus_in_use: 0,
    config_locked: enforcement === "config-locked" || enforcement === "watermark",
    watermark: enforcement === "watermark",
    blocks_new_instances: enforcement === "block-new-instance",
    lease: {
      serial: "LS-1",
      source: "online",
      granted_at: "2999-01-01T00:00:00Z",
      expires_at: "2999-02-05T00:00:00Z",
      grace_days: 14,
      grace_until: "2999-02-19T00:00:00Z",
      hard_at: "2999-03-30T00:00:00Z",
      next_contact_due: "2999-02-01T00:00:00Z",
    },
    reasons: ["lease-expiring"],
  };
}

function warning(over: Partial<HealthWarning> = {}): HealthWarning {
  return {
    code: "gpu-present-no-vulkan-adapter",
    severity: "warning",
    subsystem: "compositor",
    message: "GPU RTX 4060 detected but GPU compositing is UNAVAILABLE; fell back to CPU.",
    remediation: "Set NVIDIA_DRIVER_CAPABILITIES to include `graphics` and install libvulkan1.",
    since: 1_700_000_000_000_000_000,
    active: true,
    ...over,
  };
}

function pendingAction(): PendingAction {
  return {
    action_id: "act-9",
    kind: "restart",
    requested_by: "portal:ops",
    requested_at_nanos: 1,
    state: "pending",
    detail: null,
  };
}

/** A probe route body: a heading plus an optional in-page link to click. */
function ProbePage(props: {
  readonly label: string;
  readonly linkTo?: string;
}): JSX.Element {
  return (
    <div>
      <h1>{props.label}</h1>
      {props.linkTo === undefined ? null : <Link to={props.linkTo}>Go next</Link>}
    </div>
  );
}

function renderShell(initial = "/a"): RenderResult {
  return renderWithProviders(
    <ThemeProvider>
      <I18nProvider>
        <MemoryRouter initialEntries={[initial]}>
          <Routes>
            <Route element={<AppLayout />}>
              <Route path="/a" element={<ProbePage label="Page A" linkTo="/b" />} />
              <Route path="/b" element={<ProbePage label="Page B" />} />
              <Route
                path="/outputs"
                element={<ProbePage label="Outputs page" />}
              />
            </Route>
          </Routes>
        </MemoryRouter>
      </I18nProvider>
    </ThemeProvider>,
  );
}

describe("AppLayout — unchanged shell behaviors", () => {
  it("renders the skip link targeting the focusable main region", () => {
    renderShell("/a");
    const skip = screen.getByRole("link", { name: "Skip to content" });
    expect(skip).toHaveAttribute("href", "#main-content");
    expect(skip.className).toMatch(/sr-only/);
    const main = document.getElementById("main-content");
    expect(main).not.toBeNull();
    expect(main).toHaveAttribute("tabindex", "-1");
  });

  it("renders the routed outlet inside the main region and the desktop-only footer", () => {
    renderShell("/a");
    const main = document.getElementById("main-content");
    expect(main).toContainElement(screen.getByRole("heading", { name: "Page A" }));
    const footer = document.querySelector("footer");
    expect(footer).not.toBeNull();
    expect(footer?.className).toMatch(/hidden/);
  });

  it("moves focus to the main region on route change (SC 2.4.3)", async () => {
    const user = userEvent.setup();
    renderShell("/a");
    await user.click(screen.getByRole("link", { name: "Go next" }));
    expect(screen.getByRole("heading", { name: "Page B" })).toBeInTheDocument();
    const main = document.getElementById("main-content");
    expect(main).not.toBeNull();
    expect(document.activeElement).toBe(main);
  });

  it("renders the desktop navigation rail with the primary nav", () => {
    renderShell("/a");
    const nav = screen.getByRole("navigation", { name: "Primary" });
    expect(within(nav).getByText("Multiview")).toBeInTheDocument();
    expect(within(nav).getByRole("link", { name: /outputs/i })).toBeInTheDocument();
  });

  it("opens the mobile drawer from a 48px key and closes it on navigate", async () => {
    const user = userEvent.setup();
    renderShell("/a");
    const trigger = screen.getByRole("button", { name: "Open navigation" });
    // The broadcast 48px touch floor: the key is 48×48, not the 36px icon size.
    expect(trigger.className).toMatch(/size-12/);
    expect(trigger.className).not.toMatch(/size-9/);
    await user.click(trigger);
    const dialog = screen.getByRole("dialog", { name: "Navigation" });
    await user.click(within(dialog).getByRole("link", { name: /outputs/i }));
    await waitFor(() => {
      expect(screen.queryByRole("dialog")).toBeNull();
    });
    expect(
      screen.getByRole("heading", { name: "Outputs page" }),
    ).toBeInTheDocument();
  });

  it("mounts every banner surface and the licence chip with live data", async () => {
    licenceBody = { licensed: true, status: statusWith("config-locked") };
    pending = [pendingAction()];
    mockHealth.current = { warnings: [warning()], status: "open" };
    renderShell("/a");
    // Account-side chrome: the claimed licence chip links to the licence screen.
    const chip = await screen.findByRole("link", { name: /studio/i });
    expect(chip).toHaveAttribute("href", "/settings/licence");
    // Ladder + config-lock banners (account-side enforcement).
    expect(await screen.findByTestId("ladder-banner")).toBeInTheDocument();
    expect(screen.getByTestId("config-lock-banner")).toBeInTheDocument();
    // Pending remote actions strip.
    expect(screen.getByTestId("pending-action-strip")).toBeInTheDocument();
    // Health warning banner (SA-0).
    expect(screen.getByText(/GPU compositing is UNAVAILABLE/)).toBeInTheDocument();
  });

  it("stays quiet when everything is clean (banners render nothing)", async () => {
    renderShell("/a");
    await waitFor(() => {
      expect(screen.queryByTestId("ladder-banner")).toBeNull();
      expect(screen.queryByTestId("config-lock-banner")).toBeNull();
      expect(screen.queryByTestId("pending-action-strip")).toBeNull();
    });
    expect(screen.queryByRole("alert")).toBeNull();
    // The unclaimed licence chip still mounts and links to the account screen.
    expect(
      await screen.findByRole("link", { name: /unclaimed/i }),
    ).toHaveAttribute("href", "/settings/account");
  });
});

describe("AppLayout — touch-friendly responsive header", () => {
  it("keeps every header row at least 48px tall and wraps instead of clipping", () => {
    renderShell("/a");
    const header = screen.getByRole("banner");
    expect(header.className).toMatch(/min-h-14/);
    expect(header.className).toMatch(/flex-wrap/);
    // No fixed header height: a wrapped row must be able to grow, not clip.
    expect(header.className).not.toMatch(/(?:^|\s)h-14(?:\s|$)/);
    // The producer line takes its own full-width row at touch widths and
    // becomes the flexible middle at md+; it can shrink and wrap.
    const producerRow = document
      .querySelector("[data-producer-badge]")
      ?.closest(".basis-full");
    expect(producerRow).not.toBeNull();
    expect(producerRow?.className).toMatch(/order-last/);
    expect(producerRow?.className).toMatch(/min-w-0/);
    expect(producerRow?.className).toMatch(/md:flex-1/);
  });

  it("keeps the locale and theme controls mounted in the header cluster", () => {
    renderShell("/a");
    expect(screen.getByRole("combobox", { name: "Language" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /theme/i })).toBeInTheDocument();
  });
});

describe("AppLayout — producer vs management truth separation", () => {
  it("shows the management chip with the live connection word while the producer stays UNKNOWN", () => {
    renderShell("/a");
    expect(screen.getByText(/Management · Connected/)).toBeInTheDocument();
    expect(screen.getByText("Unknown")).toBeInTheDocument();
    expect(screen.getByText("No output tick observed")).toBeInTheDocument();
    expect(screen.queryByText("Producing")).toBeNull();
  });

  it("never infers producer health from the management plane — every status word keeps the producer UNKNOWN", () => {
    const words: readonly (readonly [EngineEvents["status"], string])[] = [
      ["open", "Connected"],
      ["connecting", "Connecting"],
      ["reconnecting", "Reconnecting"],
      ["closed", "Disconnected"],
    ];
    for (const [status, word] of words) {
      mockEngineEvents.current = { status, lastSeq: 0, gaps: 0 };
      const view = renderShell("/a");
      expect(
        screen.getByText(new RegExp(`Management · ${word}`)),
      ).toBeInTheDocument();
      expect(screen.getByText("Unknown")).toBeInTheDocument();
      expect(screen.queryByText("Producing")).toBeNull();
      // Exactly one surface carries the management word — no duplicate status.
      expect(screen.getAllByText(new RegExp(word))).toHaveLength(1);
      view.unmount();
    }
  });
});
