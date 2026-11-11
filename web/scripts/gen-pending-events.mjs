#!/usr/bin/env node
/**
 * gen-pending-events.mjs — generate `src/realtime/pending/events.pending.ts`.
 *
 * Runs gen-events-types.mjs against the committed superset AsyncAPI
 * (scripts/pending-asyncapi.superset.json) and keeps ONLY the declarations
 * that are absent from, or differ from, main's generated-types.ts. main's
 * generated file is never edited. DELETE the output and regenerate
 * generated-types.ts once the backend PRs in
 * src/api/pending/BACKEND_DEPENDENCIES.md merge.
 */
import { readFileSync, writeFileSync, mkdirSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';

const here = dirname(fileURLToPath(import.meta.url));
const mainFile = readFileSync(resolve(here, '../src/realtime/generated-types.ts'), 'utf8');
const dir = mkdtempSync(join(tmpdir(), 'pending-events-'));
try {
  const tmp = join(dir, 'all.ts');
  const r = spawnSync(
    process.execPath,
    [resolve(here, 'gen-events-types.mjs'), resolve(here, 'pending-asyncapi.superset.json'), tmp],
    { stdio: ['ignore', 'ignore', 'inherit'] },
  );
  if (r.status !== 0) process.exit(r.status ?? 1);
  // Declarations: doc comment + export block, separated by blank lines.
  const split = (t) => t.split(/\n\n(?=\/\*\*|export )/);
  const mainBlocks = new Set(split(mainFile).map((b) => b.trim()));
  const blocks = split(readFileSync(tmp, 'utf8'));
  const kept = blocks.filter((b) => /(^|\n)export /.test(b) && !mainBlocks.has(b.trim()));
  const header = `/**
 * PENDING-BACKEND EVENT TYPES — DO NOT EDIT BY HAND.
 *
 * Types for realtime events that main's docs/api/asyncapi.json does NOT yet
 * declare (or declares narrower). Generated from
 * scripts/pending-asyncapi.superset.json by scripts/gen-pending-events.mjs
 * (npm run generate:pending-events). DELETE this file and regenerate
 * generated-types.ts once the backend PRs listed in
 * src/api/pending/BACKEND_DEPENDENCIES.md merge.
 */
`;
  const out = resolve(here, '../src/realtime/pending/events.pending.ts');
  mkdirSync(dirname(out), { recursive: true });
  writeFileSync(out, header + '\n' + kept.map((b) => b.trim()).join('\n\n') + '\n');
  process.stdout.write(`pending event declarations: ${kept.length}\n`);
} finally {
  rmSync(dir, { recursive: true, force: true });
}
