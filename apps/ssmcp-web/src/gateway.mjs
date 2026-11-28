import { t } from './locale.mjs';
import {
  MAX_FRAME_BYTES, MAX_OBSERVATION_MS, ProtocolError, byteLength,
  counter, createRequest, identifier, object, requestId, validateObservation, validateRequest, validateResponse,
} from './protocol.mjs';

export class GatewayError extends Error {
  constructor(message, { code = 'transport_error', effect = 'none', status = 0, requestId = null, relayOrigin = null } = {}) {
    super(message);
    this.name = 'GatewayError';
    this.code = code;
    this.effect = effect;
    this.status = status;
    this.relayOrigin = relayOrigin;
    this.requestId = requestId;
  }
}

function abortError() { return new DOMException(t('observationAborted'), 'AbortError'); }

async function boundedJson(response, maximum) {
  const length = Number(response.headers.get('content-length'));
  if (Number.isFinite(length) && length > maximum) throw new ProtocolError(t('gatewayResponseTooLarge'));
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
        if (size > maximum) throw new ProtocolError(t('gatewayResponseTooLarge'));
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
    if (byteLength(text) > maximum) throw new ProtocolError(t('gatewayResponseTooLarge'));
  }
  try { return JSON.parse(text); } catch { throw new ProtocolError(t('invalidGatewayJson')); }
}

export function launchOptions(location) {
  const url = new URL(location.href);
  const session = url.searchParams.get('session');
  if (url.protocol !== 'http:' || url.hostname !== '127.0.0.1' || url.pathname !== '/'
      || url.username || url.password) return null;
  if (url.searchParams.size === 2 && url.searchParams.get('setup') === '1' && url.searchParams.get('launch_refused') === '1') {
    return { setup: true, refused: true };
  }
  if (url.searchParams.size !== 1) return null;
  if (/^live:[0-9a-f]{32}$/.test(session ?? '')) return {session};
  return url.searchParams.get('setup') === '1' ? {setup: true} : null;
}

export function takeLaunchPairing(location, history, modulePage) {
  const fragment = location.hash;
  if (!fragment) return null;
  const moduleOrigin = 'http://127.0.0.1:42387';
  const moduleHome = modulePage && location.href === moduleOrigin + '/' + fragment;
  const cliHome = !modulePage && Boolean(launchOptions(location));
  if (!moduleHome && !cliHome && !/^#[0-9a-f]{64}$/i.test(fragment)) return null;
  const clean = location.origin + location.pathname + location.search;
  try { history.replaceState(history.state, '', clean); }
  catch {
    // Navigate to the clean address if this embedded browser rejects a history update.
    try { location.replace(clean); } catch { /* No credential is accepted on failure. */ }
    return null;
  }
  return (moduleHome || cliHome) && /^#[0-9a-f]{64}$/.test(fragment) ? fragment.slice(1) : null;
}

// Open on the user's click, before awaiting the native registration request.
export async function registerAccount(request, args, link) {
  link.hidden = true;
  const popup = window.open('about:blank', '_blank');
  if (popup) popup.opener = null;
  try {
    const {url} = await request(args);
    const relay = args.destination.slice(args.destination.indexOf('@') + 1);
    const expected = new URL(relay.includes('://') ? relay : 'https://' + relay);
    const address = new URL(url);
    if (!['http:', 'https:'].includes(address.protocol) || address.origin !== expected.origin
        || address.username || address.password) throw new Error(t('invalidRegistrationUrl'));
    if (popup && !popup.closed) popup.location.replace(address.href);
    else { link.href = address.href; link.hidden = false; }
  } catch (error) { if (popup && !popup.closed) popup.close(); throw error; }
}

export async function readPairingToken(file, origin) {
  if (file.size > 8 * 1024) throw new Error(t('pairingFileTooLarge'));
  let pairing;
  try { pairing = JSON.parse(await file.text()); }
  catch { throw new Error(t('pairingFileUnreadable')); }
  if (pairing?.schema !== 'ssmcp-gui-pairing-v1' || typeof pairing.url !== 'string'
      || typeof pairing.token !== 'string' || !/^[0-9a-f]{64}$/i.test(pairing.token)) {
    throw new Error(t('pairingFileInvalid'));
  }
  let url;
  try { url = new URL(pairing.url); } catch { throw new Error(t('pairingAddressInvalid')); }
  if (!['http:', 'https:'].includes(url.protocol) || url.origin !== origin || url.href !== origin + '/') {
    throw new Error(t('pairingOriginMismatch'));
  }
  return pairing.token;
}

/** Same-origin HTTP transport. Requests only send the in-memory token in their authorization header. */
export class Gateway {
  #token = '';
  #ownedAttempt = null;
  #ownedUncertain = false;
  #ownedSessions = new Map();
  constructor({ fetchImpl = globalThis.fetch.bind(globalThis), prefix = '/api/gui/v1' } = {}) {
    if (!/^\/[A-Za-z0-9_/-]+$/.test(prefix) || prefix.startsWith('//')) {
      throw new TypeError(t('invalidGatewayPrefix'));
    }
    this.fetchImpl = fetchImpl;
    this.prefix = prefix.replace(/\/$/, '');
  }

  setToken(token) {
    if (typeof token !== 'string' || !token.trim() || token.length > 4096 || /[\r\n]/.test(token)) {
      throw new TypeError(t('invalidPairingToken'));
    }
    const changed = this.#token !== token.trim();
    if (changed) {
      this.#ownedAttempt = null; this.#ownedUncertain = false; this.#ownedSessions.clear();
    }
    this.#token = token.trim();
    return changed;
  }
  clearToken() { this.#token = ''; this.#ownedAttempt = null; this.#ownedUncertain = false; this.#ownedSessions.clear(); }
  get authenticated() { return this.#token.length > 0; }

  async request(path, { method = 'GET', body, signal, mutation = false, requestId = null, timeoutMs = 15_000 } = {}) {
    if (!this.#token) throw new GatewayError(t('pairingTokenRequired'), { code: 'authentication_required' });
    if (signal?.aborted) throw abortError();
    const controller = new AbortController();
    const cancel = () => controller.abort(signal.reason);
    signal?.addEventListener('abort', cancel, { once: true });
    let timedOut = false;
    const timeout = setTimeout(() => { timedOut = true; controller.abort(); }, Math.min(timeoutMs, MAX_OBSERVATION_MS));
    let dispatched = false;
    try {
      const encoded = body === undefined ? undefined : JSON.stringify(body);
      if (encoded !== undefined && byteLength(encoded) > MAX_FRAME_BYTES) throw new ProtocolError(t('requestTooLarge'));
      dispatched = true;
      const response = await this.fetchImpl(this.prefix + path, {
        method, headers: { Authorization: `Bearer ${this.#token}`, Accept: 'application/json', ...(encoded === undefined ? {} : { 'Content-Type': 'application/json' }) },
        body: encoded, signal: controller.signal, credentials: 'same-origin', cache: 'no-store',
        redirect: 'error', referrerPolicy: 'no-referrer',
      });
      if (!response.ok) {
        let error;
        try { error = (await boundedJson(response, MAX_FRAME_BYTES))?.error; }
        catch (cause) {
          if (signal?.aborted || timedOut) throw cause;
          // Proxies can return an empty/HTML error. Keep the HTTP status; only a
          // validated native envelope can establish that a write had no effect.
        }
        const valid = error && typeof error.code === 'string' && typeof error.message === 'string'
          && ['none', 'may_have_occurred'].includes(error.effect);
        throw new GatewayError(valid ? error.message.slice(0, 1024) : t('gatewayHttpStatus', response.status), {
          code: valid ? error.code : 'http_error', effect: valid ? error.effect : mutation ? 'may_have_occurred' : 'none',
          status: response.status, requestId,
          relayOrigin: valid && typeof error.relay_origin === 'string' ? error.relay_origin : null,
        });
      }
      return await boundedJson(response, MAX_FRAME_BYTES);
    } catch (error) {
      if (error instanceof GatewayError) throw error;
      if (mutation && dispatched) {
        throw new GatewayError(t('uncertainGatewayRequest'), { code: 'unknown', effect: 'may_have_occurred', requestId });
      }
      if (signal?.aborted) throw abortError();
      if (error instanceof ProtocolError) throw error;
      throw new GatewayError(timedOut ? t('incompleteObservation') : t('gatewayConnectionFailed'), {
        code: timedOut ? 'observation_timeout' : 'network_error', requestId,
      });
    } finally {
      clearTimeout(timeout);
      signal?.removeEventListener('abort', cancel);
    }
  }

  get pendingOwnedRequest() { return this.#ownedAttempt; }

  async createOwnedSession({ destination, terms, privacy }, showAuthorization) {
    if (!this.authenticated) throw new GatewayError(t('pairGatewayFirst'), { code: 'authentication_required' });
    if (typeof destination !== 'string' || !destination || typeof terms !== 'boolean' || typeof privacy !== 'boolean') {
      throw new TypeError(t('verifySessionAddressAndTerms'));
    }
    const attempt = this.#ownedAttempt ??= Object.freeze({ request_id: requestId('owned'), destination, terms, privacy });
    if (attempt.destination !== destination || attempt.terms !== terms || attempt.privacy !== privacy) {
      throw new GatewayError(t('retryCreationWithSameInput'), { code: 'pending', requestId: attempt.request_id });
    }
    try {
      let result;
      for (;;) {
        if (this.#ownedAttempt !== attempt) throw new GatewayError(t('gatewayPairingChanged'), {code: 'pairing_changed'});
        result = await this.request('/owned-sessions', { method: 'POST', body: attempt, mutation: true, requestId: attempt.request_id });
        if (this.#ownedAttempt !== attempt) throw new GatewayError(t('gatewayPairingChanged'), {code: 'pairing_changed'});
        if (result?.request_id !== attempt.request_id || !result.authorization || result.session !== undefined) break;
        const { status, verification_url, user_code } = result.authorization;
        if (status === 'pending') {
          const relay = destination.slice(destination.indexOf('@') + 1);
          const origin = new URL(relay.includes('://') ? relay : 'https://' + relay).origin;
          const link = new URL(verification_url);
          if (link.protocol !== 'https:' || link.origin !== origin || link.username || link.password
              || link.pathname !== '/setup' || link.hash || link.searchParams.size !== 1
              || typeof user_code !== 'string' || !/^[A-Za-z0-9-]{4,32}$/.test(user_code) || link.searchParams.get('code') !== user_code) {
            throw new GatewayError(t('deviceLoginOriginMismatch'), {code:'invalid_device_authorization'});
          }
          if (typeof showAuthorization !== 'function') throw new GatewayError(t('deviceOAuthRequired'), {code:'device_verification_required'});
          showAuthorization(link.href, user_code);
        } else if (status !== 'starting') {
          throw new GatewayError(t('deviceAuthorizationUnavailable'), {code:'invalid_device_authorization'});
        }
        await new Promise(resolve => setTimeout(resolve, 1500));
      }
      if (result?.request_id !== attempt.request_id || typeof result?.session !== 'string' || result.session.length !== 37
          || !/^live:[0-9a-f]{32}$/.test(result.session)) {
        throw new GatewayError(t('invalidCreationReceipt'), {code: 'unknown', effect: 'may_have_occurred', requestId: attempt.request_id});
      }
      this.#ownedAttempt = null; this.#ownedUncertain = false;
      if (!this.#ownedSessions.has(result.session)) this.#ownedSessions.set(result.session, { status: 'open' });
      return result.session;
    } catch (error) {
      if (this.#ownedAttempt === attempt) {
        if (error.effect === 'may_have_occurred') this.#ownedUncertain = true;
        else if (!this.#ownedUncertain) this.#ownedAttempt = null;
      }
      throw error;
    }
  }

  ownedSessionStatus(target) { return this.#ownedSessions.get(target)?.status; }

  async endOwnedSession(target) {
    const entry = this.#ownedSessions.get(target);
    if (!entry) throw new GatewayError(t('closeOwnedSessionOnly'), {code:'not_owned'});
    if (entry.status === 'closing') throw new GatewayError(t('awaitingEndReceipt'), {code:'pending'});
    const previous = entry.status;
    entry.status = 'closing';
    try {
      const result = await this.request('/owned-sessions/' + encodeURIComponent(target), {method:'DELETE', mutation:true});
      if (this.#ownedSessions.get(target) !== entry) throw new GatewayError(t('gatewayPairingChanged'), {code:'pairing_changed'});
      if (result?.session !== target || result?.status !== 'closed') {
        throw new GatewayError(t('endReceiptMismatch'), {code:'unknown', effect:'may_have_occurred'});
      }
      this.#ownedSessions.delete(target);
    } catch (error) {
      if (this.#ownedSessions.get(target) !== entry) throw new GatewayError(t('gatewayPairingChanged'), {code:'pairing_changed'});
      entry.status = error.effect === 'may_have_occurred' ? 'unknown' : previous;
      throw error;
    }
  }

  async sessions({ offset = 0, limit = 16, signal } = {}) {
    if (!Number.isSafeInteger(offset) || offset < 0 || !Number.isInteger(limit) || limit < 1 || limit > 32) throw new TypeError(t('invalidSessionPagination'));
    const page = await this.request(`/sessions?offset=${offset}&limit=${limit}`, { signal });
    object(page);
    if (!Array.isArray(page.sessions) || page.sessions.length > limit || typeof page.has_more !== 'boolean'
        || (page.has_more && (!Number.isSafeInteger(page.next_offset) || page.next_offset <= offset))) {
      throw new ProtocolError(t('invalidSessionDirectoryPagination'));
    }
    for (const session of page.sessions) {
      if (typeof session?.name !== 'string' || !session.name || session.name.length > 1024) throw new ProtocolError(t('missingSessionName'));
      if (session.target != null && (typeof session.target !== 'string' || !session.target || session.target.length > 1024)) throw new ProtocolError(t('invalidSessionTarget'));
    }
    return page;
  }

  async open({ request_id, session, read_only = true }) {
    identifier(request_id, t('connectionRequestId'));
    if (typeof session !== 'string' || !session || session.length > 1024 || typeof read_only !== 'boolean') throw new TypeError(t('invalidConnectionArguments'));
    const result = await this.request('/connections', { method: 'POST', body: { request_id, session, read_only }, mutation: true, requestId: request_id });
    if (result?.status !== 'connected' || typeof result.connection_id !== 'string'
        || result.session !== session || result.read_only !== read_only || typeof result.duplicate !== 'boolean') {
      throw new GatewayError(t('connectionReceiptMismatch'), { code: 'unknown', effect: 'may_have_occurred', requestId: request_id });
    }
    try { identifier(result.connection_id, t('connectionId')); }
    catch { throw new GatewayError(t('invalidConnectionReceiptId'), { code: 'unknown', effect: 'may_have_occurred', requestId: request_id }); }
    try {
      validateResponse(result.capabilities, createRequest({ op: 'capabilities' }, request_id));
      if (result.capabilities.body.kind !== 'capabilities') throw new ProtocolError(t('missingCapabilitiesReceipt'));
    } catch {
      throw new GatewayError(t('invalidCapabilitiesReceipt'), { code: 'unknown', effect: 'may_have_occurred', requestId: request_id });
    }
    return result;
  }

  async bridge(connectionId, request, { signal } = {}) {
    identifier(connectionId, t('connectionId'));
    validateRequest(request);
    return this.request(`/connections/${encodeURIComponent(connectionId)}/bridge`, {
      method: 'POST', body: request, signal, mutation: request.operation.op === 'command', requestId: request.id,
      timeoutMs: request.operation.op === 'command' ? MAX_OBSERVATION_MS : 15_000,
    });
  }

  async observe(connectionId, request, { signal } = {}) {
    identifier(connectionId, t('connectionId'));
    identifier(request.request_id, t('observationRequestId'));
    identifier(request.generation, t('observationGeneration'));
    if (request.cursor !== undefined && request.cursor !== null) counter(request.cursor);
    if (!Number.isInteger(request.wait_ms) || request.wait_ms < 0 || request.wait_ms > MAX_OBSERVATION_MS
        || !Number.isInteger(request.limit) || request.limit < 1 || request.limit > 128) throw new ProtocolError(t('invalidObservationWindow'));
    const result = await this.request(`/connections/${encodeURIComponent(connectionId)}/events`, {
      method: 'POST', body: request, signal, requestId: request.request_id, timeoutMs: MAX_OBSERVATION_MS,
    });
    validateObservation(result, request);
    return result;
  }

  async close(connectionId) {
    identifier(connectionId, t('connectionId'));
    const result = await this.request(`/connections/${encodeURIComponent(connectionId)}`, { method: 'DELETE', mutation: true });
    if (result?.closed !== true || result.session_stopped !== false) throw new GatewayError(t('closeReceiptMismatch'), { code: 'unknown', effect: 'may_have_occurred' });
    return result;
  }
}
