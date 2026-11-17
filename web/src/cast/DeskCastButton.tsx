// The desk's monitor-caption Cast button: casts Program (or Preview) from
// this browser. Once a cast from this bus is running, a second "Cast
// settings" button opens the Cast panel (`ActiveCastCard`) in an inline
// popover so the desk reaches the Video/Audio/Subtitles pickers (issue
// #418's "Desk: audio/subtitle/video pickers on the Cast panel") without
// navigating away from the desk. Renders nothing outside a `CastProvider`
// (isolated desk renders).
import { useState } from 'react';
import type { JSX } from 'react';
import { useLingui } from '@lingui/react/macro';
import { Settings } from 'lucide-react';

import { useCastConfig } from '../api/castStreamsQueries';
import { Button } from '../components/ui/button';
import { ActiveCastCard } from './ActiveCastCard';
import { CastButton } from './CastButton';
import { useFeedOptions } from './CastFeedPicker';
import { useCastSnapshot, useHasCastProvider } from './CastProvider';

export function DeskCastButton({ bus }: { readonly bus: 'program' | 'preview' }): JSX.Element | null {
  const { t } = useLingui();
  const hasProvider = useHasCastProvider();
  const [open, setOpen] = useState(false);
  if (!hasProvider) return null;
  const name = bus === 'program' ? t`Program` : t`Preview`;
  return (
    <DeskCastButtonInner name={name} bus={bus} open={open} setOpen={setOpen} />
  );
}

function DeskCastButtonInner({
  name,
  bus,
  open,
  setOpen,
}: {
  readonly name: string;
  readonly bus: 'program' | 'preview';
  readonly open: boolean;
  readonly setOpen: (open: boolean) => void;
}): JSX.Element {
  const { t } = useLingui();
  const cast = useCastSnapshot();
  const config = useCastConfig(false, open);
  const options = useFeedOptions();
  const casting = cast.status !== 'idle' && cast.status !== 'error';
  return (
    <span className="relative inline-flex items-center gap-1">
      <CastButton compact labelOnWide className="md:w-auto md:px-2 md:gap-1" target={{ key: `feed:bus:${bus}`, name, feed: { bus } }} />
      {casting ? (
        <Button
          variant="ghost"
          size="icon"
          aria-label={t`Cast settings`}
          title={t`Cast settings`}
          aria-expanded={open}
          onClick={(): void => {
            setOpen(!open);
          }}
        >
          <Settings aria-hidden="true" />
        </Button>
      ) : null}
      {open ? (
        <div className="absolute end-0 top-full z-50 mt-2 w-80 max-w-[90vw]" data-testid="desk-cast-settings">
          <ActiveCastCard options={options} outputs={config.data?.outputs ?? []} />
        </div>
      ) : null}
    </span>
  );
}
