// Build out/summary.json from the RESULT lines of the test logs and print the key numbers.
//   node summarize.mjs <outDir> <name>=<log>[:expect-fail[=part]] ...
// expect-fail: the run documents a known failure (the production-shaped stream); it counts as a harness failure
// only if it unexpectedly PASSES. informational: recorded, never a failure. Exit code = number of failures.
import fs from 'node:fs';
import path from 'node:path';

const [outDir, ...specs] = process.argv.slice(2);
const entries = []; let failures = 0;
for (const spec of specs) {
  const [name, rest] = spec.split('='); const [log, flag, part] = rest.split(/:|=/);
  let result = null; let exit = null;
  try {
    const text = fs.readFileSync(log, 'utf8');
    const line = text.split('\n').filter((l) => l.startsWith('RESULT ')).pop();
    if (line) result = JSON.parse(line.slice(7));
    const m = /^exit=(\d+)$/m.exec(text); if (m) exit = Number(m[1]);
  } catch { /* missing log */ }
  const expectFail = flag === 'expect-fail';
  let pass = !!(result && result.pass && exit === 0);
  if (expectFail && result) { const p = part ? (result.parts || []).find((x) => x.name === part) : null; pass = p ? p.pass : result.pass; }
  const informational = flag === 'informational';
  const ok = informational ? true : (result ? (expectFail ? !pass : pass) : false);
  if (!ok) failures++;
  entries.push({ name, log: path.basename(log), expect: informational ? 'info' : (expectFail ? 'fail' : 'pass'), pass, ok, exit, failed: result ? result.failed : ['no RESULT line'], parts: result ? result.parts : [] });
}
// Stream health (stream-health.mjs): how far the reference stream ran behind real time before and after the tests.
const health = {};
for (const when of ['before', 'after']) { try { health[when] = JSON.parse(fs.readFileSync(path.join(outDir, `stream-health-${when}.json`), 'utf8')); } catch { /* not measured */ } }
const late = Object.values(health).some((h) => h && h.ok === false);
const summary = { at: new Date().toISOString(), failures, ok: failures === 0, stream_health: health, host_too_slow: late, entries };
fs.writeFileSync(path.join(outDir, 'summary.json'), JSON.stringify(summary, null, 1));

const KEYS = ['to_playing_ms', 'startup_buffering_ms', 'first_frame_ms', 'start_ms', 'advance_s', 'latency_min_s', 'latency_max_s', 'buffer_min_s', 'buffer_avg_s', 'seek_range_min_s', 'max_buffering_ms', 'max_waiting_ms', 'rebuffering_ms', 'stalls', 'frames_ratio'];
console.log('\n================ SUMMARY ================');
for (const e of entries) {
  console.log(`${e.ok ? 'OK  ' : 'BAD '} ${e.name}  (${e.expect === 'info' ? 'informational: ' + (e.pass ? 'pass' : 'fails') : e.expect === 'fail' ? 'documents a failure: ' + (e.pass ? 'UNEXPECTED PASS' : 'fails as expected') : (e.pass ? 'pass' : 'FAIL')})`);
  for (const p of e.parts || []) {
    const m = p.metrics || {}; const nums = KEYS.filter((k) => m[k] !== undefined && m[k] !== null).map((k) => `${k}=${m[k]}`).join(' ');
    console.log(`       ${p.pass ? 'pass' : 'FAIL'} ${p.informational ? '(info) ' : ''}${p.name}${nums ? ': ' + nums : ''}`);
    if (!p.pass) for (const f of (p.failed || []).slice(0, 4)) console.log(`            - ${String(f).slice(0, 150)}`);
  }
  if (!e.parts || !e.parts.length) for (const f of (e.failed || []).slice(0, 3)) console.log(`       - ${String(f).slice(0, 200)}`);
}
if (health.before || health.after) console.log(`\nstream lag behind real time: before ${JSON.stringify(health.before || null)} after ${JSON.stringify(health.after || null)}`);
if (late) console.log('HOST TOO SLOW: the reference stream ran behind real time (encoder starved), so failures above describe this host, not the player.');
// Note: an on-time stream does not prove the host is fast enough: with 2 cores the stream stayed on time while Chrome's software 1080p decode starved (see REPORT.md).
console.log(`\nSUMMARY ${JSON.stringify({ ok: summary.ok, failures, host_too_slow: late, file: path.join(outDir, 'summary.json') })}`);
process.exit(Math.min(failures, 100));
