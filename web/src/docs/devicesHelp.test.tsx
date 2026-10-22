// In-app help for the devices domain (managed-devices.md §9): /help/devices,
// /help/devices/adopt, /help/display-nodes, /help/sync are registered (so they
// are searchable + linkable, ADR-W016), the glossary + config reference gain
// the device terms, and the copy keeps the honest doctrine: tiers as measured,
// discovery untrusted, and vendor support stated as "Supports …" — never
// "Official".
import { describe, expect, it } from 'vitest';
import { render } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';

import { DOCS_REGISTRY } from './registry';
import { TestProviders } from '../test/render';

const REQUIRED_PATHS = [
  '/help/devices',
  '/help/devices/adopt',
  '/help/display-nodes',
  '/help/sync',
] as const;

describe('devices help registration (ADR-W016)', () => {
  it.each(REQUIRED_PATHS.map((path) => [path] as const))(
    'registers %s',
    (path) => {
      expect(DOCS_REGISTRY.some((page) => page.path === path)).toBe(true);
    },
  );

  it('the glossary gains the devices-domain terms', () => {
    const glossary = DOCS_REGISTRY.find(
      (page) => page.path === '/help/concepts/glossary',
    );
    const ids = (glossary?.sections ?? []).map((section) => section.id);
    expect(ids).toContain('managed-device');
    expect(ids).toContain('sync-group');
    expect(ids).toContain('display-node');
  });

  it('the config reference documents [[devices]] and [[sync_groups]]', () => {
    const config = DOCS_REGISTRY.find((page) => page.path === '/help/config');
    const ids = (config?.sections ?? []).map((section) => section.id);
    expect(ids).toContain('devices');
    expect(ids).toContain('sync-groups');
  });
});

describe('devices help copy doctrine', () => {
  it('/help/devices says "Supports …", never "Official"', async () => {
    const { DevicesHelpPage } = await import('../pages/docs/DevicesHelpPage');
    render(
      <TestProviders>
        <MemoryRouter>
          <DevicesHelpPage />
        </MemoryRouter>
      </TestProviders>,
    );
    const text = document.body.textContent;
    expect(text).toMatch(/supports zowiebox/i);
    expect(text).not.toMatch(/official/i);
    // Program continuity is stated in the help, not only in the app.
    expect(text).toMatch(/program output/i);
  });

  it('/help/devices/adopt states the untrusted-discovery doctrine', async () => {
    const { DevicesAdoptHelpPage } = await import(
      '../pages/docs/DevicesAdoptHelpPage'
    );
    render(
      <TestProviders>
        <MemoryRouter>
          <DevicesAdoptHelpPage />
        </MemoryRouter>
      </TestProviders>,
    );
    const text = document.body.textContent;
    expect(text).toMatch(/untrusted/i);
    expect(text).toMatch(/never adopt/i);
    expect(text).toMatch(/explicit/i);
  });

  it('/help/sync states the honest tier table (bounded drift, cast excluded)', async () => {
    const { SyncHelpPage } = await import('../pages/docs/SyncHelpPage');
    render(
      <TestProviders>
        <MemoryRouter>
          <SyncHelpPage />
        </MemoryRouter>
      </TestProviders>,
    );
    const text = document.body.textContent;
    expect(text).toMatch(/frame-accurate/i);
    expect(text).toMatch(/±100–500\s*ms/);
    expect(text).toMatch(/cast/i);
    expect(text).toMatch(/weakest member/i);
  });

  it('/help/display-nodes covers enrollment and the achieved tier', async () => {
    const { DisplayNodesHelpPage } = await import(
      '../pages/docs/DisplayNodesHelpPage'
    );
    render(
      <TestProviders>
        <MemoryRouter>
          <DisplayNodesHelpPage />
        </MemoryRouter>
      </TestProviders>,
    );
    const text = document.body.textContent;
    expect(text).toMatch(/enrol/i);
    expect(text).toMatch(/frame-accurate/i);
  });
});
