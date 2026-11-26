import { api, el, hasCredential, openSessionGui, terminal, usesOfficialRelay } from './device-api.js';

import { registerAccount } from './src/gateway.mjs';

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
  el('hosted').hidden = !usesOfficialRelay(server);
}
el('server').addEventListener('input', relayChanged);

async function configure(register = false) {
  if (document.body.dataset.view !== 'setup' || el('connect').disabled) return;
  const name = el('name').value.trim();
  const server = el('server').value.trim();
  if (!name || !server) {
    status('请填写用户名和服务器域名。');
    el(name ? 'server' : 'name').focus();
    return;
  }
  const required = register ? [] : [['root', '请先确认 Android root 权限。']];
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
    target: `${name}@${server}`,
    root: el('root').checked, terms: el('terms').checked, privacy: el('privacy').checked,
  };
  const controls = ['name', 'server', 'root', 'terms', 'privacy', 'connect', 'register-name'].map(el);
  controls.forEach(control => { control.disabled = true; });
  status('正在检查中继账号…');
  try {
    if (register) {
      await registerAccount(body => api('/api/gui/v1/register', body), {destination: request.target, terms: request.terms, privacy: request.privacy}, el('registration-link'));
      status('完成注册后，回到此页连接。'); return;
    }
    await api('/setup', request);
    openSessionGui();
  } catch (error) {
    if (error.status === 409) {
      try { await readConfiguration(); } catch (readError) { status(readError.message); return; }
      if (document.body.dataset.view === 'complete') return;
    }
    status(`连接失败：${error.message}`);
  } finally {
    controls.forEach(control => { control.disabled = false; });
  }
}
el('connect').onclick = () => configure();
el('register-name').onclick = () => configure(true);
el('open-gui').onclick = openSessionGui;

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
