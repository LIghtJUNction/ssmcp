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
  el('hosted').hidden = !usesOfficialRelay(server);
}
el('server').addEventListener('input', relayChanged);

async function configure() {
  if (document.body.dataset.view !== 'setup' || el('connect').disabled) return;
  const name = el('name').value.trim();
  const server = el('server').value.trim();
  if (!name || !server) {
    status('请填写用户名和服务器域名。');
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
    target: `${name}@${server}`,
    root: el('root').checked, terms: el('terms').checked, privacy: el('privacy').checked,
  };
  const controls = ['name', 'server', 'root', 'terms', 'privacy', 'connect'].map(el);
  controls.forEach(control => { control.disabled = true; });
  status('正在检查中继账号…');
  try {
    await api('/setup', request);
    showComplete(request.target);
    await readConfiguration(true);
  } catch (error) {
    if (error.status === 409) {
      try { await readConfiguration(); } catch (readError) { status(readError.message); return; }
      if (document.body.dataset.view === 'complete') return;
    }
    status(document.body.dataset.view === 'complete' ? `已保存设备配置，账户详情暂时无法读取：${error.message}` : `连接失败：${error.message}`);
  } finally {
    controls.forEach(control => { control.disabled = false; });
  }
}
el('connect').onclick = () => configure();

function showComplete(target, mcp) {
  el('complete-target').textContent = target;
  el('complete-mcp').textContent = mcp || '请在中继账户页面查看';
  el('complete-traffic').hidden = !mcp || !usesOfficialRelay(mcp);
  setView('complete');
  status('');
}

async function readConfiguration(checkCompletionOnly = false) {
  const { target, configured, mcp } = await terminal('list', 0);
  if (target) {
    showComplete(target, mcp);
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
  status('请在设备上运行模块的 onboarding/open.sh 重新打开设备设置。');
}
