// Device lifecycle badge (managed-devices.md §2.2): colour + glyph + TEXT,
// with the state in the accessible name — never colour alone (WCAG 1.4.1).
import type { JSX } from 'react';
import { Trans } from '@lingui/react/macro';
import {
  CircleDot,
  CircleSlash,
  KeyRound,
  Radar,
  RefreshCw,
  TriangleAlert,
} from 'lucide-react';

import type { DeviceState } from '../realtime/generated-types';
import { Badge } from '../components/ui/badge';
import type { BadgeProps } from '../components/ui/badge';

/** Props for {@link DeviceStateBadge}. */
export interface DeviceStateBadgeProps {
  /** The device lifecycle state (uppercase wire form). */
  readonly state: DeviceState;
}

interface Presentation {
  readonly variant: NonNullable<BadgeProps['variant']>;
  readonly icon: JSX.Element;
  readonly label: JSX.Element;
}

function present(state: DeviceState): Presentation {
  switch (state) {
    case 'DISCOVERED':
      return {
        variant: 'outline',
        icon: <Radar className="size-3.5" aria-hidden="true" />,
        label: <Trans>Discovered</Trans>,
      };
    case 'ADOPTING':
      return {
        variant: 'reconnecting',
        icon: <RefreshCw className="size-3.5" aria-hidden="true" />,
        label: <Trans>Adopting</Trans>,
      };
    case 'ONLINE':
      return {
        variant: 'live',
        icon: <CircleDot className="size-3.5" aria-hidden="true" />,
        label: <Trans>Online</Trans>,
      };
    case 'DEGRADED':
      return {
        variant: 'stale',
        icon: <TriangleAlert className="size-3.5" aria-hidden="true" />,
        label: <Trans>Degraded</Trans>,
      };
    case 'AUTH_FAILED':
      return {
        variant: 'destructive',
        icon: <KeyRound className="size-3.5" aria-hidden="true" />,
        label: <Trans>Auth failed</Trans>,
      };
    case 'UNREACHABLE':
      return {
        variant: 'offline',
        icon: <CircleSlash className="size-3.5" aria-hidden="true" />,
        label: <Trans>Unreachable</Trans>,
      };
  }
}

/** A device-state pill carrying icon + text. */
export function DeviceStateBadge({ state }: DeviceStateBadgeProps): JSX.Element {
  const { variant, icon, label } = present(state);
  return (
    <Badge variant={variant}>
      {icon}
      <span>{label}</span>
    </Badge>
  );
}
