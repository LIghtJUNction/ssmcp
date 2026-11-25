import { api, el, hasCredential, terminal, usesOfficialRelay } from './device-api.js';

function setView(view) {
  document.body.dataset.view = view;
  el('setup').hidden = view !== 'setup';
  el('complete').hidden = view !== 'complete';
}
function status(message) {
  el('status').textContent = message;
}

function relayChanged() {
  const server = el('server').value.trim();
  el('server-display').textContent = server || '未填写';
  el('hosted').hidden = !usesOfficialRelay(server);
  el('registration').hidden = true;
}
el('server').addEventListener('input', relayChanged);
el('name').addEventListener('input', () => { el('registration').hidden = true; });

async function configure(register) {
  if (document.body.dataset.view !== 'setup' || el('connect').disabled) return;
  const name = el('name').value.trim();
  const server = el('server').value.trim();
  if (!name || !server) {
    status('请填写用户名和服务器域名。');
    if (name && !server) el('relay-settings').open = true;
    el(name ? 'server' : 'name').focus();
    return;
  }
  const required = [['root', '请先确认 Android root 权限。']];
  if (!el('hosted').hidden) {
    required.push(['terms', '请先阅读并同意用户协议。'], ['privacy', '请先阅读并同意隐私政策。']);
  }
  for (const [id, message] of required) {
    if (el(id).checked) continue;
    status(message);
    el(id).focus();
    return;
  }
  const request = {
    target: `${name}@${server}`, register,
    root: el('root').checked, terms: el('terms').checked, privacy: el('privacy').checked,
  };
  const controls = ['name', 'server', 'root', 'terms', 'privacy', 'connect', 'register'].map(el);
  controls.forEach(control => { control.disabled = true; });
  el('registration').hidden = true;
  status(register ? '正在请求注册…' : '正在检查中继账号…');
  try {
    const result = await api('/setup', request);
    if (result.registration) {
      const link = new URL(result.registration);
      if (!['https:', 'http:'].includes(link.protocol)) throw Error('中继返回了无效的注册链接。');
      el('registration-link').href = link.href;
      el('registration').hidden = false;
    } else {
      setView('notice');
      status('配置已保存，正在读取设备信息…');
      try { await readConfiguration(); }
      catch (error) { status(`配置已保存。读取设备信息失败：${error.message} 请刷新此页。`); }
      return;
    }
    status('');
  } catch (error) {
    if (error.status === 409) {
      try { await readConfiguration(); } catch (readError) { status(readError.message); return; }
      if (document.body.dataset.view === 'complete') return;
    }
    status(`${register ? '注册' : '连接'}失败：${error.message}`);
  } finally {
    controls.forEach(control => { control.disabled = false; });
  }
}
el('connect').onclick = () => configure(false);
el('register').onclick = () => configure(true);

async function readConfiguration(checkCompletionOnly = false) {
  const { target, configured, mcp } = await terminal('list', 0);
  if (target) {
    el('complete-target').textContent = target;
    el('complete-mcp').textContent = mcp;
    el('complete-traffic').hidden = !usesOfficialRelay(mcp);
    setView('complete');
  } else {
    if (checkCompletionOnly) return;
    if (configured) {
      const [name, server] = configured.split('@');
      el('name').value = name;
      el('server').value = server;
    }
    relayChanged();
    setView('setup');
  }
  status('');
}

function checkCompletion() {
  if (hasCredential && document.body.dataset.view === 'setup' && !el('connect').disabled) {
    readConfiguration(true).catch(error => status(error.message));
  }
}
window.addEventListener('pageshow', event => { if (event.persisted) checkCompletion(); });
document.addEventListener('visibilitychange', () => { if (!document.hidden) checkCompletion(); });

if (hasCredential) {
  status('正在读取初始化状态…');
  readConfiguration().catch(error => status(error.message));
} else {
  setView('notice');
  status('请返回模块管理器，打开 SSMCP 的 WebUI，再点「打开安装引导」。');
}
