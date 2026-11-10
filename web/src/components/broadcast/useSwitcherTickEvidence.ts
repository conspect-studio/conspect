// The ONE tick-observation source the header and the Desk both read: the
// switcher state query (same key, so de-duplicated) fed through tickObservation.
// Confirmed only on an OBSERVED tick advancement, so header and desk cannot disagree.
import { useEffect, useState } from 'react';
import { useQuery, useQueryClient } from '@tanstack/react-query';

import { getSwitcher } from '../../api/switcher';
import { tickConfirmed, useTickAdvancedAt } from './tickObservation';

export const SWITCHER_STATE_KEY = ['switcher', 'me1'] as const;

/** Last observed tick advancement (+ a consistent clock) while confirmed; null otherwise (unconfirmed). */
export function useConfirmedTick(): { readonly at: number; readonly now: number } | null {
  const client = useQueryClient();
  const state = useQuery({ queryKey: SWITCHER_STATE_KEY, queryFn: () => getSwitcher(), retry: false, refetchInterval: 2000 });
  const [now, setNow] = useState(Date.now);
  const advancedAt = useTickAdvancedAt(client, SWITCHER_STATE_KEY.join('/'), state.data?.tick, state.dataUpdatedAt);
  useEffect(() => { const timer = setInterval(() => { setNow(Date.now()); }, 1000); return () => { clearInterval(timer); }; }, []);
  // The clock is clamped to the observation time: a 1s-cached clock must never read as a future tick.
  return tickConfirmed(advancedAt, state.dataUpdatedAt, now, state.isError) ? { at: advancedAt, now: Math.max(now, advancedAt) } : null;
}
