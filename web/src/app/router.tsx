// The client-side route table (react-router v7). The shell (AppLayout) is the
// layout route; screens render into its <Outlet>.
import { createBrowserRouter } from "react-router-dom";

import { AppLayout } from "./AppLayout";
import { RequireAuth } from "../auth/RequireAuth";
import { DashboardPage } from "../pages/DashboardPage";
import { LayoutsPage } from "../pages/LayoutsPage";
import { LayoutEditorPage } from "../pages/LayoutEditorPage";
import { NotFoundPage } from "../pages/NotFoundPage";
import { MonitoringPage } from "../pages/MonitoringPage";
import { TallyPage } from "../pages/TallyPage";
import { SalvosPage } from "../pages/SalvosPage";
import { AlarmsPage } from "../pages/AlarmsPage";
import { AudioPage } from "../pages/AudioPage";
import { ProbesPage } from "../pages/ProbesPage";
import { AuditPage } from "../pages/AuditPage";
import { SystemPage } from "../pages/SystemPage";
import { SettingsPage } from "../pages/SettingsPage";
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
      { path: "probes", element: <ProbesPage /> },
      { path: "monitoring", element: <MonitoringPage /> },
      { path: "tally", element: <TallyPage /> },
      { path: "salvos", element: <SalvosPage /> },
      { path: "alarms", element: <AlarmsPage /> },
      { path: "system", element: <SystemPage /> },
      { path: "audit", element: <AuditPage /> },
      { path: "settings", element: <SettingsPage /> },
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
