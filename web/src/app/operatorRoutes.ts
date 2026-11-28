import { msg } from '@lingui/core/macro';
import type { MessageDescriptor } from '@lingui/core';

/**
 * Audited operator routes and their required visible heading. Each heading is a
 * Lingui `msg` descriptor: its content-hash id is what the compiled catalogs key
 * on, so `i18n._(descriptor)` resolves the active locale. A plain string would
 * look up the English text itself and never reach a translation.
 */
export const OPERATOR_SURFACE_HEADINGS = {
  '/': msg`Production desk`,
  '/routing': msg`Routing & outputs`,
  '/sources': msg`Sources & library`,
  '/layouts': msg`Video layouts`,
  '/switcher': msg`Program / Preview switcher`,
} as const satisfies Record<string, MessageDescriptor>;

/** Keep the document-title manifest in lockstep with visible route headings. */
export type OperatorSurfacePath = keyof typeof OPERATOR_SURFACE_HEADINGS;
export const OPERATOR_SURFACE_PATHS: readonly OperatorSurfacePath[] = ['/', '/routing', '/sources', '/layouts', '/switcher'];
