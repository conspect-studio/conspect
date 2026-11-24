// Approved broadcast shell. Full legacy navigation lives in All screens.
import { useEffect, useRef } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { Outlet, useLocation } from 'react-router-dom';
import { ConfigLockBanner, LadderBanner, PendingActionStrip } from '../components/account/chrome';
import { HealthBanner } from '../components/HealthBanner';
import { BroadcastShell } from '../components/broadcast/BroadcastShell';
import { useEngineEvents } from '../realtime/useEngineEvents';
import { OPERATOR_SURFACE_PATHS } from './operatorRoutes';
import { routeDocumentTitle } from './routeDocumentTitles';

export function AppLayout(): JSX.Element {
  const { t } = useLingui();
  const { status } = useEngineEvents();
  const location = useLocation();
  const mainRef = useRef<HTMLElement>(null);
  useEffect(() => { mainRef.current?.focus(); }, [location.pathname]);
  const titledRoute = OPERATOR_SURFACE_PATHS.find((path) => path === location.pathname);
  useEffect(() => {
    document.title = routeDocumentTitle(titledRoute ?? location.pathname);
  }, [location.pathname, titledRoute]);
  const managementLabel = status === 'open' ? t`Connected` : status === 'connecting' ? t`Connecting` : status === 'reconnecting' ? t`Reconnecting` : t`Disconnected`;
  return <BroadcastShell managementLabel={managementLabel}>
    <a href="#main-content" className="sr-only focus:not-sr-only bd-skip"><Trans>Skip to content</Trans></a>
    <HealthBanner /><LadderBanner /><PendingActionStrip /><ConfigLockBanner />
    <main id="main-content" ref={mainRef} tabIndex={-1}><Outlet /></main>
  </BroadcastShell>;
}
