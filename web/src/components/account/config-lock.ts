// The config-lock state hook (logic, no JSX) — separate from the chrome
// components so each file's exports are uniform (react-refresh). Resource forms
// import this to render read-only while enforcement locks reconfiguration.
import { useLicence } from "../../api/conspectQueries";
import type { EnforcementLevel } from "../../api/conspectQueries";

/** The config-lock state derived from the licence resource. */
export interface ConfigLockState {
  /** Whether reconfiguration is currently locked by enforcement. */
  readonly locked: boolean;
  /** The enforcement level driving the lock (for messaging), when known. */
  readonly level: EnforcementLevel | undefined;
}

/**
 * Read whether the engine is config-locked by enforcement. Resource forms read
 * this to render read-only with an explanatory banner that links to the Licence
 * screen. This is a courtesy: the API also returns a `409 config_locked` problem,
 * so the lock holds even if the UI is bypassed.
 */
export function useConfigLock(): ConfigLockState {
  const licence = useLicence();
  const status = licence.data?.status ?? null;
  return {
    locked: status?.config_locked ?? false,
    level: status?.enforcement,
  };
}
