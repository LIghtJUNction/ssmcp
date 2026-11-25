export const el = id => document.getElementById(id);

function readCredential() {
  // Manager WebViews use a separate origin. Only the device listener accepts this bearer.
  if (location.protocol !== 'http:' || location.hostname !== '127.0.0.1') return null;
  const key = 'ssmcp-console';
  const fragment = location.hash.slice(1);
  if (/^[0-9a-f]{64}$/.test(fragment)) {
    try { sessionStorage.setItem(key, fragment); } catch { /* This page can still use the credential. */ }
    history.replaceState(null, '', location.pathname + location.search);
    return fragment;
  }
  try {
    const saved = sessionStorage.getItem(key);
    return /^[0-9a-f]{64}$/.test(saved) ? saved : null;
  } catch { return null; }
}
const token = readCredential();
export const hasCredential = Boolean(token);

export function usesOfficialRelay(server) {
  try {
    const url = new URL(server.includes('://') ? server : 'https://' + server);
    return url.hostname.toLowerCase().replace(/\.+$/, '') === 'ssmcp.lmm.best';
  } catch {
    return false;
  }
}

export async function api(path, body) {
  if (!hasCredential) throw Error('请返回模块管理器，重新打开 SSMCP 的 WebUI。');
  let response;
  try {
    response = await fetch(path, {
      method: 'POST',
      mode: 'same-origin',
      headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${token}` },
      body: JSON.stringify(body),
      redirect: 'error',
    });
  } catch {
    if (body.action === 'input') throw Error('未收到设备确认，请先检查终端输出。');
    throw Error('无法访问设备上的模块服务，请检查服务后重试。');
  }
  const result = await response.json().catch(() => null);
  if (!response.ok) {
    const message = response.status === 401 ? '页面授权已失效，请返回模块管理器，重新打开 SSMCP 的 WebUI。' : result?.error;
    const error = Error(message || `模块服务返回错误（${response.status}）。`);
    error.status = response.status;
    throw error;
  }
  if (!result) throw Error('无法读取模块服务的响应。');
  return result;
}

export const terminal = (action, tab, args = {}) => api('/terminal', { action, tab, ...args });

// Keep one source string for a paste. Only the chunk being sent is measured;
// keyboard input arriving behind it shares one bounded buffer.
const inputBytes = 4096;
const waitingUnits = 64 * 1024;
const encoder = new TextEncoder();

export class TerminalWriter {
  constructor(send, interrupted, idle) {
    this.send = send;
    this.interrupted = interrupted;
    this.idle = idle;
    this.queue = [];
    this.queuedUnits = 0;
    this.hasPaste = false;
    this.done = null;
  }

  append(text) {
    const large = text.length > waitingUnits;
    if (!this.done) {
      this.hasPaste = large;
      this.done = this.drain(text);
      return true;
    }
    if (large ? this.hasPaste : text.length > waitingUnits - this.queuedUnits) return false;
    // At most three strings wait: typing before a large paste, the paste, and typing after.
    const last = this.queue.length - 1;
    if (large || last < 0 || this.queue[last].length > waitingUnits) this.queue.push(text);
    else this.queue[last] += text;
    if (large) this.hasPaste = true;
    else this.queuedUnits += text.length;
    return true;
  }

  async drain(source) {
    let scratch;
    let offset = 0, end = 0;
    try {
      while (source) {
        // One UTF-16 unit needs at most three UTF-8 bytes. Most keystrokes fit directly.
        if (source.length - offset <= Math.floor(inputBytes / 3)) end = source.length;
        else {
          scratch ??= new Uint8Array(inputBytes);
          // Include the low surrogate if a pair crosses the slice boundary.
          const window = source.slice(offset, offset + inputBytes + 1);
          end = offset + encoder.encodeInto(window, scratch).read;
        }
        await this.send(source.slice(offset, end));
        offset = end;
        if (offset === source.length) {
          if (source.length > waitingUnits) this.hasPaste = false;
          source = this.queue.shift() || '';
          if (source.length <= waitingUnits) this.queuedUnits -= source.length;
          offset = 0;
        }
      }
    } catch (error) {
      this.interrupted(source.slice(offset, end), source.slice(end) + this.queue.join(''), error);
    } finally {
      this.queue.length = 0;
      this.queuedUnits = 0;
      this.hasPaste = false;
      this.done = null;
      this.idle();
    }
  }
}
