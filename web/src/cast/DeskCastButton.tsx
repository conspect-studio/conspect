// The desk's monitor-caption Cast button: casts Program (or Preview) from this
// browser. Renders nothing outside a `CastProvider` (isolated desk renders).
import type { JSX } from 'react';
import { useLingui } from '@lingui/react/macro';

import { CastButton } from './CastButton';
import { useHasCastProvider } from './CastProvider';

export function DeskCastButton({ bus }: { readonly bus: 'program' | 'preview' }): JSX.Element | null {
  const { t } = useLingui();
  if (!useHasCastProvider()) return null;
  const name = bus === 'program' ? t`Program` : t`Preview`;
  return <CastButton compact target={{ key: `feed:bus:${bus}`, name, feed: { bus } }} />;
}
