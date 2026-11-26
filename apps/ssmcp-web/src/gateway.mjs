import {
  MAX_FRAME_BYTES, MAX_OBSERVATION_MS, ProtocolError, byteLength,
  counter, createRequest, identifier, object, validateObservation, validateRequest, validateResponse,
} from './protocol.mjs';

export class GatewayError extends Error {
  constructor(message, { code = 'transport_error', effect = 'none', status = 0, requestId = null } = {}) {
    super(message);
    this.name = 'GatewayError';
    this.code = code;
    this.effect = effect;
    this.status = status;
    this.requestId = requestId;
  }
}

function abortError() { return new DOMException('观察已暂停。', 'AbortError'); }

async function boundedJson(response, maximum) {
  const length = Number(response.headers.get('content-length'));
  if (Number.isFinite(length) && length > maximum) throw new ProtocolError('网关响应超过大小限制。');
  let text;
  if (response.body?.getReader) {
    const reader = response.body.getReader();
    const decoder = new TextDecoder('utf-8', { fatal: true });
    const chunks = [];
    let size = 0;
    try {
      while (true) {
        const { done, value } = await reader.read();
        if (done) break;
        size += value.byteLength;
        if (size > maximum) throw new ProtocolError('网关响应超过大小限制。');
        chunks.push(decoder.decode(value, { stream: true }));
      }
      chunks.push(decoder.decode());
      text = chunks.join('');
    } finally {
      try { await reader.cancel(); } catch { /* No data is committed on a failed read. */ }
      reader.releaseLock();
    }
  } else {
    text = await response.text();
    if (byteLength(text) > maximum) throw new ProtocolError('网关响应超过大小限制。');
  }
  try { return JSON.parse(text); } catch { throw new ProtocolError('网关没有返回有效 JSON。'); }
}

export async function readPairingToken(file, origin) {
  if (file.size > 8 * 1024) throw new Error('配对文件过大，请选择网关生成的 pairing.json。');
  let pairing;
  try { pairing = JSON.parse(await file.text()); }
  catch { throw new Error('无法读取配对文件，请重新选择 pairing.json。'); }
  if (pairing?.schema !== 'ssmcp-gui-pairing-v1' || typeof pairing.url !== 'string'
      || typeof pairing.token !== 'string' || !/^[0-9a-f]{64}$/i.test(pairing.token)) {
    throw new Error('配对文件格式无效，请选择网关生成的 pairing.json。');
  }
  let url;
  try { url = new URL(pairing.url); } catch { throw new Error('配对文件中的网关地址无效。'); }
  if (!['http:', 'https:'].includes(url.protocol) || url.origin !== origin || url.href !== origin + '/') {
    throw new Error('配对文件与当前网页地址不一致，请打开对应网关的网页。');
  }
  return pairing.token;
}

/** Same-origin HTTP transport. The pairing token never enters URLs, storage or receipts. */
export class Gateway {
  #token = '';
  constructor({ fetchImpl = globalThis.fetch.bind(globalThis), prefix = '/api/gui/v1' } = {}) {
    if (!/^\/[A-Za-z0-9_/-]+$/.test(prefix) || prefix.startsWith('//')) {
      throw new TypeError('网关前缀必须是同源路径。');
    }
    this.fetchImpl = fetchImpl;
    this.prefix = prefix.replace(/\/$/, '');
  }

  setToken(token) {
    if (typeof token !== 'string' || !token.trim() || token.length > 4096 || /[\r\n]/.test(token)) {
      throw new TypeError('请输入有效的配对令牌。');
    }
    this.#token = token.trim();
  }
  clearToken() { this.#token = ''; }
  get authenticated() { return this.#token.length > 0; }

  async request(path, { method = 'GET', body, signal, mutation = false, requestId = null, timeoutMs = 15_000 } = {}) {
    if (!this.#token) throw new GatewayError('请先输入本机网关的配对令牌。', { code: 'authentication_required' });
    if (signal?.aborted) throw abortError();
    const controller = new AbortController();
    const cancel = () => controller.abort(signal.reason);
    signal?.addEventListener('abort', cancel, { once: true });
    let timedOut = false;
    const timeout = setTimeout(() => { timedOut = true; controller.abort(); }, Math.min(timeoutMs, MAX_OBSERVATION_MS));
    let dispatched = false;
    try {
      const encoded = body === undefined ? undefined : JSON.stringify(body);
      if (encoded !== undefined && byteLength(encoded) > MAX_FRAME_BYTES) throw new ProtocolError('请求帧过大。');
      dispatched = true;
      const response = await this.fetchImpl(this.prefix + path, {
        method, headers: { Authorization: `Bearer ${this.#token}`, Accept: 'application/json', ...(encoded === undefined ? {} : { 'Content-Type': 'application/json' }) },
        body: encoded, signal: controller.signal, credentials: 'same-origin', cache: 'no-store',
        redirect: 'error', referrerPolicy: 'no-referrer',
      });
      const data = await boundedJson(response, MAX_FRAME_BYTES);
      if (!response.ok) {
        const error = data?.error;
        const valid = error && typeof error.code === 'string' && typeof error.message === 'string'
          && ['none', 'may_have_occurred'].includes(error.effect);
        throw new GatewayError(valid ? error.message.slice(0, 1024) : `网关返回 HTTP ${response.status}。`, {
          code: valid ? error.code : 'http_error', effect: valid ? error.effect : mutation ? 'may_have_occurred' : 'none',
          status: response.status, requestId,
        });
      }
      return data;
    } catch (error) {
      if (error instanceof GatewayError) throw error;
      if (mutation && dispatched) {
        throw new GatewayError('请求已发出，但没有取得可验证回执；结果未确定。', { code: 'unknown', effect: 'may_have_occurred', requestId });
      }
      if (signal?.aborted) throw abortError();
      if (error instanceof ProtocolError) throw error;
      throw new GatewayError(timedOut ? '观察窗口已结束，尚未收到完整响应。' : '无法连接网关。', {
        code: timedOut ? 'observation_timeout' : 'network_error', requestId,
      });
    } finally {
      clearTimeout(timeout);
      signal?.removeEventListener('abort', cancel);
    }
  }

  async sessions({ offset = 0, limit = 16, signal } = {}) {
    if (!Number.isSafeInteger(offset) || offset < 0 || !Number.isInteger(limit) || limit < 1 || limit > 32) throw new TypeError('会话分页无效。');
    const page = await this.request(`/sessions?offset=${offset}&limit=${limit}`, { signal });
    object(page);
    if (!Array.isArray(page.sessions) || page.sessions.length > limit || typeof page.has_more !== 'boolean'
        || (page.has_more && (!Number.isSafeInteger(page.next_offset) || page.next_offset <= offset))) {
      throw new ProtocolError('会话目录分页无效。');
    }
    for (const session of page.sessions) {
      if (typeof session?.name !== 'string' || !session.name || session.name.length > 1024) throw new ProtocolError('会话目录缺少真实名称。');
    }
    return page;
  }

  async open({ request_id, session, read_only = true }) {
    identifier(request_id, '连接请求 ID');
    if (typeof session !== 'string' || !session || session.length > 1024 || typeof read_only !== 'boolean') throw new TypeError('连接参数无效。');
    const result = await this.request('/connections', { method: 'POST', body: { request_id, session, read_only }, mutation: true, requestId: request_id });
    if (result?.status !== 'connected' || typeof result.connection_id !== 'string'
        || result.session !== session || result.read_only !== read_only || typeof result.duplicate !== 'boolean') {
      throw new GatewayError('连接回执不匹配，连接结果未确定。', { code: 'unknown', effect: 'may_have_occurred', requestId: request_id });
    }
    try { identifier(result.connection_id, '连接 ID'); }
    catch { throw new GatewayError('连接回执 ID 无效，连接结果未确定。', { code: 'unknown', effect: 'may_have_occurred', requestId: request_id }); }
    try {
      validateResponse(result.capabilities, createRequest({ op: 'capabilities' }, request_id));
      if (result.capabilities.body.kind !== 'capabilities') throw new ProtocolError('连接缺少能力回执。');
    } catch {
      throw new GatewayError('连接能力回执无法验证，连接结果未确定。', { code: 'unknown', effect: 'may_have_occurred', requestId: request_id });
    }
    return result;
  }

  async bridge(connectionId, request, { signal } = {}) {
    identifier(connectionId, '连接 ID');
    validateRequest(request);
    return this.request(`/connections/${encodeURIComponent(connectionId)}/bridge`, {
      method: 'POST', body: request, signal, mutation: request.operation.op === 'command', requestId: request.id,
      timeoutMs: request.operation.op === 'command' ? MAX_OBSERVATION_MS : 15_000,
    });
  }

  async observe(connectionId, request, { signal } = {}) {
    identifier(connectionId, '连接 ID');
    identifier(request.request_id, '观察请求 ID');
    identifier(request.generation, '观察代次');
    if (request.cursor !== undefined && request.cursor !== null) counter(request.cursor);
    if (!Number.isInteger(request.wait_ms) || request.wait_ms < 0 || request.wait_ms > MAX_OBSERVATION_MS
        || !Number.isInteger(request.limit) || request.limit < 1 || request.limit > 128) throw new ProtocolError('观察窗口或页大小无效。');
    const result = await this.request(`/connections/${encodeURIComponent(connectionId)}/events`, {
      method: 'POST', body: request, signal, requestId: request.request_id, timeoutMs: MAX_OBSERVATION_MS,
    });
    validateObservation(result, request);
    return result;
  }

  async close(connectionId) {
    identifier(connectionId, '连接 ID');
    const result = await this.request(`/connections/${encodeURIComponent(connectionId)}`, { method: 'DELETE', mutation: true });
    if (result?.closed !== true || result.session_stopped !== false) throw new GatewayError('关闭回执不匹配，结果未确定。', { code: 'unknown', effect: 'may_have_occurred' });
    return result;
  }
}
