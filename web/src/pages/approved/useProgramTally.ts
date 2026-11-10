// Program/preview bus ids for the Sources tally column, from the SAME switcher
// state the Desk reads (`program_source` is a source id there too). Confirmed
// only while the state keeps refreshing and the engine tick keeps advancing;
// otherwise undefined so the page reads Unknown and never guesses.
import { useEffect, useState } from 'react';
import { useQuery } from '@tanstack/react-query';

import { getSwitcher } from '../../api/switcher';

/** Confirmed bus ids. */
export interface ProgramTally {
  readonly program: string;
  readonly preview: string;
}

const FRESH_MS = 6000;

/** The confirmed program/preview source ids, or undefined when not confirmed. */
export function useProgramTally(): ProgramTally | undefined {
  const state = useQuery({
    queryKey: ['switcher', 'me1'],
    queryFn: () => getSwitcher(),
    retry: false,
    refetchInterval: 2000,
  });
  const [now, setNow] = useState(Date.now);
  const [lastTickAt, setLastTickAt] = useState(0);
  useEffect(() => {
    const timer = setTimeout(() => {
      if (state.data?.tick !== undefined) setLastTickAt(Date.now());
    }, 0);
    return (): void => { clearTimeout(timer); };
  }, [state.data?.tick]);
  useEffect(() => {
    const timer = setInterval(() => { setNow(Date.now()); }, 1000);
    return (): void => { clearInterval(timer); };
  }, []);
  const data = state.data;
  if (data === undefined || state.isError || state.dataUpdatedAt <= 0 || lastTickAt <= 0) return undefined;
  if (now - state.dataUpdatedAt >= FRESH_MS || now - lastTickAt >= FRESH_MS) return undefined;
  return { program: data.program_source, preview: data.preview_source };
}
