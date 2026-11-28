import type { I18n } from '@lingui/core';

import { OPERATOR_SURFACE_HEADINGS, OPERATOR_SURFACE_PATHS, type OperatorSurfacePath } from './operatorRoutes';

const BRAND = 'Conspect';

/**
 * The document title for an operator route in a given locale. The heading is
 * resolved through Lingui's catalog for the active locale; a message the catalog
 * lacks falls back to its source string, never to a blank or a hash id.
 */
export function localisedOperatorTitle(path: OperatorSurfacePath, i18n: I18n): string {
  return `${i18n._(OPERATOR_SURFACE_HEADINGS[path])} — ${BRAND}`;
}

/** The English (source-locale) manifest, kept for uniqueness and contract checks. */
export const OPERATOR_PATHS: readonly OperatorSurfacePath[] = OPERATOR_SURFACE_PATHS;

/** Return the unique title for an operator route; default to the app label elsewhere. */
export function routeDocumentTitle(pathname: string, i18n?: I18n): string {
  const matchingPath = OPERATOR_PATHS.find((path) => path === pathname);
  if (matchingPath !== undefined) {
    return i18n === undefined ? `${englishHeading(matchingPath)} — ${BRAND}` : localisedOperatorTitle(matchingPath, i18n);
  }
  return 'Conspect — Management';
}

/** Source-locale heading text for a route; used only where no i18n instance is passed. */
function englishHeading(path: OperatorSurfacePath): string {
  return OPERATOR_SURFACE_HEADINGS[path].message ?? path;
}

/** Source-locale manifest: one title per audited route, English. */
export const OPERATOR_ROUTE_TITLES: Readonly<Record<OperatorSurfacePath, string>> = {
  '/': `${englishHeading('/')} — ${BRAND}`,
  '/routing': `${englishHeading('/routing')} — ${BRAND}`,
  '/sources': `${englishHeading('/sources')} — ${BRAND}`,
  '/layouts': `${englishHeading('/layouts')} — ${BRAND}`,
  '/switcher': `${englishHeading('/switcher')} — ${BRAND}`,
};

const titleValues = OPERATOR_PATHS.map((path) => OPERATOR_ROUTE_TITLES[path]);
if (new Set(titleValues).size !== OPERATOR_PATHS.length) {
  throw new Error('Operator route document titles must be unique');
}
