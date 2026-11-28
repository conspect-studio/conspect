import { useLingui } from '@lingui/react/macro';
import type { JSX } from 'react';

import { PageHeader } from '../components/PageHeader';
import { OPERATOR_SURFACE_HEADINGS, type OperatorSurfacePath } from './operatorRoutes';

/** Shared visible h1 for the curated surfaces. */
export function OperatorRouteHeading({ route }: { readonly route: OperatorSurfacePath }): JSX.Element {
  const { i18n } = useLingui();
  return <PageHeader title={i18n._(OPERATOR_SURFACE_HEADINGS[route])} routeKey={route} />;
}
