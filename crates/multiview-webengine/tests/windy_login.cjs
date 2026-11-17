'use strict';
// Run: node --test crates/multiview-webengine/tests/windy_login.cjs (driven by tests/windy_login.rs)
const test = require('node:test');
const assert = require('node:assert');
const path = require('node:path');
const {premiumStep} = require(path.join(__dirname, '..', 'src', 'template_windy.cjs'));

const loggedOut = {loginVisible: true, user: false, subscription: null, canvases: 3};
const premium = {loginVisible: false, user: true, subscription: '"premium"', canvases: 3};

test('logged-out page asks for a login, bounded by the login budget', () => {
  assert.strictEqual(premiumStep(loggedOut, 0), 'login');
  assert.strictEqual(premiumStep(loggedOut, 1), 'login');
  assert.strictEqual(premiumStep(loggedOut, 2), 'wait');
});

test('a Premium session is ready only with the subscription and no login icon', () => {
  assert.strictEqual(premiumStep(premium, 0), 'ready');
  assert.strictEqual(premiumStep({...premium, loginVisible: true}, 0), 'login');
  // Stale localStorage subscription with the login icon back = expired session.
  assert.strictEqual(premiumStep({...premium, loginVisible: true}, 2), 'wait');
});

test('wrong password is a typed rejection', () => {
  assert.strictEqual(premiumStep({...loggedOut, loginFailed: true}, 1), 'rejected');
});

test('logged in without Premium is reported, never treated as ready', () => {
  assert.strictEqual(premiumStep({loginVisible: false, user: true, subscription: null}, 1), 'not_premium');
  assert.strictEqual(premiumStep({loginVisible: false, user: true, subscription: '"free"'}, 1), 'not_premium');
});

test('expiry: a session that drops after being ready re-enters the login path', () => {
  let logins = 1;
  assert.strictEqual(premiumStep(premium, logins), 'ready');
  const expired = {loginVisible: true, user: false, subscription: null};
  assert.strictEqual(premiumStep(expired, logins), 'login');
  logins++;
  assert.strictEqual(premiumStep(premium, logins), 'ready');
  // During frame capture (budget exhausted) an expired session fails closed.
  assert.notStrictEqual(premiumStep(expired, Infinity), 'ready');
});

test('garbage or missing state never reads as ready', () => {
  for (const s of [null, undefined, 'x', {}, {loginVisible: false}, {subscription: 'premium'}])
    assert.notStrictEqual(premiumStep(s, 0), 'ready', JSON.stringify(s));
});
