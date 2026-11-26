import test from 'node:test';
import assert from 'node:assert/strict';
import { Gateway, readPairingToken } from '../src/gateway.mjs';
import { createRequest, MAX_FRAME_BYTES } from '../src/protocol.mjs';
import { response, eventPage, observation, GENERATION, capabilities } from './fixtures.mjs';

const json = (body, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });

test('a foreground selector may share a saved session label without changing the older row', async () => {
  const sessions = [{ name: 'work' }, { name: 'work', target: 'live:device-session' }];
  const gateway = new Gateway({ fetchImpl: async () => json({ sessions, has_more: false }) });
  gateway.setToken('fixture-token');
  assert.deepEqual((await gateway.sessions()).sessions, sessions);
  sessions[1].target = '';
  await assert.rejects(gateway.sessions(), /连接目标无效/);
});

test('private pairing files are bounded and accepted only for the exact page origin', async () => {
  const origin = 'http://127.0.0.1:8787', token = 'ad'.repeat(32);
  const pairing = { schema: 'ssmcp-gui-pairing-v1', token, url: origin };
  const file = fields => new Blob([JSON.stringify({ ...pairing, ...fields })]);
  assert.equal(await readPairingToken(file({}), origin), token);
  assert.equal(await readPairingToken(file({ url: origin + '/' }), origin), token);
  for (const url of ['http://localhost:8787', 'http://127.0.0.1:8788', 'https://127.0.0.1:8787',
    origin + '/other', origin + '/?redirect', origin + '/#token', 'http://x@127.0.0.1:8787']) {
    await assert.rejects(readPairingToken(file({ url }), origin), /地址不一致/);
  }
  for (const fields of [{ schema: 'unknown' }, { token: 'bad' }, { url: 42 }]) {
    await assert.rejects(readPairingToken(file(fields), origin), /格式无效/);
  }
  await assert.rejects(readPairingToken(new Blob(['not JSON']), origin), /无法读取/);
  await assert.rejects(readPairingToken({ size: 9000, text() { assert.fail('oversized file read'); } }, origin), /过大/);
});

test('pairing token is sent only in Authorization and never URL, body or serializable transport state', async () => {
  const calls = [];
  const gateway = new Gateway({ fetchImpl: async (url, options) => { calls.push({ url, options }); return json({ sessions: [], next_offset: null, has_more: false }); } });
  const token = 'test-private-pairing-value'; gateway.setToken(token); await gateway.sessions();
  assert.equal(calls[0].options.headers.Authorization, `Bearer ${token}`);
  assert.equal(calls[0].url, '/api/gui/v1/sessions?offset=0&limit=16');
  assert.equal(calls[0].options.redirect, 'error');
  assert.equal(calls[0].options.body, undefined);
  assert.equal(JSON.stringify(gateway).includes(token), false);
  gateway.clearToken(); await assert.rejects(gateway.sessions(), { code: 'authentication_required' });
  assert.equal(calls.length, 1);
  assert.throws(() => new Gateway({ prefix: 'https://untrusted.example/api' }));
  assert.throws(() => new Gateway({ prefix: '//untrusted.example' }));
});

test('command transport failure remains unknown with original request ID and exactly one dispatch', async () => {
  let attempts = 0;
  const gateway = new Gateway({ fetchImpl: async () => { attempts += 1; throw new TypeError('Network failed'); } });
  gateway.setToken('pairing');
  const request = createRequest({ op: 'command', generation: GENERATION, command: 'terminal.write', arguments: { terminal_id: 'main', data: 'hello\n' } }, 'one.explicit.write');
  await assert.rejects(gateway.bridge('conn', request), error => error.effect === 'may_have_occurred' && error.requestId === request.id);
  assert.equal(attempts, 1);
});

test('HTTP error preserves explicit effect and unexpected response body is unknown for a command', async () => {
  const request = createRequest({ op: 'command', generation: GENERATION, command: 'terminal.write', arguments: {} }, 'cmd');
  for (const [reply, effect] of [[json({ error: { code: 'denied', message: 'Lease denied.', effect: 'none' } }, 403), 'none'], [new Response('<html>bad gateway</html>', { status: 502 }), 'may_have_occurred']]) {
    const gateway = new Gateway({ fetchImpl: async () => reply }); gateway.setToken('pairing');
    await assert.rejects(gateway.bridge('conn', request), error => error.effect === effect && error.requestId === 'cmd');
  }
});

test('bounded streaming rejects oversized responses and does not leak input through errors', async () => {
  const gateway = new Gateway({ fetchImpl: async () => new Response(new ReadableStream({
    start(controller) { controller.enqueue(new TextEncoder().encode('x'.repeat(MAX_FRAME_BYTES + 1))); controller.close(); },
  })) });
  gateway.setToken('pairing');
  const request = createRequest({ op: 'command', generation: GENERATION, command: 'terminal.write', arguments: { data: 'secret command body' } }, 'big');
  await assert.rejects(gateway.bridge('conn', request), error => error.effect === 'may_have_occurred' && !error.message.includes('secret command body'));
});

test('gateway observations preserve decimal cursors and enforce a <=55s outer window', async () => {
  const sent = [];
  const gateway = new Gateway({ fetchImpl: async (_url, options) => {
    const request = JSON.parse(options.body); sent.push(request);
    return json(observation(request, eventPage({ start: '9007199254740993', next: '9007199254740993' })));
  } });
  gateway.setToken('pairing');
  const request = { request_id: 'watch', generation: GENERATION, cursor: '9007199254740993', limit: 16, wait_ms: 50000 };
  const result = await gateway.observe('conn', request);
  assert.equal(sent[0].cursor, request.cursor);
  assert.equal(result.page.next_cursor, request.cursor);
  await assert.rejects(gateway.observe('conn', { ...request, wait_ms: 55001 }));
  assert.equal(sent.length, 1);
});

test('observation abort remains a read-only abort and malformed open receipt remains unknown', async () => {
  const controller = new AbortController();
  const gateway = new Gateway({ fetchImpl: async (_url, options) => new Promise((_resolve, reject) => options.signal.addEventListener('abort', () => reject(new DOMException('Aborted', 'AbortError')), { once: true })) });
  gateway.setToken('pairing');
  const watching = gateway.observe('conn', { request_id: 'watch', generation: GENERATION, limit: 16, wait_ms: 50000 }, { signal: controller.signal });
  controller.abort(); await assert.rejects(watching, { name: 'AbortError' });
  const wrong = new Gateway({ fetchImpl: async () => json({ status: 'connected', connection_id: '../wrong', session: 'actual', read_only: true, duplicate: false }) });
  wrong.setToken('pairing');
  await assert.rejects(wrong.open({ request_id: 'open.1', session: 'actual', read_only: true }), error => error.effect === 'may_have_occurred' && error.requestId === 'open.1');
});

test('bridge transport preserves the exact zbridge envelope without inventing HTTP command endpoints', async () => {
  const calls = [];
  const request = createRequest({ op: 'state', generation: GENERATION, resource: 'workers', query: { offset: 16, limit: 16 } }, 'workers.page');
  const gateway = new Gateway({ fetchImpl: async (url, options) => { calls.push([url, JSON.parse(options.body)]); return json(response(request, { resource: 'workers', schema_version: 1, value: { slots: [] } })); } });
  gateway.setToken('pairing'); await gateway.bridge('actual.connection', request);
  assert.equal(calls[0][0], '/api/gui/v1/connections/actual.connection/bridge');
  assert.deepEqual(calls[0][1], request);
});

test('connection capabilities correlate to the original open request ID', async () => {
  const open = { request_id: 'open.original', session: 'actual', read_only: true };
  for (const id of ['open.original', 'other.open']) {
    const gateway = new Gateway({ fetchImpl: async () => json({ status: 'connected', connection_id: 'conn', session: 'actual', read_only: true, duplicate: false,
      capabilities: response({ id, operation: { op: 'capabilities' } }, capabilities({ mode: 'observe', commands: [] })),
    }) }); gateway.setToken('pairing');
    if (id === open.request_id) assert.equal((await gateway.open(open)).connection_id, 'conn');
    else await assert.rejects(gateway.open(open), error => error.effect === 'may_have_occurred' && error.requestId === open.request_id);
  }
});

test('lost or malformed close acknowledgement is a possibly completed mutation', async () => {
  for (const fetchImpl of [async () => { throw new Error('Disconnected'); }, async () => json({ closed: true, session_stopped: true })]) {
    const gateway = new Gateway({ fetchImpl }); gateway.setToken('pairing');
    await assert.rejects(gateway.close('original.connection'), error => error.effect === 'may_have_occurred');
  }
});
