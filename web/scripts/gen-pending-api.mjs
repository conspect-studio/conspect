#!/usr/bin/env node
/**
 * gen-pending-api.mjs — generate `src/api/pending/schema.pending.ts`.
 *
 * Input : scripts/pending-openapi.superset.json (committed copy of the
 *         integration-branch OpenAPI, a superset of main's).
 * Base  : ../docs/api/openapi.json (main's served contract).
 * Output: only the paths ABSENT from main, plus every component they reference
 *         transitively (so the file is self-contained), run through
 *         openapi-typescript. `src/api/schema.ts` stays byte-identical to main.
 *
 * Reproducible: `npm run generate:pending-api`. Delete the pending layer and
 * regenerate schema.ts once the backend PRs in pending/BACKEND_DEPENDENCIES.md
 * merge.
 */
import { readFileSync, writeFileSync, mkdtempSync, rmSync, readFileSync as rd } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';

const here = dirname(fileURLToPath(import.meta.url));
const main = JSON.parse(readFileSync(resolve(here, '../../docs/api/openapi.json'), 'utf8'));
const sup = JSON.parse(readFileSync(resolve(here, 'pending-openapi.superset.json'), 'utf8'));

const paths = {};
for (const [p, item] of Object.entries(sup.paths)) {
  if (!(p in main.paths)) paths[p] = item;
}

const schemas = sup.components.schemas;
const keep = new Set();
const visit = (node) => {
  if (Array.isArray(node)) return node.forEach(visit);
  if (node && typeof node === 'object') {
    for (const [k, v] of Object.entries(node)) {
      if (k === '$ref' && typeof v === 'string') {
        const m = /^#\/components\/schemas\/(.+)$/.exec(v);
        if (m && !keep.has(m[1])) {
          keep.add(m[1]);
          visit(schemas[m[1]]);
        }
      } else visit(v);
    }
  }
};
visit(paths);
// Components that exist on main but are EXTENDED in the superset (extra
// fields/variants the pending routes or screens rely on). The pending copy
// shadows main's in the merged `Schemas` barrel (src/api/types.ts).
const extended = Object.keys(schemas).filter(
  (k) => k in main.components.schemas && JSON.stringify(schemas[k]) !== JSON.stringify(main.components.schemas[k]),
);
for (const k of extended) {
  keep.add(k);
  visit(schemas[k]);
}

const outSchemas = {};
for (const k of [...keep].sort()) outSchemas[k] = schemas[k];
const filtered = {
  openapi: sup.openapi,
  info: { title: `${sup.info?.title ?? 'api'} (pending-backend subset)`, version: sup.info?.version ?? '0' },
  paths,
  components: { schemas: outSchemas, securitySchemes: sup.components.securitySchemes },
};

const dir = mkdtempSync(join(tmpdir(), 'pending-api-'));
try {
  const tmp = join(dir, 'filtered.json');
  writeFileSync(tmp, JSON.stringify(filtered));
  const out = resolve(here, '../src/api/pending/schema.pending.ts');
  const r = spawnSync('npx', ['openapi-typescript', tmp, '-o', out], { stdio: 'inherit', cwd: resolve(here, '..') });
  if (r.status !== 0) process.exit(r.status ?? 1);
  const header = `/**
 * PENDING-BACKEND TYPES — DO NOT EDIT BY HAND.
 *
 * Types for routes that are NOT yet served by main. Generated from
 * scripts/pending-openapi.superset.json by scripts/gen-pending-api.mjs
 * (npm run generate:pending-api). DELETE this file and regenerate schema.ts
 * once the backend PRs listed in pending/BACKEND_DEPENDENCIES.md merge.
 */
`;
  writeFileSync(out, header + rd(out, 'utf8'));
  const names = Object.keys(outSchemas);
  const mainNames = new Set(Object.keys(main.components.schemas));
  process.stdout.write(
    `pending paths: ${Object.keys(paths).length}; components: ${names.length} (${names.filter((n) => !mainNames.has(n)).length} absent from main)\n`,
  );
} finally {
  rmSync(dir, { recursive: true, force: true });
}
