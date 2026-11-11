export function tallyForSource(id: string, state: { program_source: string; preview_source: string } | undefined, fresh: boolean): string {
  if (!state || !fresh) return 'unknown';
  return id === state.program_source ? 'program' : id === state.preview_source ? 'preview' : 'dark';
}
export function deskControlReason(available: boolean, connected: boolean, fresh: boolean, inFlight: boolean, unresolved: boolean): string | undefined {
  if (!available) return 'Switcher unavailable';
  if (!connected) return 'Management disconnected';
  if (!fresh) return 'State stale or not confirmed';
  if (inFlight) return 'Transition running';
  if (unresolved) return 'Operation outcome pending or unknown';
  return undefined;
}
/**
 * Why one bus key cannot be taken, from the engine-confirmed buses and the
 * source's demand phase (ADR-0104). `undefined` means the key works. A source
 * that is idle or starting is still selectable: the engine starts it and the
 * take waits (bounded) for its first fresh frame.
 */
export function busKeyReason(
  bus: 'program' | 'preview',
  id: string,
  state: { program_source: string; preview_source: string } | undefined,
  phase: string | undefined,
): string | undefined {
  if (!state) return undefined;
  if (bus === 'preview' && id === state.program_source) return 'Already on Program — Preview must differ';
  if (bus === 'preview' && id === state.preview_source) return 'Already on Preview';
  if (bus === 'program' && id === state.program_source) return 'Already on Program';
  if (phase === 'failed') return 'Source failed — not producing frames';
  return undefined;
}
