// The shell's navigation model. Labels are <Trans> elements (externalized);
// the route paths are stable, non-localized identifiers.
import type { JSX } from "react";
import { Trans } from "@lingui/react/macro";
import {
  Activity,
  BookOpen,
  Cast,
  Cpu,
  HardDrive,
  LayoutDashboard,
  LayoutGrid,
  Radio,
  Route,
  Send,
  Layers,
  Logs,
  Clapperboard,
  MonitorPlay,
  CircleDot,
  Zap,
  ShieldAlert,
  Bell,
  ScrollText,
  Settings,
  Volume2,
  KeyRound,
  Database,
  Network,
  UserCircle,
  ListChecks,
  LifeBuoy,
} from "lucide-react";
import type { LucideIcon } from "lucide-react";

/** A single primary navigation entry. */
export interface NavItem {
  /** Stable route path (also used as a React key). */
  readonly path: string;
  /** Localized label element. */
  readonly label: JSX.Element;
  /** Leading icon (decorative; the label carries meaning). */
  readonly Icon: LucideIcon;
}

/** The primary navigation, in display order. */
export const NAV_ITEMS: readonly NavItem[] = [
  { path: "/", label: <Trans>Dashboard</Trans>, Icon: LayoutDashboard },
  { path: "/layouts", label: <Trans>Layouts</Trans>, Icon: LayoutGrid },
  { path: "/sources", label: <Trans>Sources</Trans>, Icon: Radio },
  { path: "/outputs", label: <Trans>Outputs</Trans>, Icon: Send },
  { path: "/overlays", label: <Trans>Overlays</Trans>, Icon: Layers },
  { path: "/audio", label: <Trans>Audio</Trans>, Icon: Volume2 },
  // Devices sits between Outputs and Monitoring (managed-devices.md §9).
  { path: "/devices", label: <Trans>Devices</Trans>, Icon: HardDrive },
  { path: "/cast", label: <Trans>Cast</Trans>, Icon: Cast },
  { path: "/routing", label: <Trans>Routing</Trans>, Icon: Route },
  { path: "/monitoring", label: <Trans>Monitoring</Trans>, Icon: MonitorPlay },
  { path: "/tally", label: <Trans>Tally</Trans>, Icon: CircleDot },
  { path: "/salvos", label: <Trans>Salvos</Trans>, Icon: Zap },
  { path: "/media-players", label: <Trans>Media players</Trans>, Icon: Clapperboard },
  { path: "/probes", label: <Trans>Probes</Trans>, Icon: ShieldAlert },
  { path: "/alarms", label: <Trans>Alarms</Trans>, Icon: Bell },
  { path: "/system", label: <Trans>System</Trans>, Icon: Activity },
  { path: "/system/capabilities", label: <Trans>Capabilities</Trans>, Icon: Cpu },
  { path: "/system/actions", label: <Trans>System actions</Trans>, Icon: ListChecks },
  { path: "/logs", label: <Trans>Logs</Trans>, Icon: Logs },
  { path: "/audit", label: <Trans>Audit</Trans>, Icon: ScrollText },
  { path: "/help", label: <Trans>Docs</Trans>, Icon: BookOpen },
  { path: "/settings", label: <Trans>Settings</Trans>, Icon: Settings },
  // Account-side (Conspect) screens.
  { path: "/settings/licence", label: <Trans>Licence</Trans>, Icon: KeyRound },
  { path: "/settings/account", label: <Trans>Account</Trans>, Icon: UserCircle },
  { path: "/settings/data", label: <Trans>Data</Trans>, Icon: Database },
  { path: "/settings/mesh", label: <Trans>Mesh</Trans>, Icon: Network },
  { path: "/help/support", label: <Trans>Support</Trans>, Icon: LifeBuoy },
];
