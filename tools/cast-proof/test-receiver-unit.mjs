// Unit checks of receiver.js helpers that need no browser (#418 review ABUSE-3): the report size limit is counted in
// UTF-8 bytes, as the engine counts it, so non-ASCII track names cannot push every report over the limit.
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { check as mk, finish } from './lib.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const src = readFileSync(path.join(here, '../../crates/multiview-control/src/cast_receiver/receiver.js'), 'utf8');
const def = src.match(/function utf8Length\(text\) \{[\s\S]*?\n {2}\}/);
const checks = [];
const check = (name, pass, got) => checks.push(mk(name, !!pass, String(got), 'pass'));
check('utf8Length is defined in receiver.js', def, def ? 'found' : 'missing');
if (def) {
  const utf8Length = new Function(`${def[0]}; return utf8Length;`)();
  const samples = ['', 'abc', 'é', '中文', '😀', '\uD800x', 'x\uDC00', '中'.repeat(64), JSON.stringify({ name: '音声'.repeat(40) })];
  for (let i = 0; i < 200; i++) {
    let s = '';
    for (let j = 0; j < 40; j++) s += String.fromCharCode(Math.floor(Math.random() * 0xffff));
    samples.push(s);
  }
  const bad = samples.filter((s) => utf8Length(s) !== Buffer.byteLength(s, 'utf8'));
  check('utf8Length equals Buffer.byteLength on ASCII, BMP, astral, lone surrogates and random strings', bad.length === 0, `${bad.length} mismatch(es) of ${samples.length}`);
  check('the size checks in post() use utf8Length, never body.length', !/body\.length\s*>/.test(src) && /utf8Length\(body\) > MAX_REPORT_BODY/.test(src), 'source scan');
}
process.exit(finish('receiver-unit', [{ name: 'utf8', checks, metrics: {} }]) ? 1 : 0);
