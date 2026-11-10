// Approved Routing & outputs composition (lane R). Presentational pieces only:
// the page keeps every query/mutation. Nothing here invents a fact — a source
// is "On PGM" only when the switcher's authoritative program_source says so;
// every other crosspoint state is Unknown (the routing API has no readback).
import { useState } from "react";
import type { JSX, ReactNode } from "react";
import { Trans, useLingui } from "@lingui/react/macro";

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
  /** Friendly name of the program source when the source list carries one. */
  readonly programName?: string | undefined;
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
          {pgm.programName ??
            pgm.programSource ??
            (pgm.loading ? <Trans>Reading…</Trans> : <Trans>Unknown</Trans>)}
        </b>
        {pgm.programSource !== undefined &&
        pgm.programName !== undefined &&
        pgm.programName !== pgm.programSource ? (
          <span className="rp-cur" data-testid="pgm-readout-id">
            {pgm.programSource}
          </span>
        ) : null}
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

/** One real route destination: a video cell, an audio discrete track or a subtitle layer. */
export interface RouteDest {
  readonly kind: "video" | "audio" | "subtitle";
  readonly id: string;
  readonly label: string;
}

/** Heading + Help disclosure (exact screen-head composition). */
export function RoutingHead(): JSX.Element {
  const [open, setOpen] = useState(false);
  return (
    <div className="screen-head" data-testid="routing-head">
      <div>
        <h1>
          <Trans>Routing &amp; outputs</Trans>
        </h1>
        <p className="screen-sub">
          <Trans>
            Patch a source to a destination. Plan classifies the take without
            applying it; Take applies it.
          </Trans>
          <button
            type="button"
            className="help-btn"
            aria-expanded={open}
            aria-controls="help-routing"
            onClick={(): void => {
              setOpen(!open);
            }}
          >
            <Trans>ⓘ Help</Trans>
          </button>
        </p>
        <div className="help-panel" id="help-routing" hidden={!open}>
          <p>
            <b><Trans>Plan vs take:</Trans></b>{" "}
            <Trans>
              the plan is a dry run that classifies the take. Only Take
              applies it; a Class-2 migration is accepted asynchronously (HTTP
              202 is not applied) and its outcome arrives on the realtime
              stream.
            </Trans>
          </p>
          <p>
            <b><Trans>Configured ≠ on-air:</Trans></b>{" "}
            <Trans>
              each sink shows requested (config) against effective (measured).
              Where the API has no readback the value is Unknown, never
              assumed.
            </Trans>
          </p>
        </div>
      </div>
    </div>
  );
}

/** Desktop/tablet crosspoint grid over REAL destinations; state is Unknown (no readback). */
export function CrosspointGrid({
  sources,
  dests,
  selectedSource,
  selectedDest,
  planned,
  onPick,
}: {
  readonly sources: readonly PickerSource[];
  readonly dests: readonly RouteDest[];
  readonly selectedSource: string;
  readonly selectedDest: string;
  readonly planned: boolean;
  readonly onPick: (source: string, dest: RouteDest) => void;
}): JSX.Element {
  const { t } = useLingui();
  if (dests.length === 0 || sources.length === 0) {
    return (
      <p className="rp-note patch-wrap" data-testid="crosspoint-empty">
        <Trans>
          No crosspoint grid: it needs both configured sources and known
          destinations (layout cells, audio discrete tracks). Use the typed
          selectors below.
        </Trans>
      </p>
    );
  }
  return (
    <div className="patch-wrap" data-testid="crosspoint-grid">
      <table className="patch-table">
        <thead>
          <tr>
            <th className="src-h" scope="col">
              <Trans>Source ↓ / Dest →</Trans>
            </th>
            {dests.map((d) => (
              <th key={`${d.kind}:${d.id}`} scope="col" title={d.kind}>
                {d.label}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {sources.map((src) => (
            <tr key={src.id}>
              <th className="patch-src" scope="row">
                {src.name}
              </th>
              {dests.map((d) => {
                const isSel = selectedSource === src.id && selectedDest === d.id;
                return (
                  <td key={`${d.kind}:${d.id}`}>
                    <button
                      type="button"
                      className={isSel ? "xpt plan" : "xpt"}
                      aria-pressed={isSel}
                      aria-label={t`Select ${src.name} to ${d.label}`}
                      title={t`Crosspoint state Unknown — no readback`}
                      data-testid={`xpt-${src.id}-${d.id}`}
                      onClick={(): void => {
                        onPick(src.id, d);
                      }}
                    >
                      {isSel ? (planned ? "? PLAN" : "◇ SEL") : "?"}
                    </button>
                  </td>
                );
              })}
            </tr>
          ))}
        </tbody>
      </table>
      <p className="patch-legend">
        <Trans>
          Crosspoint states: ? Unknown (no readback) · ◇ SEL selected · ? PLAN
          dry-run plan. A take applies only from the plan panel.
        </Trans>
        {" "}
        <Trans>
          Destinations are layout cells and audio tracks because those are the
          only targets the routing API accepts (video cell, audio track /
          program bus, subtitle layer); output sinks (PGM, HLS …) are not
          crosspoint targets and are configured in the outputs panel.
        </Trans>
      </p>
    </div>
  );
}

/** Phone step 1: pick a real destination. Step 2 is the source picker. */
export function DestinationPicker({
  dests,
  selected,
  onPick,
  onBack,
  step,
}: {
  readonly dests: readonly RouteDest[];
  readonly selected: string;
  readonly step: 1 | 2;
  readonly onPick: (dest: RouteDest) => void;
  readonly onBack: () => void;
}): JSX.Element {
  const current = dests.find((d) => d.id === selected);
  return (
    <div className="route-dests" data-testid="route-dest-picker" data-step={step}>
      <div className="rp-step">
        {step === 1 || current === undefined ? (
          <p className="lbl">
            <Trans>1 — pick a destination</Trans>
          </p>
        ) : (
          <>
            <p className="lbl">
              <Trans>2 — pick the source for</Trans> <b>{current.label}</b>
            </p>
            <button type="button" className="rp-back" onClick={onBack}>
              <Trans>← Back</Trans>
            </button>
          </>
        )}
      </div>
      {step === 1 ? (
        dests.length === 0 ? (
          <p className="rp-note">
            <Trans>
              No destinations known (no layout cells or discrete audio tracks).
              Use the typed selectors.
            </Trans>
          </p>
        ) : (
          <div className="rp-dests">
            {dests.map((d) => (
              <button
                key={`${d.kind}:${d.id}`}
                type="button"
                className="rp-row"
                aria-pressed={selected === d.id}
                data-testid={`route-dest-${d.id}`}
                onClick={(): void => {
                  onPick(d);
                }}
              >
                <b>{d.label}</b>
                <span className="rp-cur">{d.kind}</span>
                <span className="pill">
                  <Trans>Crosspoint: Unknown</Trans>
                </span>
              </button>
            ))}
          </div>
        )
      ) : null}
    </div>
  );
}

/** The amber plan panel: summary, plan result and lock-aware confirmation. */
export function PlanPanel({
  selected,
  summary,
  children,
}: {
  readonly selected: boolean;
  readonly summary: ReactNode;
  readonly children: ReactNode;
}): JSX.Element {
  return (
    <div
      className="plan-panel"
      data-testid="plan-panel"
      data-selected={selected}
    >
      <h3>
        {selected ? summary : <Trans>Plan — nothing selected</Trans>}
      </h3>
      {selected ? null : (
        <p className="plan-desc">
          <Trans>
            Pick a destination and a source (or type them below) to plan a
            take. Plan and Take stay disabled until both are set.
          </Trans>
        </p>
      )}
      {children}
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
  hidden = false,
}: {
  readonly hidden?: boolean;
  readonly sources: readonly PickerSource[];
  readonly selected: string;
  readonly programSource: string | undefined;
  readonly status: "pending" | "error" | "ready";
  readonly onPick: (id: string) => void;
}): JSX.Element {
  return (
    <div
      className="route-picker source-step"
      data-testid="route-source-picker"
      data-phone-hidden={hidden}
    >
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
