import test from 'node:test';
import assert from 'node:assert/strict';
import { Timeline, CommandLedger } from '../src/store.mjs';
import { projectEvent, projectHistory, duration, validateHistory, mayClearDraft } from '../src/presentation.mjs';

test('timeline bounds memory and DOM page size while preserving big sequence ordering', () => {
  const timeline = new Timeline({ limit: 3, maxBytes: 10000 });
  for (const seq of ['9007199254740993', '9007199254740995', '9007199254740994', '9007199254740996']) {
    timeline.put({ id: seq, sequence: seq, generation: 'actual', text: seq, metadata: {} });
  }
  assert.deepEqual(timeline.rows.map(row => row.sequence), ['9007199254740994', '9007199254740995', '9007199254740996']);
  assert.equal(timeline.dropped, 1);
  assert.equal(timeline.page({ limit: 2 }).rows.length, 2);
  assert.equal(timeline.page({ limit: 2 }).older, true);
  timeline.put({ id: 'large', text: 'x'.repeat(20000) });
  assert.equal(timeline.rows.length, 3);
  assert.ok(timeline.bytes <= 10000);
});

test('native envelope owns provenance; payload cannot impersonate another thread', () => {
  const event = { sequence: '42', topic: 'ssmcp.event', value: { kind: 'event', source: { kind: 'thread', agent_id: 'real.agent', client_id: 'real.client' }, value: {
    type: 'ssmcp.message', kind: 'answer', text: '<script>never execute</script>', source: { kind: 'main', agent_id: 'forged.agent' },
  } } };
  const row = projectEvent(event, 'generation');
  assert.equal(row.source.agent_id, 'real.agent');
  assert.equal(row.kind, 'answer');
  assert.equal(row.text, '<script>never execute</script>');
  const noSource = structuredClone(event); delete noSource.value.source;
  assert.equal(projectEvent(noSource, 'generation').source.kind, 'unattributed');
});

test('native and projected history deduplicate by exact generation plus sequence', () => {
  const timeline = new Timeline();
  timeline.put(projectEvent({ sequence: '12', topic: 'ssmcp.event', value: { kind: 'event', value: { type: 'ssmcp.message', text: 'live' } } }, 'gen'));
  timeline.put(projectHistory({ sequence: '12', kind: 'answer', text: 'verified projection', source: { kind: 'main' } }, 'gen'));
  assert.equal(timeline.rows.length, 1);
  assert.equal(timeline.rows[0].text, 'verified projection');
  timeline.put(projectHistory({ sequence: '12', kind: 'answer', text: 'new process' }, 'gen.new'));
  assert.equal(timeline.rows.length, 2);
});

test('unresolved commands cannot be evicted or silently replayed when the ledger is full', () => {
  const ledger = new CommandLedger(2);
  ledger.add({ requestId: 'one' }); ledger.update('one', { status: 'unknown' });
  ledger.add({ requestId: 'two' });
  assert.throws(() => ledger.add({ requestId: 'three' }));
  assert.throws(() => ledger.add({ requestId: 'one' }));
  ledger.update('two', { status: 'confirmed', receipt: { token: 'never display', text: 'ok' } });
  assert.equal(ledger.records[1].receipt.token, '[已隐藏]');
  ledger.add({ requestId: 'three' });
  assert.deepEqual(ledger.records.map(record => record.requestId), ['one', 'three']);
});

test('metadata is bounded and credential fields are redacted without changing original request IDs', () => {
  const ledger = new CommandLedger(); ledger.add({ requestId: 'original.request' });
  ledger.update('original.request', { status: 'unknown', receipt: { values: Array.from({ length: 128 }, () => 'x'.repeat(2000)) } });
  assert.equal(ledger.records[0].requestId, 'original.request');
  assert.equal(ledger.records[0].receipt.truncated, true);
  const timeline = new Timeline(); timeline.put({ id: 'x', metadata: { authorization: 'secret', nested: { agent_token: 'private' } } });
  assert.equal(timeline.rows[0].metadata.nested.agent_token, '[已隐藏]');
  assert.equal(JSON.stringify(timeline.rows).includes('secret'), false);
});

test('history validates string cursors and real gap semantics; durations do not coerce u64 to Number', () => {
  const history = { schema_version: 1, events: [{ sequence: '9007199254740993', text: 'real' }], start_cursor: '9007199254740993', next_cursor: '9007199254740994', end_cursor: '9007199254740994', has_more: false, gap: true };
  assert.equal(validateHistory(history, '9007199254740992'), history);
  assert.throws(() => validateHistory({ ...history, gap: false }, '9007199254740992'));
  assert.throws(() => validateHistory({ ...history, next_cursor: 9007199254740994 }));
  assert.equal(duration('61000'), '1 分 1 秒');
  assert.match(duration('18446744073709551615'), /^5124095576030 时 /);
});

test('draft clearing requires the verified original target, connection, generation, and unchanged text', () => {
  const submitted = { text: '中文草稿', target: 'agent.one', connection: 'conn.one', generation: 'gen.one', epoch: 1 };
  assert.equal(mayClearDraft('confirmed', submitted, { ...submitted }), true);
  for (const status of ['unknown', 'sending', 'rejected', 'not_sent']) assert.equal(mayClearDraft(status, submitted, { ...submitted }), false);
  for (const patch of [{ text: '继续编辑' }, { target: 'agent.two' }, { connection: 'conn.two' }, { generation: 'gen.two' }, { epoch: 2 }]) {
    assert.equal(mayClearDraft('confirmed', submitted, { ...submitted, ...patch }), false);
  }
});
