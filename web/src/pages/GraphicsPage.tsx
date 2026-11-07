// Graphics (CG) — the character-generator / graphics playout section.
//
// Views (URL: /graphics, /graphics/library, /graphics/templates,
// /graphics/data): Playout (default), Library, Templates, Data. The model is
// data sources → templates → items → channel rundown → switcher source.
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { NavLink, useParams } from 'react-router-dom';

import { PageHeader } from '../components/PageHeader';
import { DataView } from './graphics/DataView';
import { LibraryView } from './graphics/LibraryView';
import { PlayoutView } from './graphics/PlayoutView';
import { TemplatesView } from './graphics/TemplatesView';

const VIEWS = [
  { key: 'playout', path: '/graphics', label: <Trans>Playout</Trans> },
  { key: 'library', path: '/graphics/library', label: <Trans>Library</Trans> },
  { key: 'templates', path: '/graphics/templates', label: <Trans>Templates</Trans> },
  { key: 'data', path: '/graphics/data', label: <Trans>Data</Trans> },
] as const;

export function GraphicsPage(): JSX.Element {
  const { t } = useLingui();
  const { view = 'playout' } = useParams();
  return (
    <div>
      <PageHeader
        title={<Trans>Graphics</Trans>}
        description={
          <Trans>
            Graphics playout (CG): prepare items from templates and live data, cue them on preview,
            and take them to air on a graphics channel the switcher layers over video.
          </Trans>
        }
      />
      <nav aria-label={t`Graphics views`} className="mb-4 inline-flex gap-1 rounded-lg bg-muted p-1">
        {VIEWS.map((v) => (
          <NavLink
            key={v.key}
            to={v.path}
            end
            className={({ isActive }): string =>
              `rounded-md px-3 py-1 text-sm font-medium ${
                isActive ? 'bg-background text-foreground shadow' : 'text-muted-foreground'
              }`
            }
          >
            {v.label}
          </NavLink>
        ))}
      </nav>
      {view === 'library' ? <LibraryView /> : null}
      {view === 'templates' ? <TemplatesView /> : null}
      {view === 'data' ? <DataView /> : null}
      {view !== 'library' && view !== 'templates' && view !== 'data' ? <PlayoutView /> : null}
    </div>
  );
}
