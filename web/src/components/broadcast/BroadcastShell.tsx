import { createContext, useContext, useState } from 'react';
import type { JSX, ReactNode } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { NavLink, useLocation } from 'react-router-dom';
import { Bell, Lock, LockOpen, Menu } from 'lucide-react';
import { useAlarms } from '../../api/alarmsQueries';
import { Dialog, DialogContent, DialogTitle, DialogTrigger } from '../ui/dialog';
import { Sidebar } from '../../app/Sidebar';
import { LicenceChip } from '../account/chrome';
import { LocaleSwitcher } from '../LocaleSwitcher';
import { ThemeToggle } from '../ThemeToggle';
import { ProducerStatus } from './ProducerStatus';
import './broadcast-desk.css';

const TakeLock = createContext({ locked: true });
// eslint-disable-next-line react-refresh/only-export-components -- session-only shell context accessor
export function useTakeLock(): { locked: boolean } { return useContext(TakeLock); }

/** Approved top navigation; complete legacy navigation remains in the drawer. */
export function BroadcastShell({ children, managementLabel }: { readonly children: ReactNode; readonly managementLabel: string }): JSX.Element {
  const { t } = useLingui();
  const [open, setOpen] = useState(false);
  const [locked, setLocked] = useState(true);
  const [unlock, setUnlock] = useState(false);
  // Real count only: unknown (loading/error) renders no badge at all.
  const alarms = useAlarms({ active: true });
  const alarmCount = alarms.isSuccess ? alarms.data.length : undefined;
  const { pathname } = useLocation();
  const tabs: readonly (readonly [string, string])[] = [
    ['/', t`Desk`], ['/show', t`Show`], ['/sources', t`Sources`],
    ['/audio', t`Audio`], ['/routing', t`Routing`], ['/media-players', t`Media`],
  ];
  const current = tabs.find(([path]) => (path === '/' ? pathname === '/' : pathname === path || pathname.startsWith(`${path}/`)))?.[1];
  return <TakeLock.Provider value={{ locked }}><div className="broadcast-shell dark">
    <header className="bd-top">
      <NavLink to="/" className="bd-brand" aria-label={t`CONSPECT Control — Desk`}><span className="bd-mark" aria-hidden="true" /><span className="bd-wordmark">CONSPECT</span> <small><Trans>Control</Trans></small>{current && <span className="bd-screen-title">{current}</span>}</NavLink>
      <nav className="bd-tabs" aria-label={t`Core screens`}>{tabs.map(([path, label], index) => <NavLink key={path} to={path} end={path === '/'}><span>{index + 1}</span>{label}</NavLink>)}</nav>
      <div className="bd-top-actions">
        <div className="bd-health"><ProducerStatus state="unknown" lastTickAt={null} managementLabel={managementLabel} /></div>
        <Dialog open={unlock} onOpenChange={setUnlock}><DialogTrigger asChild><button className="bd-lock" aria-pressed={locked} onClick={(event) => { if (!locked) { event.preventDefault(); setLocked(true); } }}>{locked ? <Lock size={14} aria-hidden="true" /> : <LockOpen size={14} aria-hidden="true" />}<span>{locked ? t`Takes locked` : t`Takes unlocked`}</span></button></DialogTrigger>
          <DialogContent><DialogTitle><Trans>Unlock time-critical takes?</Trans></DialogTitle><p><Trans>CUT will execute without another confirmation. AUTO remains unavailable. Direct program selection remains guarded.</Trans></p><button onClick={() => { setLocked(false); setUnlock(false); }}><Trans>Unlock takes</Trans></button><button onClick={() => { setUnlock(false); }}><Trans>Cancel</Trans></button></DialogContent></Dialog>
        <NavLink className="bd-icon" to="/alarms" aria-label={alarmCount ? t`Alarms and engine health — ${alarmCount} active` : t`Alarms and engine health`}><Bell size={20} aria-hidden="true" />{alarmCount ? <span className="bd-badge" data-testid="alarm-count" aria-hidden="true">{alarmCount}</span> : null}</NavLink>
        <Dialog open={open} onOpenChange={setOpen}><DialogTrigger asChild><button className="bd-icon" aria-label={t`Open navigation`}><Menu size={20} aria-hidden="true" /></button></DialogTrigger><DialogContent className="bd-drawer"><DialogTitle><Trans>All screens</Trans></DialogTitle><p className="bd-drawer-mgmt"><Trans>Management</Trans> · {managementLabel}</p><div className="flex flex-wrap gap-2"><LicenceChip /><LocaleSwitcher /><ThemeToggle /></div><Sidebar onNavigate={() => { setOpen(false); }} /></DialogContent></Dialog>
      </div>
    </header>
    {children}
  </div></TakeLock.Provider>;
}
