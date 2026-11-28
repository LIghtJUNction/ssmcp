import test from 'node:test';
import assert from 'node:assert/strict';
import { Gateway, launchOptions, readPairingToken, takeLaunchPairing } from '../src/gateway.mjs';
import { createRequest, MAX_FRAME_BYTES } from '../src/protocol.mjs';
import { response, eventPage, observation, GENERATION, capabilities } from './fixtures.mjs';

const json = (body, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });

test('module owner pairing scrubs the fragment before accepting the exact local landing page', () => {
  const home = 'http://127.0.0.1:42387/', token = '1a'.repeat(32);
  for (const [url, modulePage, expected] of [
    [home, true, token], [home, false, null], [home + 'other', true, null],
    [home + '?other=1', true, null], ['http://127.0.0.1:42388/', true, null],
    ['http://localhost:42387/', true, null], ['https://elsewhere.example/', true, null],
  ]) {
    const location = new URL(url + '#' + token), scrubbed = [];
    const history = { replaceState: (_, __, clean) => scrubbed.push(clean) };
    assert.equal(takeLaunchPairing(location, history, modulePage), expected);
    assert.deepEqual(scrubbed, [url]);
  }
  const location = new URL(home + '#' + token);
  let replacement;
  location.replace = url => { replacement = url; };
  assert.equal(takeLaunchPairing(location, { replaceState() { throw new Error(); } }, true), null);
  assert.equal(replacement, home);
  for (const invalid of ['#' + token.toUpperCase(), '#not-a-module-secret']) {
    const clean = [];
    assert.equal(takeLaunchPairing(new URL(home + invalid), { replaceState: (_, __, url) => clean.push(url) }, true), null);
    assert.deepEqual(clean, [home]);
  }
});

test('CLI handoff pairs only a single explicit live selector on the loopback landing page', () => {
  const token = 'ab'.repeat(32), session = 'live:' + 'cd'.repeat(16);
  const query = '?session=' + encodeURIComponent(session);
  for (const [base, search, accepted] of [
    ['http://127.0.0.1:8011/', query, true],
    ['http://127.0.0.1:8011/', query + '&session=' + session, false],
    ['http://127.0.0.1:8011/other', query, false],
    ['http://127.0.0.1:8011/', '?session=saved:other', false],
    ['https://foreign.example/', query, false],
  ]) {
    const url = new URL(base + search + '#' + token), clean = [];
    assert.deepEqual(launchOptions(url), accepted ? {session} : null);
    assert.equal(takeLaunchPairing(url, {replaceState: (_, __, address) => clean.push(address)}, false), accepted ? token : null);
    assert.deepEqual(clean, [base + search]);
  }
});

test('desktop setup accepts only the exact informational refusal marker on loopback', () => {
  const token = 'ab'.repeat(32), base = 'http://127.0.0.1:8011/';
  for (const [query, expected] of [
    ['?setup=1&launch_refused=1', true], ['?launch_refused=1&setup=1', true],
    ['?launch_refused=1', false], ['?setup=1&launch_refused=0', false],
    ['?setup=1&launch_refused=1&relay=other.test', false], ['?setup=1&setup=1', false],
  ]) {
    const url = new URL(base + query + '#' + token);
    assert.deepEqual(launchOptions(url), expected ? {setup:true, refused:true} : null);
    assert.equal(takeLaunchPairing(url, {replaceState() {}}, false), expected ? token : null);
    assert.equal(takeLaunchPairing(url, {replaceState() {}}, true), null);
  }
});

test('a foreground selector may share a saved session label without changing the older row', async () => {
  const sessions = [{ name: 'work' }, { name: 'work', target: 'live:device-session' }];
  const gateway = new Gateway({ fetchImpl: async () => json({ sessions, has_more: false }) });
  gateway.setToken('fixture-token');
  assert.deepEqual((await gateway.sessions()).sessions, sessions);
  sessions[1].target = '';
  await assert.rejects(gateway.sessions(), /invalid target/);
});

test('private pairing files are bounded and accepted only for the exact page origin', async () => {
  const origin = 'http://127.0.0.1:8787', token = 'ad'.repeat(32);
  const pairing = { schema: 'ssmcp-gui-pairing-v1', token, url: origin };
  const file = fields => new Blob([JSON.stringify({ ...pairing, ...fields })]);
  assert.equal(await readPairingToken(file({}), origin), token);
  assert.equal(await readPairingToken(file({ url: origin + '/' }), origin), token);
  for (const url of ['http://localhost:8787', 'http://127.0.0.1:8788', 'https://127.0.0.1:8787',
    origin + '/other', origin + '/?redirect', origin + '/#token', 'http://x@127.0.0.1:8787']) {
    await assert.rejects(readPairingToken(file({ url }), origin), /different address/);
  }
  for (const fields of [{ schema: 'unknown' }, { token: 'bad' }, { url: 42 }]) {
    await assert.rejects(readPairingToken(file(fields), origin), /file is invalid/);
  }
  await assert.rejects(readPairingToken(new Blob(['not JSON']), origin), /Could not read/);
  await assert.rejects(readPairingToken({ size: 9000, text() { assert.fail('oversized file read'); } }, origin), /too large/);
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

test('owned creation binds the exact receipt and only retries the original explicit request', async () => {
  const bodies = [], target = 'live:' + '3a'.repeat(16);
  let respond = () => { throw new Error('connection lost'); };
  const gateway = new Gateway({ fetchImpl: async (path, init) => {
    assert.equal(path, '/api/gui/v1/owned-sessions');
    assert.equal(init.method, 'POST'); assert.equal(init.redirect, 'error');
    assert.equal(init.headers.Authorization, 'Bearer pair');
    const body = JSON.parse(init.body); bodies.push(body); return respond(body);
  }});
  const args = { destination: 'alice@relay.example', terms: false, privacy: false };
  await assert.rejects(gateway.createOwnedSession(args), /Pair with/);
  gateway.setToken('pair');
  await assert.rejects(gateway.createOwnedSession(args), {effect: 'may_have_occurred'});
  await assert.rejects(gateway.createOwnedSession({...args, privacy: true}), {code: 'pending'});
  assert.equal(bodies.length, 1);
  for (const invalid of [{session:target, request_id:'other'}, {session:'live:bad'},
    {session:target + '\n'}, {session:[target]}, {session:target.toUpperCase()}]) {
    respond = body => json({request_id:body.request_id, ...invalid});
    await assert.rejects(gateway.createOwnedSession(args), {effect:'may_have_occurred'});
  }
  respond = () => json({error:{code:'denied', effect:'none', message:'denied'}}, 403);
  await assert.rejects(gateway.createOwnedSession(args), {status:403});
  gateway.setToken('pair');
  respond = body => json({request_id:body.request_id, session:target});
  assert.equal(await gateway.createOwnedSession(args), target);
  assert.ok(bodies.every(body => JSON.stringify(body) === JSON.stringify(bodies[0])));
  assert.equal(gateway.pendingOwnedRequest, null);
  await gateway.createOwnedSession(args);
  assert.notEqual(bodies.at(-1).request_id, bodies[0].request_id);
});

test('first device sign-in shows the chosen relay and reuses the original creation after approval', async () => {
  const ids = [], target = 'live:' + 'ab'.repeat(16);
  const args = { destination:'alice@relay.example', terms:false, privacy:false };
  const challenge = {status:'pending', verification_url:'https://relay.example/setup?code=ABCD-1234', user_code:'ABCD-1234'};
  let pending = true;
  const gateway = new Gateway({fetchImpl: async (_path, {body}) => {
    const {request_id} = JSON.parse(body); ids.push(request_id);
    return json(pending ? {request_id, authorization:challenge} : {request_id, session:target});
  }});
  gateway.setToken('pairing');
  assert.equal(await gateway.createOwnedSession(args, (url, code) => {
    assert.equal(url, challenge.verification_url); assert.equal(code, challenge.user_code); pending = false;
  }), target);
  assert.deepEqual(ids, [ids[0], ids[0]]);
  pending = true; challenge.verification_url = 'https://other.example/setup?code=ABCD-1234';
  await assert.rejects(gateway.createOwnedSession(args, () => assert.fail('foreign link shown')), {code:'invalid_device_authorization'});
});

test('replacing pairing while an owned request is in flight discards its receipt', async () => {
  let respond;
  const gateway = new Gateway({fetchImpl: async (_path, {body}) => new Promise(resolve => {
    respond = () => resolve(json({request_id:JSON.parse(body).request_id, session:'live:' + 'ab'.repeat(16)}));
  })});
  gateway.setToken('original');
  const received = gateway.createOwnedSession({destination:'alice@local', terms:false, privacy:false});
  gateway.setToken('replacement'); respond();
  await assert.rejects(received, {code:'pairing_changed'});
  assert.equal(gateway.pendingOwnedRequest, null);
});

test('ending a session requires this pairing’s verified creation and an exact closed receipt', async () => {
  const target = 'live:' + 'a1'.repeat(16), other = 'live:' + 'b2'.repeat(16), deletes = [];
  let answer = () => { throw new Error('lost response'); };
  const gateway = new Gateway({fetchImpl: async (path, init) => {
    if (init.method === 'POST') return json({request_id:JSON.parse(init.body).request_id, session:target});
    deletes.push(path); assert.equal(init.method, 'DELETE'); assert.equal(init.headers.Authorization, 'Bearer pair');
    assert.equal(init.body, undefined); return answer();
  }});
  gateway.setToken('pair');
  await assert.rejects(gateway.endOwnedSession(target), {code:'not_owned'});
  await gateway.createOwnedSession({destination:'alice@local', terms:false, privacy:false});
  await assert.rejects(gateway.endOwnedSession(other), {code:'not_owned'});
  assert.equal(deletes.length, 0);
  await assert.rejects(gateway.endOwnedSession(target), {effect:'may_have_occurred'});
  assert.equal(gateway.ownedSessionStatus(target), 'unknown');
  for (const value of [{session:other, status:'closed'}, {session:target, status:'closing'}, {}]) {
    answer = () => json(value);
    await assert.rejects(gateway.endOwnedSession(target), {effect:'may_have_occurred'});
  }
  answer = () => json({error:{code:'denied',effect:'none',message:'denied'}}, 403);
  await assert.rejects(gateway.endOwnedSession(target), {status:403});
  assert.equal(gateway.ownedSessionStatus(target), 'unknown');
  answer = () => json({session:target, status:'closed'});
  await gateway.endOwnedSession(target);
  assert.equal(gateway.ownedSessionStatus(target), undefined);
  await assert.rejects(gateway.endOwnedSession(target), {code:'not_owned'});
  assert.deepEqual(deletes, Array(6).fill('/api/gui/v1/owned-sessions/' + encodeURIComponent(target)));
});

test('duplicate end and a late old-pairing receipt cannot affect a replacement’s session', async () => {
  const target = 'live:' + 'ac'.repeat(16), args = {destination:'a@host', terms:false, privacy:false};
  let release, deletes = 0;
  const gateway = new Gateway({fetchImpl: async (_path, init) => {
    if (init.method === 'POST') return json({request_id:JSON.parse(init.body).request_id, session:target});
    deletes++;
    return new Promise(resolve => { release = () => resolve(json({session:target, status:'closed'})); });
  }});
  gateway.setToken('first'); await gateway.createOwnedSession(args);
  const closing = gateway.endOwnedSession(target);
  assert.equal(gateway.ownedSessionStatus(target), 'closing');
  await assert.rejects(gateway.endOwnedSession(target), {code:'pending'});
  gateway.setToken('replacement');
  await assert.rejects(gateway.endOwnedSession(target), {code:'not_owned'});
  await gateway.createOwnedSession(args);
  release(); await assert.rejects(closing, {code:'pairing_changed'});
  assert.equal(deletes, 1); assert.equal(gateway.ownedSessionStatus(target), 'open');
  gateway.clearToken(); await assert.rejects(gateway.endOwnedSession(target), {code:'not_owned'});
});
