// An accessible on/off switch (ARIA `role="switch"`) for the account-side
// consent + relay toggles. Account-scoped (not a shared shadcn primitive) so it
// adds no coordination cost with the framework-owner session. The hue is paired
// with the on/off label text (never colour alone — WCAG 1.4.1) and the control
// meets the 44px target.
import type { JSX } from "react";

/** Props for {@link Switch}. */
export interface SwitchProps {
  /** The accessible name (what the switch controls). */
  readonly label: string;
  /** Whether the switch is on. */
  readonly checked: boolean;
  /** Called with the requested new state when toggled. */
  readonly onToggle: (next: boolean) => void;
  /** Whether the switch is disabled (e.g. a mutation is in flight). */
  readonly disabled?: boolean;
}

/** A labelled ARIA switch. */
export function Switch({ label, checked, onToggle, disabled }: SwitchProps): JSX.Element {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      disabled={disabled ?? false}
      onClick={(): void => {
        onToggle(!checked);
      }}
      className={`relative inline-flex h-6 min-h-[44px] w-11 min-w-[44px] shrink-0 cursor-pointer items-center rounded-full border p-0.5 transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring disabled:cursor-not-allowed disabled:opacity-50 ${
        checked
          ? "border-status-live/40 bg-status-live/30"
          : "border-input bg-muted"
      }`}
    >
      <span
        aria-hidden="true"
        className={`pointer-events-none inline-block size-5 rounded-full bg-foreground shadow transition-transform ${
          checked ? "translate-x-5 rtl:-translate-x-5" : "translate-x-0"
        }`}
      />
    </button>
  );
}
