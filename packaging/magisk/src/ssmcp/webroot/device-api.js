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
  if (!hasCredential) throw Error('请从模块管理器重新打开页面。');
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
    const message = response.status === 401 ? '页面授权已失效，请从模块管理器重新打开。' : result?.error;
    const error = Error(message || `模块服务返回错误（${response.status}）。`);
    error.status = response.status;
    throw error;
  }
  if (!result) throw Error('无法读取模块服务的响应。');
  return result;
}

export const terminal = (action, tab, args = {}) => api('/terminal', { action, tab, ...args });
