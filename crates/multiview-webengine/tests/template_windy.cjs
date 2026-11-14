'use strict';
// Run: node --test crates/multiview-webengine/tests/template_windy.cjs (driven by tests/template_windy.rs)
const test = require('node:test');
const assert = require('node:assert');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const t = require(path.join(__dirname, '..', 'src', 'template_windy.cjs'));

// Minimal fake DOM: elements are {sel, rect, visibility, opacity, display}; querySelectorAll
// answers the guard's "<sel>,<sel> *" query by matching the element's own selector list.
const el = (sels, o = {}) => ({sels, rect: {left: 600, top: 20, right: 650, bottom: 60, width: 50, height: 40}, visibility: 'visible', opacity: '1', display: 'block', ...o});
function run(expression, elements, view = {w: 1280, h: 720}, head = []) {
  const doc = {
    head: {get lastElementChild() { return head[head.length - 1] || null; }, appendChild(n) { head.push(n); }},
    documentElement: {appendChild() {}},
    getElementById: id => head.find(n => n.id === id) || null,
    createElement: () => ({id: '', textContent: '', isConnected: true, remove() { head.splice(head.indexOf(this), 1); }}),
    querySelectorAll: q => {
      const sel = q.split(',')[0];
      return elements.filter(e => e.sels.includes(sel)).map(e => ({...e, getBoundingClientRect: () => e.rect}));
    },
  };
  const ctx = {document: doc, innerWidth: view.w, innerHeight: view.h, getComputedStyle: e => e};
  return vm.runInNewContext(expression, ctx);
}

test('CLEAN hides the hamburger menu icon, its bars and the mobile UI, children included', () => {
  for (const s of ['.rhitem__hamburger', '[class*="hamburger"]', '.mobile-ui__hamburger-icon', '.mobile-ui', '#plugin-search-input'])
    assert.ok(t.CHROME_SELECTORS.includes(s), s);
  assert.match(t.CLEAN, /\.rhitem__hamburger \*/);
  assert.match(t.CLEAN, /\[class\*="hamburger"\] \*/);
  assert.match(t.CLEAN, /\{visibility:hidden!important\}$/);
});

test('guard reports visible chrome and ignores hidden, off-screen, empty or transparent chrome', () => {
  const ok = [
    el(['.rhitem__hamburger'], {visibility: 'hidden'}),
    el(['#logo'], {rect: {left: 0, top: 0, right: 0, bottom: 0, width: 0, height: 0}}),
    el(['#search'], {rect: {left: 1300, top: 20, right: 1350, bottom: 60, width: 50, height: 40}}),
    el(['.timecode'], {opacity: '0'}),
    el(['.play-pause'], {display: 'none'}),
    el(['.not-chrome']),
  ];
  assert.strictEqual(run(t.CHROME_VISIBLE_EXPRESSION, ok), '');
  const bad = run(t.CHROME_VISIBLE_EXPRESSION, [...ok, el(['.rhitem__hamburger']), el(['#logo'])]);
  assert.deepStrictEqual(bad.split(','), ['#logo', '.rhitem__hamburger']);
});

test('ensure-clean style is idempotent and re-created if Windy removed it', () => {
  const head = [];
  run(t.ENSURE_CLEAN_EXPRESSION, [], undefined, head);
  run(t.ENSURE_CLEAN_EXPRESSION, [], undefined, head);
  assert.strictEqual(head.length, 1);
  assert.strictEqual(head[0].textContent, t.CLEAN);
  head.length = 0;
  run(t.ENSURE_CLEAN_EXPRESSION, [], undefined, head);
  assert.strictEqual(head.length, 1);
});

test('host wires the guard and registers its safe error', () => {
  const host = fs.readFileSync(path.join(__dirname, '..', 'src', 'browser_host.cjs'), 'utf8');
  assert.match(host, /'Windy page chrome visible in capture'\s*,/);
  assert.match(host, /CHROME_VISIBLE_EXPRESSION/);
  assert.match(host, /await bounded\(assertClean\(\)\);\s*\n\s*const shot = await bounded\(screenshot\(\)\)/);
});
