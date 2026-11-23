import test from 'node:test';
import assert from 'node:assert/strict';
import { createRequest, validateResponse, unwrapResponse, validatePage, validateObservation, counter, compareCounters, BridgeError } from '../src/protocol.mjs';
import { capabilities, eventPage, response, GENERATION, observation } from './fixtures.mjs';

test('zbridge version, response ID, operation and opaque generation must correlate', () => {
  const request = createRequest({ op: 'state', generation: GENERATION, resource: 'runtime', query: {} }, 'state.1');
  const reply = response(request, { resource: 'runtime', schema_version: 1, value: {} });
  assert.equal(validateResponse(reply, request), reply);
  for (const altered of [{ ...reply, version: 2 }, { ...reply, id: 'another' }, { ...reply, generation: 'other.generation' },
    { ...reply, body: { kind: 'command', value: { command: 'terminal.write', value: {} } } },
    { ...reply, body: { kind: 'state', value: { resource: 'workers', schema_version: 1, value: {} } } }]) {
    assert.throws(() => validateResponse(altered, request));
  }
});

test('u64 cursor remains a canonical decimal string above Number precision', () => {
  assert.equal(counter('18446744073709551615'), '18446744073709551615');
  assert.equal(compareCounters('9007199254740993', '9007199254740992'), 1);
  for (const invalid of [9007199254740992, '00', '01', '-1', '1e3', '18446744073709551616', ' 1']) assert.throws(() => counter(invalid));
  const page = eventPage({ start: '9007199254740992', next: '9007199254740994', events: [{ sequence: '9007199254740993', topic: 'ssmcp.event', value: {} }] });
  assert.equal(validatePage(page, { cursor: '9007199254740993', limit: 16 }).next_cursor, '9007199254740994');
});

test('event pages reject replayed, out-of-order, future and inconsistent gap cursors', () => {
  const page = eventPage({ start: '10', next: '13', events: [{ sequence: '10', topic: 'ssmcp.event', value: {} }, { sequence: '12', topic: 'ssmcp.event', value: {} }] });
  assert.doesNotThrow(() => validatePage(page, { cursor: '10' }));
  assert.throws(() => validatePage(page, { cursor: '11' }));
  assert.throws(() => validatePage({ ...page, events: [...page.events].reverse() }));
  assert.throws(() => validatePage({ ...page, events: [{ ...page.events[0], sequence: '13' }] }));
  assert.throws(() => validatePage({ ...page, has_more: true }));
  assert.throws(() => validatePage(page, { cursor: '9' }));
  assert.doesNotThrow(() => validatePage({ ...page, retention_gap: true }, { cursor: '9' }));
});

test('v1 direct observations retain 1s limit; gateway envelope has explicit schema and correlation', () => {
  assert.throws(() => createRequest({ op: 'events', generation: GENERATION, wait_ms: 55000 }, 'read'));
  assert.doesNotThrow(() => createRequest({ op: 'events', generation: GENERATION, wait_ms: 1000 }, 'read'));
  const request = { request_id: 'observation.1', generation: GENERATION, cursor: '10', limit: 16, wait_ms: 50000 };
  const reply = observation(request, eventPage({ next: '10' }));
  assert.equal(validateObservation(reply, request).next_cursor, '10');
  assert.throws(() => validateObservation({ ...reply, request_id: 'old' }, request));
  assert.throws(() => validateObservation({ ...reply, schema: 'unknown' }, request));
  assert.throws(() => validateObservation({ ...reply, generation: 'old' }, request));
});

test('error envelope may describe a new generation but must preserve request identity and effect', () => {
  const request = createRequest({ op: 'command', generation: GENERATION, command: 'terminal.write', arguments: {} }, 'command.1');
  const reply = response(request, { code: 'stale_generation', message: 'Old generation.', effect: 'none' }, 'new.generation', 'error');
  assert.throws(() => unwrapResponse(reply, request), error => error instanceof BridgeError && error.responseGeneration === 'new.generation' && error.effect === 'none');
  assert.throws(() => validateResponse({ ...reply, id: 'command.2' }, request));
  const read = createRequest({ op: 'capabilities' }, 'caps');
  assert.throws(() => validateResponse(response(read, { code: 'internal', message: 'Bad read.', effect: 'may_have_occurred' }, null, 'error'), read));
});

test('schema mirrors deny_unknown_fields and rejects null object/scalar defaults', () => {
  assert.throws(() => createRequest({ op: 'state', generation: GENERATION, resource: 'runtime', query: null }, 'r'));
  assert.throws(() => createRequest({ op: 'command', generation: GENERATION, command: 'x', arguments: null }, 'r'));
  assert.throws(() => createRequest({ op: 'events', generation: GENERATION, limit: null }, 'r'));
  assert.throws(() => createRequest({ op: 'capabilities', pretend: true }, 'r'));
  const request = createRequest({ op: 'capabilities' }, 'r');
  assert.throws(() => validateResponse(response(request, capabilities({ mode: 'observe' })), request));
});
