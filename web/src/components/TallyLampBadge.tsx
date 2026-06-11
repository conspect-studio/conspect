// Tally lamp badge: the lamp colour as text + a glyph, never colour alone
// (WCAG 1.4.1). The TSL UMD palette is Off / Red / Green / Amber; the badge hue
// is a redundant cue and the colour NAME is always shown.
import type { JSX } from 'react';
import { Trans } from '@lingui/react/macro';
import { Circle, CircleDot } from 'lucide-react';

import type { TallyColor } from '../api/tallyQueries';
import { Badge } from './ui/badge';
import type { BadgeProps } from './ui/badge';

/** Props for {@link TallyLampBadge}. */
export interface TallyLampBadgeProps {
  /** The resolved lamp colour. */
  readonly color: TallyColor;
}

interface LampPresentation {
  readonly variant: NonNullable<BadgeProps['variant']>;
  readonly icon: JSX.Element;
  readonly label: JSX.Element;
}

function present(color: TallyColor): LampPresentation {
  switch (color) {
    case 'Red':
      return {
        variant: 'destructive',
        icon: <CircleDot className="size-3.5" aria-hidden="true" />,
        label: <Trans>Red</Trans>,
      };
    case 'Green':
      return {
        variant: 'live',
        icon: <CircleDot className="size-3.5" aria-hidden="true" />,
        label: <Trans>Green</Trans>,
      };
    case 'Amber':
      return {
        variant: 'stale',
        icon: <CircleDot className="size-3.5" aria-hidden="true" />,
        label: <Trans>Amber</Trans>,
      };
    case 'Off':
      return {
        variant: 'outline',
        icon: <Circle className="size-3.5" aria-hidden="true" />,
        label: <Trans>Off</Trans>,
      };
  }
}

/** A tally-lamp pill carrying icon + colour name. */
export function TallyLampBadge({ color }: TallyLampBadgeProps): JSX.Element {
  const { variant, icon, label } = present(color);
  return (
    <Badge variant={variant}>
      {icon}
      <span>{label}</span>
    </Badge>
  );
}
