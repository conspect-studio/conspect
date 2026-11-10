// Program/preview bus ids for the Sources tally column, from the SAME switcher
// state the Desk reads (`program_source` is a source id there too). Confirmed
// only after the engine tick has been OBSERVED ADVANCING between polls and the
// state is still refreshing; a frozen tick, a cached remount or a merely
// successful poll never confirms, so the page reads Unknown and never guesses.
import { useEffect, useState } from 'react';
import { useQuery, useQueryClient } from '@tanstack/react-query';

import { getSwitcher } from '../../api/switcher';
import { tickConfirmed, useTickAdvancedAt } from '../../components/broadcast/tickObservation';

/** Confirmed bus ids. */
export interface ProgramTally {
  readonly program: string;
  readonly preview: string;
}

const STATE_KEY = ['switcher', 'me1'] as const;

/** The confirmed program/preview source ids, or undefined when not confirmed. */
export function useProgramTally(): ProgramTally | undefined {
  const client = useQueryClient();
  const state = useQuery({
    queryKey: STATE_KEY,
    queryFn: () => getSwitcher(),
    retry: false,
    refetchInterval: 2000,
  });
  const [now, setNow] = useState(Date.now);
  const advancedAt = useTickAdvancedAt(client, STATE_KEY.join('/'), state.data?.tick, state.dataUpdatedAt);
  useEffect(() => {
    const timer = setInterval(() => { setNow(Date.now()); }, 1000);
    return (): void => { clearInterval(timer); };
  }, []);
  const data = state.data;
  if (data === undefined || !tickConfirmed(advancedAt, state.dataUpdatedAt, now, state.isError)) return undefined;
  return { program: data.program_source, preview: data.preview_source };
}
