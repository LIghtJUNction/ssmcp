export const el = id => document.getElementById(id);

function readCredential() {
  // A device setup credential belongs to one native action and one page load.
  // Clear cached credentials left by older module versions without using them.
  if (location.protocol !== 'http:' || location.hostname !== '127.0.0.1'
      || location.pathname !== '/setup' || location.search) return null;
  try { sessionStorage.removeItem('ssmcp-console'); } catch { /* Storage is optional. */ }
  const fragment = location.hash.slice(1);
  if (location.hash) history.replaceState(null, '', location.pathname);
  return fragment.length === 64 && /^[0-9a-f]{64}$/.test(fragment) ? fragment : null;
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
  if (!hasCredential) throw Error('请从模块管理器重新打开设备设置。');
  if (path !== '/setup' && path !== '/terminal') throw Error('设备凭据仅供本机设备设置使用。');
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
    const message = response.status === 401 ? '设备授权已失效，请从模块管理器重新打开设备设置。' : result?.error;
    const error = Error(message || `模块服务返回错误（${response.status}）。`);
    error.status = response.status;
    throw error;
  }
  if (!result) throw Error('无法读取模块服务的响应。');
  return result;
}

export const terminal = (action, tab, args = {}) => api('/terminal', { action, tab, ...args });
