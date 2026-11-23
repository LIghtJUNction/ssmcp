import test from 'node:test';
import assert from 'node:assert/strict';
import { abortableDelay } from '../src/client.mjs';
import { GatewayError } from '../src/gateway.mjs';
import { harness, capabilities, response, observation, eventPage, deferred, GENERATION } from './fixtures.mjs';

test('opening negotiates a fresh correlated capabilities response before real state reads', async () => {
  const { client, calls } = harness({ seed: false, caps: capabilities({ commands: [], mode: 'observe' }) });
  await client.connect('real-session', true);
  assert.equal(calls[0][1].session, 'real-session');
  assert.equal(calls[1][1].operation.op, 'capabilities');
  assert.equal(calls[2][1].operation.resource, 'runtime');
  assert.equal(client.cursor, '10');
  assert.equal(client.status, 'connected');
  assert.equal(calls.filter(call => call[0] === 'bridge' && call[1].operation.op === 'command').length, 0);
});

test('retention gap waits for authoritative state and never advances cursor on snapshot failure', async () => {
  const entered = deferred(), snapshot = deferred();
  let snapshotRequest;
  const { client } = harness({
    bridge: request => { if (request.operation.op === 'state') { snapshotRequest = request; entered.resolve(); return snapshot.promise; } },
    observe: request => observation(request, eventPage({ start: '12', next: '13', events: [{ sequence: '12', topic: 'ssmcp.event', value: { kind: 'event', value: { type: 'ssmcp.message', kind: 'answer', text: 'actual' } } }], gap: true })),
  });
  client.subscribe(event => { if (event.type === 'events') void client.pauseObservation(); });
  const waiting = client.startObservation(); await entered.promise;
  assert.equal(client.cursor, '10');
  assert.equal(client.observation, 'resyncing');
  snapshot.resolve(response(snapshotRequest, { resource: 'runtime', schema_version: 1, value: { live_cursor: '13', backend_active: true } }));
  await waiting;
  assert.equal(client.cursor, '13');
  assert.equal(client.timeline.rows.some(row => row.sequence === '12'), true);

  let failed;
  const failureHarness = harness({
    bridge: request => { if (request.operation.op === 'state') throw new Error('Snapshot unavailable'); },
    observe: request => observation(request, eventPage({ start: '12', next: '12', gap: true })),
    sleep: async () => { void failed.pauseObservation(); },
  });
  failed = failureHarness.client;
  await failed.startObservation();
  assert.equal(failed.cursor, '10');
  assert.equal(failed.timeline.rows.length, 0);
});

test('lost command response is unknown, retains original ID and is never retried by refresh', async () => {
  const { client, calls } = harness({ bridge: request => {
    if (request.operation.op === 'command') throw new GatewayError('Lost receipt.', { effect: 'may_have_occurred', requestId: request.id });
  } });
  const record = await client.command('terminal.write', { terminal_id: 'term.1', data: 'one write\n' });
  assert.equal(record.status, 'unknown');
  assert.match(record.requestId, /^command_/);
  await client.refresh(); await client.refresh();
  const commands = calls.filter(call => call[0] === 'bridge' && call[1].operation.op === 'command');
  assert.equal(commands.length, 1);
  assert.equal(commands[0][1].id, record.requestId);
  assert.equal(client.ledger.records[0].status, 'unknown');
});

test('mismatched command response and native unknown are never reported as success', async () => {
  for (const mode of ['wrong-id', 'native-unknown']) {
    const { client } = harness({ bridge: request => {
      if (request.operation.op !== 'command') return;
      const reply = response(request, { command: request.operation.command, value: { status: mode === 'native-unknown' ? 'unknown' : 'done' } });
      if (mode === 'wrong-id') reply.id = 'other-command';
      return reply;
    } });
    const record = await client.command('terminal.write', { terminal_id: 'term.1', data: 'x' });
    assert.equal(record.status, 'unknown');
  }
});

test('native uncertain status or effect keeps the original command unknown across refreshes', async () => {
  for (const command of ['chat.send', 'terminal.write']) {
    for (const patch of [{ status: 'uncertain' }, { effect: 'may_have_occurred' }]) {
      const { client, calls } = harness({ caps: capabilities({ commands: [command] }), bridge: request => {
        if (request.operation.op !== 'command') return;
        return response(request, { command, value: {
          request_id: request.id, message_id: 'actual-message', target: 'next', duplicate: false,
          status: 'sending', confirmation: 'local_queue_admission', ...patch,
        } });
      } });
      const record = await client.command(command, command === 'chat.send'
        ? { text: 'retain this draft', target: 'next' } : { terminal_id: 'term.1', data: 'one write\n' });
      assert.equal(record.status, 'unknown');
      assert.equal(record.error.effect, 'may_have_occurred');
      assert.equal(record.receipt.request_id, record.requestId);
      await client.refresh(); await client.refresh();
      const commands = calls.filter(call => call[0] === 'bridge' && call[1].operation.op === 'command');
      assert.equal(commands.length, 1);
      assert.equal(commands[0][1].id, record.requestId);
      assert.equal(client.ledger.records[0].status, 'unknown');
    }
  }
});

test('verified no-effect rejection stays rejected and does not rebind to the error generation', async () => {
  const { client } = harness({ bridge: request => {
    if (request.operation.op === 'command') return response(request, { code: 'stale_generation', message: 'Changed.', effect: 'none' }, 'generation.other', 'error');
  } });
  const record = await client.command('terminal.write', { terminal_id: 'term.1', data: 'x' });
  assert.equal(record.status, 'rejected');
  assert.equal(record.generation, GENERATION);
  assert.equal(client.generation, GENERATION);
});

test('aborting observation leaves a concurrent command alive and releases no session', async () => {
  const native = deferred(), observed = deferred();
  let commandSignal = 'not-called';
  const { client, calls } = harness({
    bridge: (request, options) => {
      if (request.operation.op !== 'command') return;
      commandSignal = options?.signal;
      return native.promise.then(() => response(request, { command: request.operation.command, value: { received: true } }));
    },
    observe: (_request, { signal }) => { observed.resolve(); return new Promise((_resolve, reject) => signal.addEventListener('abort', () => reject(new DOMException('Paused', 'AbortError')), { once: true })); },
  });
  const command = client.command('terminal.write', { terminal_id: 'term.1', data: 'x' });
  const watching = client.startObservation(); await observed.promise;
  await client.pauseObservation(); await watching;
  assert.equal(commandSignal, undefined);
  assert.equal(client.ledger.records[0].status, 'sending');
  assert.equal(calls.some(call => call[0] === 'close'), false);
  native.resolve(); assert.equal((await command).status, 'confirmed');
});

test('abort then resume observes the exact unchanged large cursor with fresh read IDs', async () => {
  const entered = deferred(); let attempt = 0;
  const { client, calls } = harness({ observe: (request, { signal }) => {
    attempt += 1;
    if (attempt === 1) { entered.resolve(); return new Promise((_resolve, reject) => signal.addEventListener('abort', () => reject(new DOMException('Paused', 'AbortError')), { once: true })); }
    return observation(request, eventPage({ start: '9007199254740993', next: '9007199254740993' }));
  } });
  client.cursor = '9007199254740993';
  const first = client.startObservation(); await entered.promise; await client.pauseObservation(); await first;
  client.subscribe(event => { if (event.type === 'events') void client.pauseObservation(); });
  await client.startObservation();
  const observations = calls.filter(call => call[0] === 'observe').map(call => call[1]);
  assert.equal(observations.length, 2);
  assert.equal(observations[0].cursor, observations[1].cursor);
  assert.notEqual(observations[0].request_id, observations[1].request_id);
  assert.ok(observations.every(item => item.wait_ms <= 55000));
});

test('read failures use bounded exponential backoff without mutation replay', async () => {
  const delays = []; let client;
  const fixture = harness({ observe: () => { throw new Error('Offline'); }, sleep: async ms => { delays.push(ms); if (delays.length === 7) void client.pauseObservation(); } });
  client = fixture.client; await client.startObservation();
  assert.deepEqual(delays, [1000, 2000, 4000, 8000, 16000, 30000, 30000]);
  assert.equal(fixture.calls.some(call => call[0] === 'bridge' && call[1].operation.op === 'command'), false);
});

test('later resource pages supersede delayed reads without moving the view backwards', async () => {
  const pending = [];
  const { client } = harness({ caps: capabilities({ resources: ['workers'] }), bridge: request => {
    if (request.operation.op === 'state') { const promise = deferred(); pending.push({ request, ...promise }); return promise.promise; }
  } });
  const old = client.readState('workers', { offset: 0, limit: 16 });
  const newer = client.readState('workers', { offset: 16, limit: 16 });
  pending[1].resolve(response(pending[1].request, { resource: 'workers', schema_version: 1, value: { offset: 16, slots: [] } }));
  await newer;
  pending[0].resolve(response(pending[0].request, { resource: 'workers', schema_version: 1, value: { offset: 0, slots: [] } }));
  await assert.rejects(old, { name: 'AbortError' });
  assert.equal(client.states.get('workers').offset, 16);
});

test('chat.send owns one immutable explicit submission ID and confirms queue admission only', async () => {
  const { client, calls } = harness({ caps: capabilities({ commands: ['chat.send'] }), bridge: request => {
    if (request.operation.op === 'command') return response(request, { command: 'chat.send', value: {
      request_id: request.operation.arguments.request_id, message_id: 'actual-message', status: 'queued', target: 'next', duplicate: false, confirmation: 'local_queue_admission',
    } });
  } });
  const record = await client.command('chat.send', { text: '真实输入', target: 'next' });
  const request = calls.find(call => call[0] === 'bridge' && call[1].operation.op === 'command')[1];
  assert.equal(request.operation.arguments.request_id, request.id);
  assert.equal(record.receipt.confirmation, 'local_queue_admission');
  assert.equal(record.receipt.status, 'queued');
  assert.equal(client.timeline.rows.some(row => row.text.includes('模型已读')), false);
});

test('abortable backoff cancels promptly without waiting for a timer', async () => {
  const controller = new AbortController();
  const pending = abortableDelay(30000, controller.signal); controller.abort();
  await assert.rejects(pending, { name: 'AbortError' });
});

test('worker task snapshots appear once in the conversation and disappearance never implies completion', async () => {
  let slots = [{ slot: 0, task_id: 'real-task', task_summary: 'Actual assignment', state: 'working', progress: 'Native progress' }];
  const { client } = harness({ caps: capabilities({ resources: ['workers'] }), bridge: request => {
    if (request.operation.op === 'state') return response(request, { resource: 'workers', schema_version: 1, value: { pool_generation: 'actual.pool', slots } });
  } });
  await client.readState('workers'); await client.readState('workers');
  assert.equal(client.timeline.rows.length, 1);
  assert.equal(client.timeline.rows[0].kind, 'worker_snapshot');
  assert.equal(client.timeline.rows[0].text, 'Actual assignment\nNative progress');
  slots = []; await client.readState('workers');
  assert.equal(client.timeline.rows[0].status, 'working');
  assert.equal(client.timeline.rows[0].metadata.snapshot, true);
});

test('chat admission with another inner request ID stays unknown', async () => {
  const { client } = harness({ caps: capabilities({ commands: ['chat.send'] }), bridge: request => {
    if (request.operation.op === 'command') return response(request, { command: 'chat.send', value: {
      request_id: 'other', message_id: 'actual-message', target: 'next', duplicate: false, confirmation: 'local_queue_admission',
    } });
  } });
  assert.equal((await client.command('chat.send', { text: 'x', target: 'next' })).status, 'unknown');
});

test('a verified gap page survives failed reconciliation and pause without refetch or duplicate consumption', async () => {
  let snapshotAvailable = false, client;
  const fixture = harness({
    bridge: request => {
      if (request.operation.op !== 'state') return;
      if (!snapshotAvailable) throw new Error('Authoritative snapshot unavailable');
      return response(request, { resource: 'runtime', schema_version: 1, value: { live_cursor: '100', backend_active: true } });
    },
    observe: request => observation(request, eventPage({ start: '12', next: '13', gap: true,
      events: [{ sequence: '12', topic: 'ssmcp.event', value: { kind: 'event', value: { type: 'ssmcp.message', text: 'already-received event' } } }] })),
    sleep: async () => { if (!snapshotAvailable) void client.pauseObservation(); },
  });
  client = fixture.client;
  await client.startObservation();
  assert.equal(client.cursor, '10'); assert.equal(client.pendingObservation.page.events[0].sequence, '12');
  snapshotAvailable = true;
  client.subscribe(event => { if (event.type === 'events') void client.pauseObservation(); });
  await client.startObservation();
  assert.equal(client.cursor, '13'); assert.equal(client.pendingObservation, null);
  assert.equal(fixture.calls.filter(call => call[0] === 'observe').length, 1);
  assert.equal(client.timeline.rows.filter(row => row.sequence === '12').length, 1);
});

test('state schema mismatch never replaces a supported snapshot or grants a future resource', async () => {
  const { client } = harness({ bridge: request => {
    if (request.operation.op === 'state') return response(request, { resource: 'runtime', schema_version: 2, value: { live_cursor: '100' } });
  } });
  client.states.set('runtime', { live_cursor: '10' });
  await assert.rejects(client.readState('runtime'), { code: 'invalid_response' });
  assert.equal(client.states.get('runtime').live_cursor, '10');
  client.capabilities.resources[0].schema_version = 2;
  assert.equal(client.hasResource('runtime'), false);
  client.connection.read_only = true;
  assert.equal(client.hasCommand('terminal.write'), false);
});

test('inner chat target and confirmation mismatch or uncertain native status retain unknown', async () => {
  for (const patch of [{ target: 'another-agent' }, { confirmation: 'model_read' }, { uncertain: true }, { status: 'unknown' }, { message_id: '' }]) {
    const { client } = harness({ caps: capabilities({ commands: ['chat.send'] }), bridge: request => {
      if (request.operation.op === 'command') return response(request, { command: 'chat.send', value: {
        request_id: request.id, message_id: 'actual-message', target: 'next', duplicate: false,
        status: 'sending', confirmation: 'local_queue_admission', ...patch,
      } });
    } });
    const result = await client.command('chat.send', { text: 'actual input' });
    assert.equal(result.status, 'unknown');
  }
});

test('unverified open attempts and close outcomes retain their original connection context', async () => {
  const { client, transport } = harness({ seed: false });
  transport.open = async () => { throw new GatewayError('No open receipt.', { effect: 'may_have_occurred' }); };
  await assert.rejects(client.connect('first')); await assert.rejects(client.connect('second'));
  assert.equal(client.connectionRecords.length, 2);
  assert.deepEqual(client.connectionRecords.map(record => [record.session, record.status]), [['first', 'unknown'], ['second', 'unknown']]);
  assert.notEqual(client.connectionRecords[0].requestId, client.connectionRecords[1].requestId);
  client.connection = { connection_id: 'original.connection', session: 'first', read_only: true };
  transport.close = async () => { throw new GatewayError('No close receipt.', { effect: 'may_have_occurred' }); };
  await assert.rejects(client.disconnect());
  const close = client.connectionRecords.at(-1);
  assert.equal(close.status, 'unknown'); assert.equal(close.connectionId, 'original.connection');
});

test('one read-cycle deadline cancels only its own reads and cannot apply a late snapshot', async () => {
  const { client } = harness({ bridge: async request => {
    if (request.operation.op !== 'state') return;
    await new Promise(resolve => setTimeout(resolve, 15));
    return response(request, { resource: 'runtime', schema_version: 1, value: { live_cursor: '999' } });
  } });
  client.states.set('runtime', { live_cursor: '10' });
  await assert.rejects(client.readWindow(signal => client.readState('runtime', {}, { signal }), { maximumMs: 2 }), { code: 'read_window_timeout' });
  assert.equal(client.states.get('runtime').live_cursor, '10');
  assert.equal(client.reads.signal.aborted, false);
  assert.equal((await client.command('terminal.write', { terminal_id: 'main', data: 'explicit later operation' })).status, 'confirmed');
});

test('a newer worker page during gap reconciliation retries the held page without pausing observation', async () => {
  const firstWorkerRead = deferred(), originalWorkerReply = deferred();
  const delays = []; let workerReads = 0, originalWorkerRequest, recoveredWithActiveObserver = false;
  let client;
  const fixture = harness({
    caps: capabilities({ resources: ['runtime', 'workers'] }),
    bridge: request => {
      if (request.operation.op !== 'state' || request.operation.resource !== 'workers') return;
      workerReads += 1;
      if (workerReads === 1) {
        originalWorkerRequest = request; firstWorkerRead.resolve(); return originalWorkerReply.promise;
      }
      if (workerReads === 3) recoveredWithActiveObserver = client.observer?.signal.aborted === false;
      return response(request, { resource: 'workers', schema_version: 1,
        value: { offset: request.operation.query.offset, slots: [], pool_generation: 'actual.pool' } });
    },
    observe: request => observation(request, eventPage({ start: '12', next: '13', gap: true,
      events: [{ sequence: '12', topic: 'ssmcp.event', value: { kind: 'event', value: { type: 'ssmcp.message', text: 'held event' } } }] })),
    sleep: async (ms, signal) => {
      if (signal?.aborted) throw new DOMException('Explicitly paused after consumption', 'AbortError');
      delays.push(ms);
    },
  });
  client = fixture.client;
  client.subscribe(event => { if (event.type === 'events') void client.pauseObservation(); });
  const watching = client.startObservation(); await firstWorkerRead.promise;
  await client.readState('workers', { offset: 16, limit: 16 });
  originalWorkerReply.resolve(response(originalWorkerRequest, { resource: 'workers', schema_version: 1,
    value: { offset: 0, slots: [], pool_generation: 'actual.pool' } }));
  await watching;
  assert.equal(recoveredWithActiveObserver, true);
  assert.equal(workerReads, 3);
  assert.equal(delays[0], 1000);
  assert.equal(client.states.get('workers').offset, 16);
  assert.equal(client.cursor, '13'); assert.equal(client.pendingObservation, null);
  assert.equal(fixture.calls.filter(call => call[0] === 'observe').length, 1);
  assert.equal(client.timeline.rows.filter(row => row.sequence === '12').length, 1);
});

test('observation and automatic gap reconciliation share one absolute 55s deadline', async () => {
  let clock = 0, reads = 0, client;
  const expired = [];
  const fixture = harness({
    observe: request => {
      clock = 50_000;
      return observation(request, eventPage({ start: '12', next: '13', gap: true,
        events: [{ sequence: '12', topic: 'ssmcp.event', value: { kind: 'event', value: { type: 'ssmcp.message', text: 'retained across deadline' } } }] }));
    },
    bridge: request => {
      if (request.operation.op !== 'state') return;
      reads += 1;
      // Six seconds of reconciliation after a 50s observation must exceed the
      // original deadline, even though a fresh 55s state window would succeed.
      clock = reads === 1 ? 56_000 : 60_010;
      return response(request, { resource: 'runtime', schema_version: 1, value: { live_cursor: '13', backend_active: true } });
    },
    sleep: async (_ms, signal) => {
      if (signal?.aborted) throw new DOMException('Paused after consumption', 'AbortError');
      expired.push({ cursor: client.cursor, sequence: client.pendingObservation?.page.events[0]?.sequence,
        applied: client.states.has('runtime'), code: client.error?.code });
      clock = 60_000;
    },
  });
  client = fixture.client; client.clock = () => clock;
  client.subscribe(event => { if (event.type === 'events') void client.pauseObservation(); });
  await client.startObservation();
  assert.deepEqual(expired, [{ cursor: '10', sequence: '12', applied: false, code: 'read_window_timeout' }]);
  assert.equal(reads, 2);
  assert.equal(fixture.calls.filter(call => call[0] === 'observe').length, 1);
  assert.equal(client.cursor, '13'); assert.equal(client.pendingObservation, null);
  assert.equal(client.timeline.rows.filter(row => row.sequence === '12').length, 1);
});

test('a page received at the outer deadline is retained before any cursor advancement', async () => {
  let clock = 0, client;
  const fixture = harness({
    observe: request => {
      clock = 55_000;
      return observation(request, eventPage({ start: '0', next: '11', events: [{ sequence: '10', topic: 'ssmcp.event', value: {} }] }));
    },
    sleep: async () => { void client.pauseObservation(); },
  });
  client = fixture.client; client.clock = () => clock;
  await client.startObservation();
  assert.equal(client.cursor, '10');
  assert.equal(client.pendingObservation.page.next_cursor, '11');
  assert.equal(client.timeline.rows.length, 0);
});
