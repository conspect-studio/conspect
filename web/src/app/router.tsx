// The client-side route table (react-router v7). The shell (AppLayout) is the
// layout route; screens render into its <Outlet>.
import { createBrowserRouter } from "react-router-dom";

import { AppLayout } from "./AppLayout";
import { RequireAuth } from "../auth/RequireAuth";
import { DashboardPage } from "../pages/DashboardPage";
import { LayoutsPage } from "../pages/LayoutsPage";
import { LayoutEditorPage } from "../pages/LayoutEditorPage";
import { CastPage } from "../pages/CastPage";
import { RoutingPage } from "../pages/RoutingPage";
import { LogsPage } from "../pages/LogsPage";
import { NotFoundPage } from "../pages/NotFoundPage";
import { MonitoringPage } from "../pages/MonitoringPage";
import { TallyPage } from "../pages/TallyPage";
import { SalvosPage } from "../pages/SalvosPage";
import { MediaPlayersPage } from "../pages/MediaPlayersPage";
import { AlarmsPage } from "../pages/AlarmsPage";
import { AudioPage } from "../pages/AudioPage";
import { DeviceDetailPage } from "../pages/DeviceDetailPage";
import { DevicesPage } from "../pages/DevicesPage";
import { ProbesPage } from "../pages/ProbesPage";
import { SyncGroupsPage } from "../pages/SyncGroupsPage";
import { AuditPage } from "../pages/AuditPage";
import { SystemPage } from "../pages/SystemPage";
import { CapabilitiesPage } from "../pages/CapabilitiesPage";
import { SettingsPage } from "../pages/SettingsPage";
import { WelcomePage } from "../pages/WelcomePage";
import { LicencePage } from "../pages/LicencePage";
import { DataPage } from "../pages/DataPage";
import { MeshPage } from "../pages/MeshPage";
import { AccountPage } from "../pages/AccountPage";
import { SystemActionsPage } from "../pages/SystemActionsPage";
import { SupportPage } from "../pages/SupportPage";
import {
  OutputsPage,
  OverlaysPage,
  SourcesPage,
} from "../pages/SimplePages";
import { ApiPage } from "../pages/docs/ApiPage";
import { ComposePage } from "../pages/docs/ComposePage";
import { ConfigPage } from "../pages/docs/ConfigPage";
import { ContainerPage } from "../pages/docs/ContainerPage";
import { DocsLayout } from "../pages/docs/DocsLayout";
import { FeaturesPage } from "../pages/docs/FeaturesPage";
import { OverviewPage } from "../pages/docs/OverviewPage";

/** The application router. */
export const router = createBrowserRouter([
  {
    path: "/",
    element: (
      <RequireAuth>
        <AppLayout />
      </RequireAuth>
    ),
    children: [
      { index: true, element: <DashboardPage /> },
      { path: "layouts", element: <LayoutsPage /> },
      { path: "layouts/new", element: <LayoutEditorPage /> },
      { path: "layouts/:id", element: <LayoutEditorPage /> },
      { path: "sources", element: <SourcesPage /> },
      { path: "outputs", element: <OutputsPage /> },
      { path: "overlays", element: <OverlaysPage /> },
      { path: "audio", element: <AudioPage /> },
      { path: "devices", element: <DevicesPage /> },
      { path: "devices/:id", element: <DeviceDetailPage /> },
      { path: "cast", element: <CastPage /> },
      { path: "routing", element: <RoutingPage /> },
      { path: "sync-groups", element: <SyncGroupsPage /> },
      { path: "probes", element: <ProbesPage /> },
      { path: "monitoring", element: <MonitoringPage /> },
      { path: "tally", element: <TallyPage /> },
      { path: "salvos", element: <SalvosPage /> },
      { path: "media-players", element: <MediaPlayersPage /> },
      { path: "alarms", element: <AlarmsPage /> },
      { path: "system", element: <SystemPage /> },
      { path: "system/capabilities", element: <CapabilitiesPage /> },
      { path: "system/actions", element: <SystemActionsPage /> },
      { path: "logs", element: <LogsPage /> },
      { path: "audit", element: <AuditPage /> },
      { path: "settings", element: <SettingsPage /> },
      // Account-side (Conspect) settings screens.
      { path: "welcome", element: <WelcomePage /> },
      { path: "settings/licence", element: <LicencePage /> },
      { path: "settings/data", element: <DataPage /> },
      { path: "settings/mesh", element: <MeshPage /> },
      { path: "settings/account", element: <AccountPage /> },
      // The account-side support surface. It lives at /help/support but is a
      // SIBLING of the /help docs layout — it renders in the plain app chrome
      // (its own PageHeader), not the docs ToC/search/breadcrumb shell, and it
      // is deliberately NOT a docs-registry page (the registry indexes concept
      // articles only). Keeping it out of the /help layout's children also
      // preserves the docs registry ↔ router contract (registry.test.ts).
      { path: "help/support", element: <SupportPage /> },
      // In-app documentation under /help. (/docs is the backend Scalar API
      // playground, so the SPA guide deliberately avoids that path.)
      //
      // Concept articles are route-level lazy chunks (router `lazy()`), so
      // the management UI bundle does not carry the concept library.
      {
        path: "help",
        element: <DocsLayout />,
        children: [
          { index: true, element: <OverviewPage /> },
          { path: "containers", element: <ContainerPage /> },
          { path: "compose", element: <ComposePage /> },
          { path: "config", element: <ConfigPage /> },
          { path: "api", element: <ApiPage /> },
          { path: "features", element: <FeaturesPage /> },
          {
            path: "devices",
            lazy: async () => ({
              Component: (await import("../pages/docs/DevicesHelpPage"))
                .DevicesHelpPage,
            }),
          },
          {
            path: "devices/adopt",
            lazy: async () => ({
              Component: (await import("../pages/docs/DevicesAdoptHelpPage"))
                .DevicesAdoptHelpPage,
            }),
          },
          {
            path: "display-nodes",
            lazy: async () => ({
              Component: (await import("../pages/docs/DisplayNodesHelpPage"))
                .DisplayNodesHelpPage,
            }),
          },
          {
            path: "sync",
            lazy: async () => ({
              Component: (await import("../pages/docs/SyncHelpPage"))
                .SyncHelpPage,
            }),
          },
          {
            path: "concepts/transports",
            lazy: async () => ({
              Component: (await import("../pages/docs/concepts/TransportsPage"))
                .TransportsPage,
            }),
          },
          {
            path: "concepts/timing-sync",
            lazy: async () => ({
              Component: (await import("../pages/docs/concepts/TimingSyncPage"))
                .TimingSyncPage,
            }),
          },
          {
            path: "concepts/codecs",
            lazy: async () => ({
              Component: (await import("../pages/docs/concepts/CodecsPage"))
                .CodecsPage,
            }),
          },
          {
            path: "concepts/color",
            lazy: async () => ({
              Component: (await import("../pages/docs/concepts/ColorPage"))
                .ColorPage,
            }),
          },
          {
            path: "concepts/resilience",
            lazy: async () => ({
              Component: (await import("../pages/docs/concepts/ResiliencePage"))
                .ResiliencePage,
            }),
          },
          {
            path: "concepts/latency",
            lazy: async () => ({
              Component: (await import("../pages/docs/concepts/LatencyPage"))
                .LatencyPage,
            }),
          },
          {
            path: "concepts/glossary",
            lazy: async () => ({
              Component: (await import("../pages/docs/concepts/GlossaryPage"))
                .GlossaryPage,
            }),
          },
        ],
      },
      { path: "*", element: <NotFoundPage /> },
    ],
  },
]);
