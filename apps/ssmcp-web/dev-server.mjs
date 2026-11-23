#!/usr/bin/env node
import http from 'node:http';
import { readFile } from 'node:fs/promises';
import { resolve, sep, extname } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('.', import.meta.url));
const args = process.argv.slice(2);
let port = 4173, upstream = null;
for (let index = 0; index < args.length; index += 1) {
  const option = args[index];
  if (option === '--port') port = Number(args[++index]);
  else if (option === '--gateway') upstream = new URL(args[++index]);
  else if (option === '--help') {
    process.stdout.write('node dev-server.mjs [--port 4173] [--gateway http://127.0.0.1:PORT]\n');
    process.exit(0);
  } else throw new Error(`Unknown option: ${option}`);
}
if (!Number.isInteger(port) || port < 1 || port > 65535) throw new Error('Invalid port.');
if (upstream && (!['http:', 'https:'].includes(upstream.protocol)
    || !['127.0.0.1', 'localhost', '[::1]'].includes(upstream.hostname)
    || upstream.username || upstream.password || upstream.search || upstream.hash || upstream.pathname !== '/')) {
  throw new Error('--gateway must be a loopback HTTP(S) origin, without credentials or a path.');
}
const maximum = 256 * 1024;
const types = { '.html': 'text/html; charset=utf-8', '.css': 'text/css; charset=utf-8', '.mjs': 'text/javascript; charset=utf-8' };
const headers = {
  'Cache-Control': 'no-store', 'X-Content-Type-Options': 'nosniff', 'Referrer-Policy': 'no-referrer',
  'Content-Security-Policy': "default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'",
};
function error(response, status, code, message, effect = 'none') {
  if (response.destroyed) return;
  response.writeHead(status, { ...headers, 'Content-Type': 'application/json; charset=utf-8' });
  response.end(JSON.stringify({ error: { code, message, effect } }));
}
async function bodyBytes(request) {
  const chunks = []; let size = 0;
  for await (const chunk of request) {
    size += chunk.length;
    if (size > maximum) throw new Error('body_too_large');
    chunks.push(chunk);
  }
  return Buffer.concat(chunks);
}

const server = http.createServer(async (request, response) => {
  try {
    const host = request.headers.host;
    const allowedHosts = new Set([`127.0.0.1:${port}`, `localhost:${port}`]);
    if (!allowedHosts.has(host)) { error(response, 403, 'denied', 'Unexpected Host header.'); return; }
    const ownOrigin = `http://${host}`;
    if (request.headers.origin && request.headers.origin !== ownOrigin) { error(response, 403, 'denied', 'Cross-origin requests are not allowed.'); return; }
    const url = new URL(request.url, ownOrigin);
    if (url.pathname.startsWith('/api/gui/v1/')) {
      if (!upstream) { error(response, 503, 'unavailable', '没有配置真实 CLI 网关。请使用 --gateway 指定其本机地址。'); return; }
      if (!['GET', 'POST', 'DELETE'].includes(request.method)) { error(response, 405, 'denied', 'Method not allowed.'); return; }
      const body = await bodyBytes(request);
      let mutation = request.method === 'DELETE' || request.method === 'POST';
      if (url.pathname.endsWith('/events')) mutation = false;
      if (url.pathname.endsWith('/bridge')) {
        try { mutation = JSON.parse(body.toString('utf8')).operation?.op === 'command'; } catch { /* Invalid body is passed through unchanged. */ }
      }
      const controller = new AbortController();
      const timeout = setTimeout(() => controller.abort(), 55_000);
      const disconnected = () => { if (!response.writableEnded) controller.abort(); };
      response.once('close', disconnected);
      try {
        const result = await fetch(new URL(url.pathname + url.search, upstream), {
          method: request.method, body: body.length ? body : undefined, signal: controller.signal, redirect: 'error',
          headers: { Accept: 'application/json', 'Content-Type': 'application/json',
            ...(request.headers.authorization ? { Authorization: request.headers.authorization } : {}),
            ...(request.headers.origin ? { Origin: upstream.origin } : {}) },
        });
        const chunks = []; let size = 0;
        if (result.body) for await (const chunk of result.body) {
          size += chunk.length;
          if (size > maximum) { controller.abort(); throw new Error('response_too_large'); }
          chunks.push(Buffer.from(chunk));
        }
        if (!response.destroyed) {
          response.writeHead(result.status, { ...headers, 'Content-Type': 'application/json; charset=utf-8' });
          response.end(Buffer.concat(chunks));
        }
      } catch {
        error(response, 502, 'upstream_unavailable', mutation ? '没有取得原生网关回执；操作结果未确定。' : '原生网关暂不可用。', mutation ? 'may_have_occurred' : 'none');
      } finally { clearTimeout(timeout); response.removeListener('close', disconnected); }
      return;
    }
    if (request.method !== 'GET' && request.method !== 'HEAD') { error(response, 405, 'denied', 'Method not allowed.'); return; }
    const path = decodeURIComponent(url.pathname === '/' ? '/index.html' : url.pathname);
    const file = resolve(root, '.' + path);
    if (!file.startsWith(root.endsWith(sep) ? root : root + sep) || !types[extname(file)]
        || path.includes('/test/') || path.endsWith('/dev-server.mjs')) { error(response, 404, 'unavailable', 'Not found.'); return; }
    try {
      const data = await readFile(file);
      response.writeHead(200, { ...headers, 'Content-Type': types[extname(file)], 'Content-Length': data.length });
      response.end(request.method === 'HEAD' ? undefined : data);
    } catch { error(response, 404, 'unavailable', 'Not found.'); }
  } catch {
    error(response, 400, 'invalid_request', '请求无效或超过大小限制。');
  }
});
server.requestTimeout = 60_000;
server.headersTimeout = 10_000;
server.listen(port, '127.0.0.1', () => {
  process.stdout.write(`SSMCP Web: http://127.0.0.1:${port}\n${upstream ? 'Proxying the configured local CLI gateway.' : 'Static preview only; no gateway configured.'}\n`);
});
