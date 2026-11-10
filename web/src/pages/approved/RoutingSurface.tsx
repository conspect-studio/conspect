// Approved Routing & outputs composition (lane R). Presentational pieces only:
// the page keeps every query/mutation. Nothing here invents a fact — a source
// is "On PGM" only when the switcher's authoritative program_source says so;
// every other crosspoint state is Unknown (the routing API has no readback).
import type { JSX, ReactNode } from "react";
import { Trans } from "@lingui/react/macro";

import "./routing-approved.css";

/** Frame: scopes the namespaced CSS + local tokens. */
export function RoutingFrame({
  children,
}: {
  readonly children: ReactNode;
}): JSX.Element {
  return <div data-approved-page="routing">{children}</div>;
}

export interface PgmReadout {
  /** Authoritative program source id, or undefined when unreadable. */
  readonly programSource: string | undefined;
  readonly loading: boolean;
}

/** PGM readback + restart-summary strip. */
export function RouteTruthStrip({
  pgm,
}: {
  readonly pgm: PgmReadout;
}): JSX.Element {
  return (
    <div className="route-truth" data-testid="route-truth">
      <div className="pgm-readout" data-testid="pgm-readout">
        <span className="lbl">
          <Trans>On PGM out now</Trans>
        </span>
        <b>
          {pgm.programSource ??
            (pgm.loading ? <Trans>Reading…</Trans> : <Trans>Unknown</Trans>)}
        </b>
      </div>
      <span className="pending-summary" data-testid="pending-summary">
        <Trans>
          Restart state: Unknown — the API exposes no per-output restart
          readback.
        </Trans>
      </span>
    </div>
  );
}

export interface PickerSource {
  readonly id: string;
  readonly name: string;
}

/** Source picker: selects the source input id; PGM chip only from switcher. */
export function SourcePicker({
  sources,
  selected,
  programSource,
  status,
  onPick,
}: {
  readonly sources: readonly PickerSource[];
  readonly selected: string;
  readonly programSource: string | undefined;
  readonly status: "pending" | "error" | "ready";
  readonly onPick: (id: string) => void;
}): JSX.Element {
  return (
    <div className="route-picker" data-testid="route-source-picker">
      <p className="lbl">
        <Trans>Pick a source</Trans>
      </p>
      {status === "pending" ? (
        <p className="rp-note" role="status">
          <Trans>Loading sources…</Trans>
        </p>
      ) : status === "error" ? (
        <p className="rp-note">
          <Trans>
            Source list unavailable — type the source input id below.
          </Trans>
        </p>
      ) : sources.length === 0 ? (
        <p className="rp-note">
          <Trans>No sources configured.</Trans>
        </p>
      ) : (
        <div className="rp-sources">
          {sources.map((source) => (
            <button
              key={source.id}
              type="button"
              className="rp-row"
              aria-pressed={selected === source.id}
              data-testid={`route-source-${source.id}`}
              onClick={(): void => {
                onPick(source.id);
              }}
            >
              <b>{source.name}</b>
              <span className="rp-cur">{source.id}</span>
              {programSource === source.id ? (
                <span className="pill pill-live">
                  <Trans>● on PGM</Trans>
                </span>
              ) : (
                <span className="pill">
                  <Trans>PGM: not this source</Trans>
                </span>
              )}
            </button>
          ))}
        </div>
      )}
      <p className="patch-legend">
        <Trans>
          Crosspoint state for video cells and audio targets is Unknown: the
          routing API has no readback. A take applies only from the plan panel.
        </Trans>
      </p>
    </div>
  );
}

/** Two-column grid: crosspoints (+plan) and output sinks. */
export function RouteGrid({
  patch,
  outputs,
}: {
  readonly patch: ReactNode;
  readonly outputs: ReactNode;
}): JSX.Element {
  return (
    <div className="route-grid">
      <div className="patch-block">
        <div className="block-head">
          <h2>
            <Trans>Crosspoints</Trans>
          </h2>
        </div>
        {patch}
      </div>
      <div className="outputs-block">{outputs}</div>
    </div>
  );
}
