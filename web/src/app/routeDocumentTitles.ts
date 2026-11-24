import { OPERATOR_SURFACE_HEADINGS, OPERATOR_SURFACE_PATHS, type OperatorSurfacePath } from './operatorRoutes';

const BRAND = 'Conspect';

/** One route-title manifest, composed from each audited route's heading. */
export const OPERATOR_ROUTE_TITLES = {
  '/': `${OPERATOR_SURFACE_HEADINGS['/']} — ${BRAND}`,
  '/routing': `${OPERATOR_SURFACE_HEADINGS['/routing']} — ${BRAND}`,
  '/sources': `${OPERATOR_SURFACE_HEADINGS['/sources']} — ${BRAND}`,
  '/layouts': `${OPERATOR_SURFACE_HEADINGS['/layouts']} — ${BRAND}`,
  '/switcher': `${OPERATOR_SURFACE_HEADINGS['/switcher']} — ${BRAND}`,
} satisfies Record<OperatorSurfacePath, string>;

export const OPERATOR_PATHS: readonly OperatorSurfacePath[] = OPERATOR_SURFACE_PATHS;
const titleValues = OPERATOR_PATHS.map((path) => OPERATOR_ROUTE_TITLES[path]);
if (new Set(titleValues).size !== OPERATOR_PATHS.length) {
  throw new Error('Operator route document titles must be unique');
}

/** Return the unique title for an operator route; default to the app label elsewhere. */
export function routeDocumentTitle(pathname: string): string {
  const matchingPath = OPERATOR_PATHS.find((path) => path === pathname);
  if (matchingPath !== undefined) {
    return OPERATOR_ROUTE_TITLES[matchingPath];
  }
  return 'Conspect — Management';
}
