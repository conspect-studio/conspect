// Approved Media screen composition (round-4): heading/help + task tabs
// (VT transport / Capture / Clip library). Presentation only: the page owns
// all state and operations. Capture and Clip library have no backend
// contract yet, so they render explicit, specific unavailable states.
import { useState } from 'react';
import type { JSX, ReactNode } from 'react';
import { Link } from 'react-router-dom';
import { Trans, useLingui } from '@lingui/react/macro';

import { LibraryBrowser } from '../../components/broadcast/library';
import './media-approved.css';

export type MediaTab = 'transport' | 'capture' | 'library';

export interface MediaSurfaceProps {
  readonly tab: MediaTab;
  readonly onTabChange: (tab: MediaTab) => void;
  /** The working VT transport region. */
  readonly transport: ReactNode;
}

/** Capture area: no capture API exists, so every control is absent with a reason. */
export function CaptureUnavailable(): JSX.Element {
  return (
    <aside className="media-side" data-testid="media-capture-unavailable">
      <div className="cap-card">
        <h2>
          <Trans>Capture</Trans>{' '}
          <span className="chip chip-pending"><Trans>Unavailable</Trans></span>
        </h2>
        <p className="rail-note">
          <Trans>
            Capture-to-slot is not available: this control plane exposes no capture
            endpoint. Nothing is captured, scheduled or stored from this page.
          </Trans>
        </p>
        <ul className="unavail-list">
          <li><b><Trans>One-shot capture</Trans></b><Trans>No capture job API exists yet.</Trans></li>
          <li><b><Trans>Interval capture</Trans></b><Trans>No capture scheduling API exists yet.</Trans></li>
          <li><b><Trans>Decoded-memory budget</Trans></b><Trans>No memory-budget report is exposed, so no usage figure is shown.</Trans></li>
        </ul>
      </div>
    </aside>
  );
}

/**
 * Graphics region of the Media tab: the real graphics (CG) workflow lives at
 * /graphics (Playout / Library / Templates / Data). Linked, never simulated.
 */
export function GraphicsRegion(): JSX.Element {
  const views: readonly { to: string; label: ReactNode }[] = [
    { to: '/graphics', label: <Trans>Playout</Trans> },
    { to: '/graphics/library', label: <Trans>Library</Trans> },
    { to: '/graphics/templates', label: <Trans>Templates</Trans> },
    { to: '/graphics/data', label: <Trans>Data</Trans> },
  ];
  return (
    <aside className="media-side" data-testid="media-graphics-region">
      <div className="cap-card">
        <h2><Trans>Graphics</Trans></h2>
        <p className="rail-note">
          <Trans>
            Graphics (CG) playout is a separate workflow: prepare items, cue them on preview and take
            them to air on a graphics channel. Open it here; nothing is cued or taken from this tab.
          </Trans>
        </p>
        <nav className="gfx-links" aria-label="Graphics workflow">
          {views.map((v) => (
            <Link key={v.to} to={v.to} className="gfx-link">{v.label}</Link>
          ))}
        </nav>
      </div>
    </aside>
  );
}

/** Clip-library tab: the shared LibraryBrowser in its honest listing-unavailable state. */
export function LibraryTab(): JSX.Element {
  const { t } = useLingui();
  const [query, setQuery] = useState('');
  const [sortId, setSortId] = useState('recent');
  return (
    <div className="lib-card" data-testid="media-library-tab">
      <h2><Trans>Clip library</Trans></h2>
      <p className="rail-note">
        <Trans>
          Temporary state: the media-library listing, save and unload APIs are not shipped yet.
          Load by asset id from the VT transport tab still works.
        </Trans>
      </p>
      <LibraryBrowser
        assets={[]}
        listingGate={{
          supported: false,
          unavailableReason: t`No library listing endpoint exists on this control plane yet, so no clips are listed.`,
        }}
        query={query}
        onSearchChange={setQuery}
        activeFilterOptionIds={[]}
        sortOptions={[{ id: 'recent', label: t`Most recent` }, { id: 'name', label: t`Name` }]}
        activeSortId={sortId}
        onSortChange={setSortId}
        selectionMode={false}
        selectedAssetIds={new Set<string>()}
      />
    </div>
  );
}

/** The approved Media layout. */
export function MediaSurface({ tab, onTabChange, transport }: MediaSurfaceProps): JSX.Element {
  const { t } = useLingui();
  const [helpOpen, setHelpOpen] = useState(false);
  const tabs: readonly { id: MediaTab; label: string }[] = [
    { id: 'transport', label: t`VT transport` },
    { id: 'capture', label: t`Capture` },
    { id: 'library', label: t`Clip library` },
  ];
  return (
    <div data-approved-page="media" data-mt={tab}>
      <div className="screen-head">
        <div>
          <h1><Trans>Media · VT & graphics</Trans></h1>
          <p className="screen-sub">
            <Trans>VT transport, graphics, capture and the clip library.</Trans>
            <button
              type="button"
              className="help-btn"
              aria-expanded={helpOpen}
              aria-controls="media-help"
              onClick={(): void => { setHelpOpen((open) => !open); }}
            >
              ⓘ <Trans>Help</Trans>
            </button>
          </p>
          <div className="help-panel" id="media-help" hidden={!helpOpen}>
            <p>
              <Trans>
                VT slots are transport channels. Each command is accepted asynchronously; live state and
                playhead arrive on the realtime stream. A missing observation is shown as unknown.
              </Trans>
            </p>
            <p>
              <Trans>
                Capture and the clip library listing are unavailable until their backend contracts ship.
              </Trans>
            </p>
          </div>
        </div>
        <nav className="media-tabs" aria-label={t`Media tasks`}>
          {tabs.map((entry) => (
            <button
              key={entry.id}
              type="button"
              className="mt-btn"
              aria-pressed={tab === entry.id}
              onClick={(): void => { onTabChange(entry.id); }}
            >
              {entry.label}
            </button>
          ))}
        </nav>
      </div>
      {tab === 'transport' ? <div className="mt-panel mt-transport">
          <div className="mt-transport-main">{transport}</div>
          <GraphicsRegion />
        </div> : null}
      {tab === 'capture' ? <div className="mt-panel mt-capture"><CaptureUnavailable /></div> : null}
      {tab === 'library' ? <div className="mt-panel mt-library"><LibraryTab /></div> : null}
    </div>
  );
}
