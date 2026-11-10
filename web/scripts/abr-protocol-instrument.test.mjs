import test from 'node:test';
import assert from 'node:assert/strict';
import { decodeControlMessages } from './abr-protocol-instrument.js';

test('draft16 SUBSCRIBE_OK exports only numeric request and track alias', () => {
  const wire = Uint8Array.of(4, 0, 4, 10, 12, 99, 98);
  const { messages, rest } = decodeControlMessages(wire);
  assert.equal(rest.length, 0);
  assert.deepEqual(messages, [{ type: 4, typeName: 'SUBSCRIBE_OK', size: 4, firstVarint: 10, bodyBytes: 4, trackAlias: 12 }]);
  assert.equal(JSON.stringify(messages).includes('99'), false);
});

test('truncated control headers and bodies remain unconsumed', () => {
  for (const wire of [Uint8Array.of(4), Uint8Array.of(4, 0), Uint8Array.of(4, 0, 2, 10)]) {
    const { messages, rest } = decodeControlMessages(wire);
    assert.deepEqual(messages, []);
    assert.deepEqual(rest, wire);
  }
});

test('multiple messages preserve separate numeric identities', () => {
  const { messages } = decodeControlMessages(Uint8Array.of(4, 0, 2, 8, 8, 4, 0, 2, 10, 10));
  assert.deepEqual(messages.map(({firstVarint, trackAlias}) => [firstVarint, trackAlias]), [[8,8], [10,10]]);
});
