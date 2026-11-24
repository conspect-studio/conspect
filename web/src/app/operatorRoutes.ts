/** Audited operator routes and their required visible heading. */
export const OPERATOR_SURFACE_HEADINGS = {
  '/': 'Production desk',
  '/routing': 'Routing & outputs',
  '/sources': 'Sources & library',
  '/layouts': 'Video layouts',
  '/switcher': 'Program / Preview switcher',
} as const;

/** Keep the document-title manifest in lockstep with visible route headings. */
export type OperatorSurfacePath = keyof typeof OPERATOR_SURFACE_HEADINGS;
export const OPERATOR_SURFACE_PATHS: readonly OperatorSurfacePath[] = ['/', '/routing', '/sources', '/layouts', '/switcher'];
