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
