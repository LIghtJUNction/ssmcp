const terminalHelper = '/system/bin/sh /data/adb/modules/ssmcp/webui-transport.sh';
const setupAction = '/system/bin/sh /data/adb/modules/ssmcp/action.sh setup';
const maxBodyBytes = 24 * 1024;
let nextCallback = 0;

export function hasManagerTransport() {
  return location.hostname !== '127.0.0.1' && typeof window.ksu?.exec === 'function';
}

function managerExec(command, timeoutMs) {
  return new Promise((resolve, reject) => {
    if (!hasManagerTransport()) {
      reject(Error('请在模块管理器中打开设备终端。'));
      return;
    }
    const callback = '__ssmcpManager' + (++nextCallback);
    const finish = () => { delete window[callback]; clearTimeout(timer); };
    const timer = setTimeout(() => {
      finish();
      reject(Error('模块管理器未及时响应，请检查会话状态。'));
    }, timeoutMs);
    window[callback] = (code, stdout) => {
      finish();
      if (Number(code) !== 0) reject(Error('模块服务未就绪，请检查模块状态。'));
      else resolve(stdout);
    };
    try {
      window.ksu.exec(command, '{}', callback);
    } catch {
      finish();
      reject(Error('模块管理器无法执行此操作。'));
    }
  });
}

export async function managerTerminal(body) {
  const bytes = new TextEncoder().encode(JSON.stringify(body));
  if (!bytes.length || bytes.length > maxBodyBytes) throw Error('终端请求大小超出范围。');
  const argument = btoa(String.fromCharCode(...bytes));
  if (!/^[A-Za-z0-9+/]+={0,2}$/.test(argument) || argument.length > 32768 || argument.length % 4) {
    throw Error('无法编码终端请求。');
  }

  const reply = await managerExec(terminalHelper + ' ' + argument, 12000);
  if (typeof reply !== 'string' || !/^[1-5][0-9]{2}\n/.test(reply)) {
    throw Error('无法读取模块服务的响应。');
  }
  const status = Number(reply.slice(0, 3));
  let result;
  try { result = JSON.parse(reply.slice(4)); } catch { /* HTTP errors can also have plain text bodies. */ }
  if (status < 200 || status >= 300) {
    const message = typeof result?.error === 'string' ? result.error : '模块服务返回错误（' + status + '）。';
    const error = Error(message);
    error.status = status;
    throw error;
  }
  if (!result || typeof result !== 'object' || Array.isArray(result)) {
    throw Error('模块服务返回了无效的终端数据。');
  }
  return result;
}

export async function openManagerSetup() {
  await managerExec(setupAction, 25000);
}
