import { describe, expect, it } from 'vitest';
import { createElement } from 'react';
import { screen } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { OPERATOR_SURFACE_HEADINGS, OPERATOR_SURFACE_PATHS } from './operatorRoutes';
import { OPERATOR_ROUTE_TITLES, routeDocumentTitle } from './routeDocumentTitles';
import { router } from './router';
import { RoutingHead } from '../pages/approved/RoutingSurface';
import { LayoutsPage } from '../pages/LayoutsPage';
import { SwitcherPage } from '../pages/SwitcherPage';
import { renderWithProviders } from '../test/render';

describe('operator route document titles', () => {
  it('defines one distinct title for every audited route', () => {
    const paths = OPERATOR_SURFACE_PATHS;
    expect(Object.keys(OPERATOR_ROUTE_TITLES).sort()).toEqual([...paths].sort());
    const root = router.routes.find((route) => route.path === '/');
    const titledPaths = (root?.children ?? []).flatMap((route) => {
      const handle: unknown = route.handle;
      if (typeof handle === 'object' && handle !== null && 'documentTitlePath' in handle) {
        return [handle.documentTitlePath];
      }
      return [];
    });
    expect(titledPaths.sort()).toEqual([...paths].sort());
    expect(OPERATOR_ROUTE_TITLES['/']).toBe('Production desk — Conspect');
    expect(OPERATOR_ROUTE_TITLES['/routing']).toBe('Routing & outputs — Conspect');
    expect(OPERATOR_ROUTE_TITLES['/sources']).toBe('Sources & library — Conspect');
    expect(OPERATOR_ROUTE_TITLES['/layouts']).toBe('Video layouts — Conspect');
    expect(OPERATOR_ROUTE_TITLES['/switcher']).toBe('Program / Preview switcher — Conspect');
    const titles = paths.map((path) => routeDocumentTitle(path));
    expect(titles).toEqual([
      'Production desk — Conspect',
      'Routing & outputs — Conspect',
      'Sources & library — Conspect',
      'Video layouts — Conspect',
      'Program / Preview switcher — Conspect',
    ]);
    expect(new Set(titles).size).toBe(paths.length);
    const headingContracts = [
      ['/routing', createElement(RoutingHead)],
      ['/layouts', createElement(LayoutsPage)],
      ['/switcher', createElement(SwitcherPage)],
    ] as const;
    for (const [path, page] of headingContracts) {
      const { unmount } = renderWithProviders(createElement(MemoryRouter, null, page));
      const headings = screen.getAllByRole('heading', { level: 1 });
      expect(headings).toHaveLength(1);
      expect(headings[0]).toHaveTextContent(OPERATOR_SURFACE_HEADINGS[path]);
      unmount();
    }
    expect(paths.map((path) => OPERATOR_ROUTE_TITLES[path]))
      .toEqual(paths.map((path) => `${OPERATOR_SURFACE_HEADINGS[path]} — Conspect`));
  });

  it('keeps the existing app title for other routes', () => {
    expect(routeDocumentTitle('/media-players')).toBe('Conspect — Management');
  });
});
