import test from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { once } from 'node:events';

async function port() {
  const probe = http.createServer(); probe.listen(0, '127.0.0.1'); await once(probe, 'listening');
  const value = probe.address().port; await new Promise(resolve => probe.close(resolve)); return value;
}

test('same-origin server serves strict-CSP modules, forwards exact bridge requests, and preserves uncertain transport effects', async () => {
  const forwarded = [];
  const native = http.createServer(async (request, response) => {
    const chunks = []; for await (const chunk of request) chunks.push(chunk);
    const text = Buffer.concat(chunks).toString('utf8');
    forwarded.push({ path: request.url, authorization: request.headers.authorization, origin: request.headers.origin, text });
    if (request.url.endsWith('/bridge')) {
      if (JSON.parse(text).operation.op === 'command') { request.socket.destroy(); return; }
      response.setHeader('Content-Type', 'application/json'); response.end(text); return;
    }
    response.setHeader('Content-Type', 'application/json');
    response.end(JSON.stringify({ sessions: [], next_offset: 0, has_more: false }));
  });
  native.listen(0, '127.0.0.1'); await once(native, 'listening');
  const webPort = await port(); const nativeOrigin = `http://127.0.0.1:${native.address().port}`;
  const child = spawn(process.execPath, [fileURLToPath(new URL('../dev-server.mjs', import.meta.url)), '--port', String(webPort), '--gateway', nativeOrigin], { stdio: ['ignore', 'pipe', 'pipe'] });
  try {
    await new Promise((resolve, reject) => { child.stdout.once('data', resolve); child.once('error', reject); child.once('exit', code => reject(new Error(`Server exited ${code}`))); });
    const origin = `http://127.0.0.1:${webPort}`;
    const html = await fetch(origin); const source = await html.text();
    assert.equal(html.status, 200);
    assert.match(html.headers.get('content-security-policy'), /script-src 'self'; style-src 'self'; connect-src 'self'/);
    assert.equal(html.headers.get('content-security-policy').includes('unsafe-inline'), false);
    assert.match(source, /<script type="module" src="\.\/app.mjs"><\/script>/);
    assert.equal((await fetch(origin + '/styles.css')).headers.get('content-type'), 'text/css; charset=utf-8');
    assert.equal((await fetch(origin + '/src/client.mjs')).headers.get('content-type'), 'text/javascript; charset=utf-8');
    assert.equal((await fetch(origin + '/test/fixtures.mjs')).status, 404);
    assert.equal((await fetch(origin + '/dev-server.mjs')).status, 404);
    assert.equal((await fetch(origin + '/api/gui/v1/sessions', { headers: { Origin: 'https://elsewhere.invalid' } })).status, 403);
    assert.equal(forwarded.length, 0);
    const request = { version: 1, id: 'exact.fixture', operation: { op: 'state', generation: 'actual.gen', resource: 'runtime', query: {} } };
    const body = JSON.stringify(request);
    const reply = await fetch(origin + '/api/gui/v1/connections/actual/bridge', { method: 'POST', headers: { Authorization: 'Bearer fixture-secret', Origin: origin, 'Content-Type': 'application/json' }, body });
    assert.deepEqual(await reply.json(), request);
    assert.equal(forwarded[0].text, body);
    assert.equal(forwarded[0].authorization, 'Bearer fixture-secret');
    assert.equal(forwarded[0].origin, nativeOrigin);
    assert.equal(reply.headers.has('Access-Control-Allow-Origin'), false);
    const unknown = await fetch(origin + '/api/gui/v1/connections/actual/bridge', { method: 'POST', headers: { Authorization: 'Bearer fixture-secret', 'Content-Type': 'application/json' }, body: JSON.stringify({ ...request, operation: { op: 'command', generation: 'actual.gen', command: 'terminal.write', arguments: { data: 'fixture' } } }) });
    assert.equal(unknown.status, 502);
    assert.equal((await unknown.json()).error.effect, 'may_have_occurred');
    assert.equal(forwarded.length, 2);
  } finally {
    child.kill('SIGTERM'); await once(child, 'exit');
    native.closeAllConnections(); await new Promise(resolve => native.close(resolve));
  }
});
