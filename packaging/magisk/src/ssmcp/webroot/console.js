const el = id => document.getElementById(id);
const fromAction = location.hash.slice(1);
let token = sessionStorage.getItem('ssmcp-console');
if (/^[0-9a-f]{64}$/.test(fromAction)) {
  token = fromAction;
  sessionStorage.setItem('ssmcp-console', token);
  history.replaceState(null, '', '/');
}

async function api(path, body) {
  const response = await fetch(path, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${token ?? ''}` },
    body: JSON.stringify(body),
  });
  const result = await response.json();
  if (!response.ok) throw Error(result.error);
  return result;
}

let selected = 0;
const terminal = (action, args) => api('/terminal', { action, tab: selected, ...args });
const status = value => { el('status').textContent = value; };

function relayChanged() {
  const hosted = el('server').value.trim().toLowerCase().split(':')[0].replace(/\.+$/, '') === 'ssmcp.lmm.best';
  el('hosted').hidden = !hosted;
  el('connect').textContent = hosted ? '我接受风险，连接官方中继' : '我接受风险，连接自部署中继';
  el('registration').hidden = true;
}
el('server').addEventListener('input', relayChanged);
el('name').addEventListener('input', () => { el('registration').hidden = true; });

async function configure(register) {
  const name = el('name').value.trim();
  const server = el('server').value.trim();
  if (!name || !server) return status('请填写用户名和服务器域名。');
  el('connect').disabled = el('register').disabled = true;
  status('正在检查中继账号…');
  try {
    const result = await api('/setup', {
      target: `${name}@${server}`, register,
      root: el('root').checked, terms: el('terms').checked, privacy: el('privacy').checked,
    });
    if (result.registration) {
      el('registration-link').href = result.registration;
      el('registration').hidden = false;
    } else {
      await refreshTabs();
      el('plugin').open = true;
    }
    status('');
  } catch (error) {
    status(error.message);
  } finally {
    el('connect').disabled = el('register').disabled = false;
  }
}
el('connect').onclick = () => configure(false);
el('register').onclick = () => configure(true);

function button(label, action) {
  const control = document.createElement('button');
  control.textContent = label;
  control.onclick = async () => {
    try { await action(); } catch (error) { status(error.message); }
  };
  return control;
}

async function refreshTabs() {
  const { target, tabs, mcp, configured } = await terminal('list');
  el('setup').hidden = Boolean(target);
  el('console').hidden = !target;
  if (!target) {
    if (configured) {
      const [name, server] = configured.split('@');
      el('name').value = name;
      el('server').value = server;
      relayChanged();
    }
    return;
  }
  el('target').textContent = target;
  el('mcp').textContent = mcp;
  el('traffic').hidden = new URL(mcp).hostname.replace(/\.+$/, '') !== 'ssmcp.lmm.best';
  if (!tabs.includes(selected)) selected = tabs[0];
  const controls = tabs.map(id => {
    const tab = button(id ? `会话 ${id + 1}` : '启动会话', async () => {
      selected = id;
      await refreshTabs();
    });
    tab.setAttribute('aria-pressed', String(selected === id));
    return tab;
  });
  controls.push(button('+', async () => {
    selected = (await terminal('open')).tab;
    await refreshTabs();
  }));
  if (selected !== 0) controls.push(button('关闭当前', async () => {
    await terminal('close');
    await refreshTabs();
  }));
  el('tabs').replaceChildren(...controls);
  await resize();
}

async function resize() {
  const screen = el('screen');
  if (!el('console').hidden) {
    await terminal('resize', {
      cols: Math.floor((screen.clientWidth - 16) / 8),
      rows: Math.floor((screen.clientHeight - 16) / 17),
    });
  }
}
let sizeTimer;
window.addEventListener('resize', () => {
  clearTimeout(sizeTimer);
  sizeTimer = setTimeout(() => resize().catch(error => status(error.message)), 150);
});
let previous = '';
async function poll() {
  if (!document.hidden && !el('console').hidden) {
    const tab = selected;
    try {
      const { screen, exited, connected_once } = await terminal('screen');
      if (tab === selected && screen !== previous) {
        el('screen').textContent = previous = screen;
      }
      if (tab === selected) el('connection').textContent = exited ? '已退出' : connected_once ? '已完成首次连接' : '等待中继连接…';
    } catch (error) { status(error.message); }
  }
  setTimeout(poll, 450);
}

const sequences = {
  Enter: '\r', Escape: '\x1b', Tab: '\t', Backspace: '\x7f',
  ArrowUp: '\x1b[A', ArrowDown: '\x1b[B', ArrowRight: '\x1b[C', ArrowLeft: '\x1b[D',
};
const ctrl = el('keys').querySelector('[data-key=Ctrl]');
ctrl.onclick = () => {
  ctrl.setAttribute('aria-pressed', String(ctrl.getAttribute('aria-pressed') !== 'true'));
  el('input').focus();
};
function send(text) {
  return terminal('input', { text }).then(() => status('')).catch(error => {
    status(error.message);
    throw error;
  });
}
for (const key of el('keys').querySelectorAll('button:not([data-key=Ctrl])')) {
  key.onclick = () => {
    const name = key.dataset.key;
    if (name === 'Enter' && el('input').value) submit();
    else send(sequences[name]).catch(() => {});
  };
}
function submit() {
  const field = el('input');
  const text = field.value;
  const control = ctrl.getAttribute('aria-pressed') === 'true';
  if (control && !/^[a-z]$/i.test(text)) return status('Ctrl 组合键请输入一个英文字母。');
  const bytes = control ? String.fromCharCode(text.toUpperCase().charCodeAt(0) & 31) : text + '\r';
  ctrl.setAttribute('aria-pressed', 'false');
  send(bytes).then(() => { if (field.value === text) field.value = ''; }).catch(() => {});
}
el('send').onclick = submit;
el('input').addEventListener('keydown', event => {
  if (event.isComposing) return;
  if (event.key === 'Enter') { event.preventDefault(); submit(); }
  else if (event.ctrlKey && /^[a-z]$/i.test(event.key)) {
    event.preventDefault();
    send(String.fromCharCode(event.key.toUpperCase().charCodeAt(0) & 31)).catch(() => {});
  } else if (event.key in sequences && event.key !== 'Backspace') {
    event.preventDefault();
    send(sequences[event.key]).catch(() => {});
  }
});
if (token) refreshTabs().then(poll).catch(error => status(error.message));
else status('请从 Magisk 的模块操作打开此网页。');
