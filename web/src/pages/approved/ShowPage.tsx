// Show rundown (approved round-4 hierarchy). There is NO production-show
// rundown API: NOW/NEXT, the rundown and the cue card render empty with the
// specific reason, and Take/Stop/Hold/Skip/Arm are disabled. Only the local
// browser clock (with its zone) is real. Nothing here switches desk program.
import { Info } from 'lucide-react';
import { useEffect, useState } from 'react';
import type { JSX } from 'react';
import { Link } from 'react-router-dom';
import { Trans, useLingui } from '@lingui/react/macro';

import './show-approved.css';

function localClock(now: Date): { time: string; zone: string } {
  const zone = Intl.DateTimeFormat().resolvedOptions().timeZone || 'local';
  return { time: now.toLocaleTimeString(undefined, { hour12: false }), zone };
}

export function ShowPage(): JSX.Element {
  const { t } = useLingui();
  const [helpOpen, setHelpOpen] = useState(false);
  const [now, setNow] = useState(() => new Date());
  useEffect(() => {
    const id = window.setInterval(() => { setNow(new Date()); }, 1000);
    return (): void => { window.clearInterval(id); };
  }, []);
  const clock = localClock(now);
  const reason = t`No production rundown API exists, so there is no cue list to take, hold or skip.`;
  const dash = '—';
  const btn = (label: string, tone: 'take' | 'stop' | 'arm' | 'plain'): JSX.Element => (
    <button type="button" className={`show-btn show-btn-${tone}`} disabled aria-disabled="true" title={reason}>{label}</button>
  );
  return (
    <div data-approved-page="show">
      <div className="screen-head">
        <div>
          <h1><Trans>Show rundown</Trans></h1>
          <p className="screen-sub">
            <Trans>The running order as cue cards.</Trans>
            <button type="button" className="help-btn" aria-expanded={helpOpen} aria-controls="show-help"
              onClick={(): void => { setHelpOpen((o) => !o); }}>
              <Info aria-hidden="true" size={14} className="mv-help-ico" /> <Trans>Help</Trans>
            </button>
          </p>
          <div className="help-panel" id="show-help" hidden={!helpOpen}>
            <p><Trans>The production rundown is unavailable: no show-control contract exists. No video switch occurs here; the Desk PROGRAM monitor is the only truth for what viewers see.</Trans></p>
            <p><Trans>Graphics-channel rundowns and Salvos are real workflows, but they are not a production rundown.</Trans></p>
          </div>
        </div>
        <div className="show-clocks">
          <span className="lbl"><Trans>Local clock</Trans></span>
          <span className="clock-mono" data-testid="show-clock">{clock.time}</span>
          <span className="lbl" data-testid="show-zone">{clock.zone}</span>
          <span className="lbl"><Trans>Planned cue in</Trans></span>
          <span className="clock-mono">{dash}</span>
        </div>
      </div>

      <div className="now-next" aria-label={t`Now and next`}>
        <div className="nn-cell">
          <span className="lbl"><Trans>Now — rundown live</Trans></span>
          <div className="nn-line">{dash}</div>
          <div className="nn-meta"><Trans>No live item: timing and overrun are unknown.</Trans></div>
        </div>
        <div className="nn-cell">
          <span className="lbl"><Trans>Next — cue</Trans></span>
          <div className="nn-line">{dash}</div>
          <div className="nn-meta"><Trans>No planned cue.</Trans></div>
        </div>
        <div className="nn-actions">
          {btn(t`Take next`, 'take')}
          {btn(t`Stop live item`, 'stop')}
        </div>
        <p className="nn-note" role="status"><Trans>Unavailable:</Trans> {reason}</p>
      </div>

      <div className="show-grid">
        <ol className="rundown" aria-label={t`Running order`}>
          <li className="rundown-empty">
            <b><Trans>Rundown unavailable</Trans></b>
            <Trans>No items are listed. Use the real workflows instead:</Trans>{' '}
            <Link to="/graphics"><Trans>Graphics</Trans></Link>
            <Link to="/salvos"><Trans>Salvos</Trans></Link>
          </li>
        </ol>
        <aside className="cue-rail">
          <div className="cue-card">
            <h2><Trans>Cue card — next</Trans></h2>
            <div className="cue-body">
              <div className="cue-line"><span><Trans>Item</Trans></span><b>{dash}</b></div>
              <div className="cue-line"><span><Trans>Source</Trans></span><b>{dash}</b></div>
              <div className="cue-line"><span><Trans>Planned</Trans></span><b>{dash}</b></div>
            </div>
            <div className="cue-actions">
              {btn(t`Skip cue`, 'plain')}
              {btn(t`Hold cue`, 'plain')}
              {btn(t`Arm cue`, 'arm')}
            </div>
            <p className="nn-note" data-testid="cue-unavailable"><Trans>Unavailable: there is no cue list, so cue actions cannot run.</Trans></p>
          </div>
        </aside>
      </div>
    </div>
  );
}

export default ShowPage;
