// Single typed entry for the control-plane contract.
//
// `schema.ts` is generated from main's docs/api/openapi.json and stays
// byte-identical to main. `pending/schema.pending.ts` is generated from the
// integration superset for routes NOT yet served by main (see
// pending/BACKEND_DEPENDENCIES.md). This barrel merges the two: pending
// component schemas add the missing names and shadow main's where the
// superset extends them. Once the backend lands, delete `pending/` and point
// these aliases back at `./schema` alone.
import type { components as MainComponents, paths } from './schema';
import type { components as PendingComponents, paths as PendingPaths } from './pending/schema.pending';

/** Every path the client may call: served by main plus pending-backend. */
export type AllPaths = paths & PendingPaths;
export type { paths, PendingPaths };

type MainSchemas = MainComponents['schemas'];
type PendingSchemas = PendingComponents['schemas'];

/** Merged component schemas (main + pending). */
export type Schemas = Omit<MainSchemas, keyof PendingSchemas> & PendingSchemas;

/** Drop-in replacement for the generated `components` type. */
export interface components {
  schemas: Schemas;
}
