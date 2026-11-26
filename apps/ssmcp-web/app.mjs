import { Gateway, readPairingToken, takeMagiskPairing } from './src/gateway.mjs';
import { BridgeClient } from './src/client.mjs';
import { byteLength } from './src/protocol.mjs';
import { renderWaitContent } from './src/wait-view.mjs';
import { plain, shortId, duration, redact, mayClearDraft, quoteReply } from './src/presentation.mjs';

const $ = id => document.getElementById(id);
const gateway = new Gateway();
{
  const pairing = takeMagiskPairing(location, history, document.documentElement.hasAttribute('data-ssmcp-magisk'));
  if (pairing) gateway.setToken(pairing);
}
const client = new BridgeClient(gateway);
const busy = new Set();
const drafts = new Map();
let sessions = [], sessionOffset = 0, sessionsNext = null, selectedSession = null;
let selectedTerminal = null, selectedThread = 'process', timelineOffset = 0, activePanel = 'conversation';
let lastRenderedRows = [], localError = null, workerDraftEdited = false, queueOffset = 0;
const composing = new WeakSet();

function element(tag, className = '', text = '') {
  const node = document.createElement(tag);
  node.className = className; node.textContent = text; return node;
}
function setText(id, value) { $(id).textContent = value; }
function show(id, visible) { $(id).hidden = !visible; }
function errorInfo(error) {
  return { code: error?.code, message: error?.message ?? String(error), effect: error?.effect, request_id: error?.requestId };
}
function fail(error) { if (error?.name === 'AbortError') return; localError = error; renderStatus(); }
async function run(key, action) {
  if (busy.has(key)) return;
  busy.add(key); renderStatus(); renderComposer();
  try { return await action(); } catch (error) { fail(error); }
  finally { busy.delete(key); renderStatus(); renderComposer(); }
}
function metadata(value, label = '原始元数据') {
  const details = element('details', 'event-meta');
  details.append(element('summary', '', label));
  details.addEventListener('toggle', () => {
    if (details.open && details.children.length === 1) details.append(element('pre', '', JSON.stringify(redact(value), null, 2)));
  });
  return details;
}
function details(title, value) {
  setText('details-title', title);
  $('details-content').replaceChildren(element('pre', '', JSON.stringify(redact(value), null, 2)));
  if (!$('details-dialog').open) $('details-dialog').showModal();
}
function authDialog() {
  if (client.connection || ['connecting', 'disconnecting'].includes(client.status)) { fail(new Error('请先完成当前连接操作并断开前端，再更换认证。')); return; }
  setText('auth-error', ''); $('pairing-token').value = ''; $('auth-dialog').showModal(); $('pairing-file').focus();
}
function saveDrafts() {
  if (!client.connection) return;
  drafts.delete(client.connection.session);
  drafts.set(client.connection.session, { chat: $('chat-text').value, terminal: $('terminal-input').value, target: $('chat-target').value });
  while (drafts.size > 8) drafts.delete(drafts.keys().next().value);
}
function restoreDrafts(session) {
  const draft = drafts.get(session);
  $('chat-text').value = draft?.chat ?? ''; $('terminal-input').value = draft?.terminal ?? '';
  if (draft?.target && [...$('chat-target').options].some(option => option.value === draft.target)) $('chat-target').value = draft.target;
}

async function listSessions(offset = 0) {
  const page = await gateway.sessions({ offset, limit: 16 });
  sessions = page.sessions; sessionOffset = offset; sessionsNext = page.has_more ? page.next_offset : null;
  renderSessions(); renderStatus();
}
function sessionTarget(row) { return row?.target ?? row?.name; }
function renderSessions() {
  const nodes = sessions.map(session => {
    const target = sessionTarget(session);
    const button = element('button', 'session-item'); button.type = 'button';
    button.disabled = ['connecting', 'disconnecting'].includes(client.status) || busy.has('session-select');
    button.setAttribute('aria-current', String(target === (client.connection?.session ?? sessionTarget(selectedSession))));
    const detail = session.cwd || session.phase || session.connection_state || '状态未提供';
    const sameName = sessions.some(item => item.name === session.name && sessionTarget(item) !== target);
    button.append(element('strong', '', session.name), element('small', 'mono', sameName ? `${detail} · ${target}` : detail));
    button.title = [session.name, session.cwd, target !== session.name ? target : ''].filter(Boolean).join('\n');
    button.addEventListener('click', () => run('session-select', async () => {
      if (client.connection?.session === target) return;
      if (client.connection) { saveDrafts(); await client.disconnect(); }
      selectedSession = session; timelineOffset = 0; selectedTerminal = null; workerDraftEdited = false;
      localError = null; renderAll();
      $('sidebar').classList.remove('is-open'); $('menu-button').setAttribute('aria-expanded', 'false');
    }));
    return button;
  });
  if (!nodes.length) nodes.push(element('p', 'quiet', gateway.authenticated ? '没有找到会话。请确认 CLI 会话正在运行。' : '认证后显示本机真实会话。'));
  $('session-list').replaceChildren(...nodes);
  show('session-pagination', sessionOffset > 0 || sessionsNext !== null);
  $('sessions-prev').disabled = sessionOffset === 0; $('sessions-next').disabled = sessionsNext === null;
}

const connectionNames = { disconnected: '未连接', connecting: '正在连接', connected: '已连接', paused: '连接需检查', disconnecting: '正在断开' };
const observationNames = { observing: '正在观察', paused: '观察已暂停', backoff: '断线退避', resyncing: '正在同步状态', unsupported: '无事件能力' };
function renderStatus() {
  const connected = Boolean(client.connection);
  setText('auth-status', gateway.authenticated ? '已设置认证' : '尚未认证');
  $('auth-dot').classList.toggle('good', gateway.authenticated);
  setText('auth-button', gateway.authenticated ? '认证设置' : '连接网关');
  setText('connection-label', connected ? `SSMCP / ${client.capabilities?.mode === 'control' ? 'CONTROL' : 'OBSERVE'}` : 'WORKSPACE');
  const info = sessions.find(item => sessionTarget(item) === client.connection?.session) ?? selectedSession;
  setText('session-title', info?.name ?? '工作台');
  setText('session-cwd', info?.cwd ?? '');
  show('connect-strip', Boolean(selectedSession) && !connected);
  show('connected-actions', connected);
  $('connect-button').disabled = busy.has('connect') || client.status === 'connecting' || !gateway.authenticated;
  $('disconnect-button').disabled = busy.has('disconnect');
  $('refresh-state').disabled = busy.has('refresh') || !client.capabilities;
  $('refresh-sessions').disabled = busy.has('sessions') || !gateway.authenticated;
  document.querySelectorAll('.session-item').forEach(button => { button.disabled = ['connecting', 'disconnecting'].includes(client.status) || busy.has('session-select'); });
  $('observe-button').disabled = !client.capabilities?.events || busy.has('observe');
  setText('observe-button', client.observation === 'paused' ? '继续观察' : '暂停观察');
  const live = $('observation-status');
  live.children[1].textContent = connected ? observationNames[client.observation] ?? connectionNames[client.status] : connectionNames[client.status];
  live.children[0].className = `status-dot ${connected && client.observation === 'observing' ? 'good' : client.error ? 'warn' : ''}`;
  setText('status-cursor', client.cursor === null ? 'zbridge v1' : `cursor ${client.cursor}`);
  const error = localError ?? client.error;
  show('error-banner', Boolean(error)); if (error) setText('error-text', error.message ?? String(error));
  setText('command-count', client.ledger.records.length || '');
  show('conversation-toolbar', connected);
  show('welcome', !connected);
  $('authenticate').disabled = busy.has('auth');
  $('forget-token').disabled = busy.has('forget') || busy.has('auth');
}

function fillAgents(select, baseOptions, selected, allowMissing) {
  const agents = client.states.get('threads')?.agents ?? [];
  const options = [...baseOptions, ...agents.slice(0, 32).map(agent => [agent.agent_id, `${agent.name || shortId(agent.agent_id)} · ${agent.role || agent.status || 'agent'}`])];
  if (allowMissing && selected && !options.some(([value]) => value === selected)) options.push([selected, `当前页未列出 · ${shortId(selected)}`, true]);
  const signature = JSON.stringify(options);
  if (select.dataset.options === signature) return;
  select.dataset.options = signature;
  select.replaceChildren(...options.map(([value, text, disabled]) => {
    const option = element('option', '', text); option.value = value; option.disabled = Boolean(disabled); return option;
  }));
  select.value = options.some(([value]) => value === selected) ? selected : options[0][0];
}
function renderComposer() {
  const threads = client.states.get('threads');
  const supported = client.hasCommand('chat.send');
  show('chat-form', supported);
  fillAgents($('chat-target'), [['next', '下一个合法接收者']], $('chat-target').value, true);
  fillAgents($('thread-filter'), [['process', '整个会话'], ['main', '主对话']], selectedThread, true);
  const validTarget = $('chat-target').selectedOptions[0]?.disabled !== true;
  $('chat-send').disabled = !supported || threads?.can_send !== true || !validTarget || busy.has('chat.send') || !$('chat-text').value.trim() || byteLength($('chat-text').value) > 16_384;
  const note = !client.connection ? '先连接一个会话。' : !supported ? '当前连接未提供 chat.send；可继续观察真实事件。'
    : threads?.can_send !== true ? '当前运行时暂不能接收输入。草稿保留在此页面。'
      : !validTarget ? '当前线程页未列出原接收者。请打开线程列表核对并显式选择接收方。'
        : byteLength($('chat-text').value) > 16_384 ? '输入超过 16 KiB UTF-8 上限，请缩短后发送。'
          : '发送到 CLI 本地输入队列。入队回执不表示模型已读；未知结果不会自动重发。';
  setText('composer-note', note);
  show('queue-button', client.hasResource('chat.queue'));
  show('threads-button', client.hasResource('threads'));
  show('history-more', client.hasResource('conversation') && (!client.history || client.history.has_more));
  $('history-more').disabled = busy.has('history');
  $('terminal-send').disabled = busy.has('terminal.write') || !$('terminal-input').value || !selectedTerminal
    || byteLength($('terminal-input').value + ($('terminal-newline').checked ? '\n' : '')) > 16_384;
}

function timelineFilter(row) {
  if (row.terminalId) return false;
  if (selectedThread === 'process' || ['notice', 'receipt'].includes(row.kind)) return true;
  if (row.source?.kind === 'worker_snapshot') return selectedThread === 'main';
  if (row.source?.kind === 'local_queue') {
    return selectedThread === 'main' || row.metadata?.target === selectedThread || row.metadata?.target?.agent_id === selectedThread;
  }
  if (row.source?.kind === 'local_human') {
    const recipient = row.source.recipient_agent_id;
    return selectedThread === 'main' ? !recipient || recipient === 'next' : recipient === selectedThread;
  }
  return selectedThread === 'main' ? row.source?.kind === 'main' : row.source?.kind === 'thread' && row.source.agent_id === selectedThread;
}
function sourceLabel(row) {
  if (row.kind === 'receipt' || row.kind === 'notice') return '此页面';
  if (row.source?.kind === 'local_queue') return '真实队列状态';
  if (row.source?.kind === 'local_human') return row.source.recipient_agent_id && row.source.recipient_agent_id !== 'next' ? shortId(row.source.recipient_agent_id) : '主对话';
  if (row.source?.kind === 'worker_snapshot') return '实际 pool 快照';
  if (row.source?.kind === 'main') return '主对话';
  if (row.source?.kind === 'thread' && row.source.agent_id) {
    const agent = client.states.get('threads')?.agents?.find(item => item.agent_id === row.source.agent_id);
    return agent?.name ? `${agent.name} · ${shortId(row.source.agent_id)}` : shortId(row.source.agent_id);
  }
  return '来源未归属';
}
const stateLabels = { confirmed: '回执已验证', rejected: '已拒绝', unknown: '未确定', sending: '发送中', running: '运行中', done: '完成', error: '错误', cancelled: '已取消' };
function eventNode(row) {
  const article = element('article', `timeline-row is-${row.kind === 'notice' ? 'notice' : row.kind === 'receipt' ? 'receipt' : 'event'}`);
  article.dataset.row = row.id; article._row = row;
  article.classList.toggle('is-input', row.inputSide === true);
  const mark = element('div', `event-mark ${row.kind === 'answer' ? 'answer' : ''}`, row.kind === 'answer' ? 's.' : row.kind === 'input' ? '↑' : row.kind === 'receipt' ? '↗' : '·');
  const body = element('div'); const heading = element('div', 'event-heading');
  heading.append(element('strong', '', row.title || row.kind), element('span', 'source', sourceLabel(row)));
  if (row.status) heading.append(element('span', `pill ${['unknown', 'confirmed', 'error', 'done'].includes(row.status) ? row.status : ''}`, row.kind === 'worker_snapshot' ? `最后观察 · ${row.status}` : row.kind === 'input' ? `队列 · ${row.status}` : stateLabels[row.status] ?? String(row.status)));
  if (row.sequence) heading.append(element('span', 'source mono', `#${row.sequence}`));
  const textNode = element('div', 'event-text', plain(row.text, 16384));
  body.append(heading, textNode);
  if (row.messageId) {
    article.tabIndex = 0;
    const reply = element('button', 'text-button reply-button', '引用回复');
    reply.type = 'button'; reply.title = '选中消息后按 R 引用回复';
    const insertReply = () => {
      const selection = window.getSelection();
      const excerpt = selection && !selection.isCollapsed && textNode.contains(selection.anchorNode)
        && textNode.contains(selection.focusNode) ? selection.toString() : row.text;
      try {
        const composer = $('chat-text');
        composer.value = quoteReply(row.messageId, excerpt, composer.value);
        renderComposer(); composer.focus(); composer.setSelectionRange(composer.value.length, composer.value.length);
      } catch (error) { fail(error); }
    };
    reply.addEventListener('click', insertReply);
    article.addEventListener('keydown', event => {
      if (event.target === article && event.key.toLowerCase() === 'r' && !event.ctrlKey && !event.metaKey && !event.altKey) {
        event.preventDefault(); insertReply();
      }
    });
    body.append(reply);
  }
  body.append(metadata(row.metadata, '详细信息与原始 ID'));
  article.append(mark, body); return article;
}
function renderTimeline({ preservePage = true } = {}) {
  const scroll = $('timeline-scroller');
  const nearBottom = scroll.scrollHeight - scroll.scrollTop - scroll.clientHeight < 90;
  const previousTop = scroll.scrollTop;
  const nodes = [...$('timeline').children];
  const anchor = nodes.find(node => node.getBoundingClientRect().bottom > scroll.getBoundingClientRect().top);
  const anchorY = anchor?.getBoundingClientRect().top;
  const allRows = client.timeline.rows.filter(timelineFilter);
  if (preservePage && (!nearBottom || timelineOffset > 0) && lastRenderedRows.length) {
    const end = allRows.findIndex(row => row.id === lastRenderedRows.at(-1));
    if (end >= 0) timelineOffset = allRows.length - end - 1;
  }
  timelineOffset = Math.min(timelineOffset, Math.max(0, allRows.length - 1));
  const page = client.timeline.page({ offset: timelineOffset, filter: timelineFilter });
  const existing = new Map(nodes.map(node => [node.dataset.row, node]));
  const wanted = page.rows.map(row => {
    const old = existing.get(row.id);
    if (old?._row === row) return old;
    const node = eventNode(row);
    if (old?.querySelector('details')?.open) node.querySelector('details').open = true;
    return node;
  });
  // Moving an existing node preserves expanded metadata and focus within it.
  for (let index = 0; index < wanted.length; index += 1) {
    if ($('timeline').children[index] !== wanted[index]) $('timeline').insertBefore(wanted[index], $('timeline').children[index] ?? null);
  }
  for (const node of [...$('timeline').children]) if (!wanted.includes(node)) node.remove();
  lastRenderedRows = page.rows.map(row => row.id);
  show('timeline-empty', Boolean(client.connection) && page.rows.length === 0);
  show('timeline-paging', Boolean(client.connection) && (page.older || page.newer));
  $('timeline-older').disabled = !page.older; $('timeline-newer').disabled = !page.newer;
  setText('timeline-page-label', `${page.rows.length} / ${page.total} 条本地记录`);
  setText('timeline-count', page.total || '');
  show('retention-note', Boolean(client.connection) && (client.timeline.dropped > 0 || page.rows.length > 0));
  setText('retention-note', client.timeline.dropped > 0 ? `页面最多保留 300 条 / 768 KiB；已释放 ${client.timeline.dropped} 条较早记录。服务端历史也受当前进程保留窗口限制。` : '当前进程保留历史 · 事件序号保持原始精度');
  if (nearBottom && timelineOffset === 0) scroll.scrollTop = scroll.scrollHeight;
  else {
    const retained = anchor ? [...$('timeline').children].find(node => node.dataset.row === anchor.dataset.row) : null;
    if (retained && anchorY !== undefined) scroll.scrollTop = previousTop + retained.getBoundingClientRect().top - anchorY;
    else scroll.scrollTop = previousTop;
  }
  show('new-events', Boolean(client.connection) && (!nearBottom || timelineOffset > 0) && page.total > 0);
}

function renderTerminals() {
  const state = client.states.get('terminals');
  const terminals = Array.isArray(state?.terminals) ? state.terminals.slice(0, 128) : [];
  show('terminal-content', client.hasResource('terminals') && terminals.length > 0);
  show('terminal-empty', !client.hasResource('terminals') || terminals.length === 0);
  setText('terminal-empty', client.stateErrors.get('terminals') ?? (client.hasResource('terminals') ? '当前列表没有终端。' : '当前连接没有终端资源。'));
  setText('terminal-count', terminals.length || '');
  if (!terminals.some(item => item.terminal_id === selectedTerminal)) selectedTerminal = terminals.find(item => item.terminal_id === state?.selected)?.terminal_id ?? terminals[0]?.terminal_id ?? null;
  const signature = JSON.stringify(terminals.map(item => [item.terminal_id, item.name, selectedTerminal === item.terminal_id]));
  if ($('terminal-list').dataset.signature !== signature) {
    $('terminal-list').dataset.signature = signature;
    $('terminal-list').replaceChildren(...terminals.map(terminal => {
      const button = element('button', 'terminal-tab', terminal.name || shortId(terminal.terminal_id));
      button.setAttribute('aria-pressed', String(terminal.terminal_id === selectedTerminal));
      button.addEventListener('click', () => { selectedTerminal = terminal.terminal_id; renderTerminals(); renderComposer(); });
      return button;
    }));
  }
  const selected = terminals.find(item => item.terminal_id === selectedTerminal);
  setText('terminal-caption', selected ? `${selected.terminal_id} · ${selected.cwd ?? 'cwd 未提供'}` : '');
  const chunks = client.timeline.rows.filter(row => row.terminalId === selectedTerminal && row.generation === client.generation).map(row => row.text);
  const output = chunks.join('').slice(-65_536) || '等待此终端的新输出。保留事件之前的屏幕内容不会被伪造。';
  const screen = $('terminal-output'); const bottom = screen.scrollHeight - screen.scrollTop - screen.clientHeight < 50;
  if (screen.textContent !== output) { screen.textContent = output; if (bottom) screen.scrollTop = screen.scrollHeight; }
  show('terminal-form', Boolean(selected) && client.hasCommand('terminal.write'));
  show('terminal-resize', client.hasCommand('terminal.resize'));
}

function renderWorkers() {
  const state = client.states.get('workers');
  const supported = client.hasResource('workers');
  show('workers-content', supported && Boolean(state)); show('workers-empty', !supported || !state);
  setText('workers-empty', client.stateErrors.get('workers') ?? (supported ? '正在读取 pool 状态。' : '当前连接没有 worker pool 资源。'));
  show('worker-details', Boolean(state));
  setText('worker-count', state?.total ?? '');
  if (!state) return;
  const counts = [['total', '当前站数'], ['registered_workers', '已注册'], ['working', '工作中'], ['idle', '空闲'], ['offline', '离线'], ['queued', '排队任务']];
  $('worker-summary').replaceChildren(...counts.map(([key, title]) => {
    const node = element('div', 'metric'); node.append(element('strong', '', state[key] ?? '—'), element('span', '', title)); return node;
  }));
  show('worker-resize', client.hasCommand('workers.resize') && typeof state.pool_generation === 'string');
  if (!workerDraftEdited && document.activeElement !== $('worker-total')) $('worker-total').value = state.desired_total ?? '';
  $('worker-resize').querySelector('button').disabled = busy.has('workers.resize');
  const slots = Array.isArray(state.slots) ? state.slots.slice(0, 32) : [];
  const signature = JSON.stringify(slots);
  if ($('worker-list').dataset.signature !== signature) {
    $('worker-list').dataset.signature = signature;
    $('worker-list').replaceChildren(...slots.map(slot => {
      const card = element('article', 'worker-card'); const heading = element('div', 'worker-card-header');
      heading.append(element('h3', '', `站点 ${slot.slot}`), element('span', 'pill', slot.state || '未提供状态'));
      if (slot.retire_requested) heading.append(element('span', 'pill', 'retiring'));
      card.append(heading, element('p', 'worker-task', plain(slot.task_summary || slot.task_id || slot.reason || (slot.registered ? '没有当前任务。' : '尚未注册。'))));
      if (slot.progress !== null && slot.progress !== undefined && slot.progress !== '') card.append(element('p', 'worker-progress', plain(typeof slot.progress === 'string' ? slot.progress : JSON.stringify(slot.progress))));
      if (slot.last_command) card.append(element('p', 'tiny mono', plain(typeof slot.last_command === 'string' ? slot.last_command : JSON.stringify(slot.last_command), 250)));
      const foot = element('div', 'worker-foot');
      foot.append(element('span', 'tiny', slot.current_task_elapsed_ms ? `当前任务 ${duration(slot.current_task_elapsed_ms)}` : `累计工作 ${duration(slot.total_work_ms)}`));
      card.append(foot, metadata(slot, '站点与任务原始 ID')); return card;
    }));
  }
  const offset = Number.isSafeInteger(state.offset) ? state.offset : 0;
  setText('workers-page', `${offset + (slots.length ? 1 : 0)}–${offset + slots.length} / ${state.slot_count ?? '—'}`);
  $('workers-prev').disabled = offset === 0 || busy.has('workers-page');
  $('workers-next').disabled = !Number.isSafeInteger(state.next_offset) || state.next_offset <= offset || busy.has('workers-page');
}

let timeReadInFlight = false;
function renderWaits() {
  const snapshot = client.states.get('time'), supported = client.hasResource('time');
  show('waits-content', supported && Boolean(snapshot)); show('waits-empty', !supported || !snapshot);
  setText('waits-empty', client.stateErrors.get('time') ?? (supported ? '正在读取等待与计时。' : '当前连接尚不支持等待面板。'));
  setText('wait-count', snapshot?.active?.length ?? '');
  if (snapshot && supported) renderWaitContent($('waits-content'), snapshot,
    client.stateErrors.get('time') ? '刷新未完成，保留上次核实的快照。' :
    client.observation === 'paused' ? '观察已暂停，保留最近快照。' : '显示当前连接最近一次返回的实际快照。');
}
async function refreshWaits() {
  if (timeReadInFlight || !client.connection || !client.hasResource('time')) return;
  timeReadInFlight = true;
  try { await client.readWindow(signal => client.readState('time', {}, { signal })); }
  catch (error) { if (error?.name !== 'AbortError') renderWaits(); }
  finally { timeReadInFlight = false; }
}
// A visible read-only panel refreshes independently of the long event window.
// Pausing observation or hiding the page stops new reads; no command is sent.
setInterval(() => {
  if (activePanel === 'waits' && !document.hidden && client.observation === 'observing') void refreshWaits();
}, 2000);

function renderAll() { renderStatus(); renderSessions(); renderComposer(); renderTimeline(); renderTerminals(); renderWorkers(); renderWaits(); }
let pendingRender = false;
client.subscribe(event => {
  if (event.type === 'status' || event.type === 'error') { renderStatus(); renderWaits(); return; }
  if (pendingRender) return;
  pendingRender = true;
  requestAnimationFrame(() => { pendingRender = false; renderStatus(); renderComposer(); renderTimeline(); renderTerminals(); renderWorkers(); renderWaits(); });
});

async function send(command, args, textarea) {
  const draft = textarea?.value;
  const context = client.context();
  const target = command === 'chat.send' ? args.target : command === 'terminal.write' ? args.terminal_id : null;
  const submitted = { ...context, text: draft, target };
  const record = await client.command(command, args);
  if (record.status === 'confirmed') {
    const currentTarget = command === 'chat.send' ? $('chat-target').value : command === 'terminal.write' ? selectedTerminal : null;
    if (textarea && mayClearDraft(record.status, submitted, { ...client.context(), text: textarea.value, target: currentTarget })) textarea.value = '';
    if (client.current(context)) localError = null;
  } else {
    localError = Object.assign(new Error(record.status === 'unknown' ? '结果未确定。请在操作记录中核对原请求 ID；页面不会自动重发。' : record.error?.message ?? '操作已拒绝。'), { code: record.status, requestId: record.requestId });
  }
  renderStatus(); renderComposer(); return record;
}

$('auth-button').addEventListener('click', authDialog); $('welcome-connect').addEventListener('click', authDialog);
$('pairing-file').addEventListener('change', () => { if ($('pairing-file').files.length) $('pairing-token').value = ''; });
$('pairing-token').addEventListener('input', () => { if ($('pairing-token').value) $('pairing-file').value = ''; });
$('auth-form').addEventListener('submit', event => {
  event.preventDefault(); void run('auth', async () => {
    try {
      const file = $('pairing-file').files[0];
      const token = file ? await readPairingToken(file, location.origin) : $('pairing-token').value.trim();
      if (!token) throw new Error('请选择配对文件或输入令牌。');
      if (!$('auth-dialog').open) return;
      gateway.setToken(token);
      $('auth-form').reset();
      try { await listSessions(); }
      catch (error) {
        gateway.clearToken(); sessions = []; selectedSession = null; sessionsNext = null; renderSessions(); throw error;
      }
      localError = null; $('auth-dialog').close();
    } catch (error) { setText('auth-error', error.message); }
  });
});
$('auth-dialog').addEventListener('close', () => { $('auth-form').reset(); });
$('forget-token').addEventListener('click', () => run('forget', async () => {
  try { if (client.connection) { saveDrafts(); await client.disconnect(); } }
  finally { gateway.clearToken(); sessions = []; selectedSession = null; sessionOffset = 0; sessionsNext = null; $('auth-dialog').close(); renderAll(); }
}));
document.querySelectorAll('[data-close]').forEach(button => button.addEventListener('click', () => $(button.dataset.close).close()));
$('refresh-sessions').addEventListener('click', () => run('sessions', () => listSessions(sessionOffset)));
$('sessions-prev').addEventListener('click', () => run('sessions', () => listSessions(Math.max(0, sessionOffset - 16))));
$('sessions-next').addEventListener('click', () => run('sessions', () => listSessions(sessionsNext)));
$('connect-button').addEventListener('click', () => run('connect', async () => {
  const session = sessionTarget(selectedSession);
  localError = null; await client.connect(session, $('connection-mode').value === 'observe');
  restoreDrafts(session); timelineOffset = 0; renderAll();
  void client.startObservation();
}));
$('disconnect-button').addEventListener('click', () => run('disconnect', async () => { saveDrafts(); await client.disconnect(); renderAll(); }));
$('observe-button').addEventListener('click', () => run('observe', async () => {
  if (client.observation === 'paused') { await client.pauseObservation(); localError = null; void client.startObservation(); }
  else await client.pauseObservation();
}));
$('refresh-state').addEventListener('click', () => run('refresh', async () => { localError = null; await client.refresh(); renderAll(); }));
$('menu-button').addEventListener('click', () => { const open = $('sidebar').classList.toggle('is-open'); $('menu-button').setAttribute('aria-expanded', String(open)); });
document.querySelector('.main').addEventListener('click', event => { if (event.target.closest('#menu-button')) return; $('sidebar').classList.remove('is-open'); $('menu-button').setAttribute('aria-expanded', 'false'); });
document.addEventListener('keydown', event => { if (event.key === 'Escape') { $('sidebar').classList.remove('is-open'); $('menu-button').setAttribute('aria-expanded', 'false'); } });
$('theme-button').addEventListener('click', () => { const dark = document.documentElement.dataset.theme ? document.documentElement.dataset.theme === 'dark' : matchMedia('(prefers-color-scheme:dark)').matches; document.documentElement.dataset.theme = dark ? 'light' : 'dark'; });

const tabButtons = [...document.querySelectorAll('[data-panel]')];
function setPanel(name) {
  activePanel = name;
  tabButtons.forEach(button => { const active = button.dataset.panel === name; button.setAttribute('aria-selected', String(active)); button.tabIndex = active ? 0 : -1; show(`panel-${button.dataset.panel}`, active); });
  if (name === 'conversation') renderTimeline();
  if (name === 'waits') { renderWaits(); void refreshWaits(); }
}
tabButtons.forEach(button => {
  button.addEventListener('click', () => setPanel(button.dataset.panel));
  button.addEventListener('keydown', event => {
    if (!['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) return;
    event.preventDefault(); const index = tabButtons.indexOf(button);
    const next = event.key === 'Home' ? 0 : event.key === 'End' ? tabButtons.length - 1 : (index + (event.key === 'ArrowRight' ? 1 : -1) + tabButtons.length) % tabButtons.length;
    setPanel(tabButtons[next].dataset.panel); tabButtons[next].focus();
  });
});
$('thread-filter').addEventListener('change', () => { selectedThread = $('thread-filter').value; timelineOffset = 0; lastRenderedRows = []; renderTimeline({ preservePage: false }); });
$('timeline-older').addEventListener('click', () => { timelineOffset += 40; renderTimeline({ preservePage: false }); $('timeline-scroller').scrollTop = 0; });
$('timeline-newer').addEventListener('click', () => { timelineOffset = Math.max(0, timelineOffset - 40); renderTimeline({ preservePage: false }); });
$('new-events').addEventListener('click', () => { timelineOffset = 0; lastRenderedRows = []; renderTimeline({ preservePage: false }); $('timeline-scroller').scrollTop = $('timeline-scroller').scrollHeight; show('new-events', false); });
$('timeline-scroller').addEventListener('scroll', () => { const scroll = $('timeline-scroller'); if (timelineOffset === 0 && scroll.scrollHeight - scroll.scrollTop - scroll.clientHeight < 90) show('new-events', false); }, { passive: true });
$('history-more').addEventListener('click', () => run('history', () => client.loadHistory()));

for (const [textareaId, formId] of [['chat-text', 'chat-form'], ['terminal-input', 'terminal-form']]) {
  const textarea = $(textareaId);
  textarea.addEventListener('input', renderComposer);
  textarea.addEventListener('compositionstart', () => composing.add(textarea));
  textarea.addEventListener('compositionend', () => { composing.delete(textarea); renderComposer(); });
  textarea.addEventListener('keydown', event => {
    if (event.key === 'Enter' && (event.ctrlKey || event.metaKey) && !event.isComposing && !composing.has(textarea)) {
      event.preventDefault(); if (!$(formId).querySelector('button[type="submit"]').disabled) $(formId).requestSubmit();
    }
  });
}
$('chat-target').addEventListener('change', renderComposer); $('terminal-newline').addEventListener('change', renderComposer);
$('chat-form').addEventListener('submit', event => {
  event.preventDefault(); if ($('chat-send').disabled || composing.has($('chat-text'))) return;
  void run('chat.send', () => send('chat.send', { text: $('chat-text').value, target: $('chat-target').value }, $('chat-text')));
});
$('terminal-form').addEventListener('submit', event => {
  event.preventDefault(); if ($('terminal-send').disabled || composing.has($('terminal-input'))) return;
  void run('terminal.write', () => send('terminal.write', { terminal_id: selectedTerminal, data: $('terminal-input').value + ($('terminal-newline').checked ? '\n' : '') }, $('terminal-input')));
});
$('terminal-resize').addEventListener('submit', event => {
  event.preventDefault(); void run('terminal.resize', () => send('terminal.resize', { rows: Number($('terminal-rows').value), cols: Number($('terminal-cols').value) }));
});
$('worker-total').addEventListener('input', () => { workerDraftEdited = true; });
$('worker-resize').addEventListener('submit', event => {
  event.preventDefault(); void run('workers.resize', async () => {
    const state = client.states.get('workers'); const draft = $('worker-total').value;
    const context = client.context();
    const record = await send('workers.resize', { pool_generation: state.pool_generation, total: Number(draft) });
    if (record.status === 'confirmed' && client.current(context) && client.states.get('workers')?.pool_generation === state.pool_generation && $('worker-total').value === draft) workerDraftEdited = false;
  });
});
for (const direction of ['prev', 'next']) $('workers-' + direction).addEventListener('click', () => run('workers-page', async () => {
  const state = client.states.get('workers'); const offset = direction === 'prev' ? Math.max(0, state.offset - 16) : state.next_offset;
  await client.readState('workers', { offset, limit: 16 }); renderWorkers();
}));

$('terminal-details').addEventListener('click', () => details('终端信息', client.states.get('terminals')?.terminals?.find(item => item.terminal_id === selectedTerminal)));
$('worker-details').addEventListener('click', () => details('Worker pool', client.states.get('workers')));
$('error-details').addEventListener('click', () => details('错误与关联请求', errorInfo(localError ?? client.error)));
$('diagnostics-button').addEventListener('click', () => details('连接与观察', {
  session: client.connection?.session, connection_id: client.connection?.connection_id, generation: client.generation,
  connection_status: client.status, observation_status: client.observation, cursor: client.cursor,
  retention_start: client.startCursor, live_cursor: client.liveCursor, capabilities: client.capabilities,
  connection_requests: client.connectionRecords, resource_errors: Object.fromEntries(client.stateErrors),
  runtime: client.states.get('runtime'), error: (localError ?? client.error) ? errorInfo(localError ?? client.error) : null,
}));
$('commands-button').addEventListener('click', () => details('原始操作请求与回执', {
  meaning: 'confirmed 表示收到并验证原生回执；不代表异步任务完成。unknown 不会自动重发。', records: client.ledger.records,
}));

async function showQueue(offset = 0) {
  const page = await client.readState('chat.queue', { offset, limit: 16 }); queueOffset = offset;
  setText('details-title', 'CLI 输入队列');
  const nodes = [element('p', 'tiny', '这里直接显示 CLI 的队列状态。入队、转交与模型处理是不同阶段。')];
  for (const item of (page.items ?? []).slice(0, 32)) {
    const node = element('article', 'queue-item');
    node.append(element('span', `pill ${item.uncertain ? 'unknown' : ''}`, item.uncertain ? '未确定' : item.status ?? '状态未提供'), element('p', '', plain(item.text, 16384)), metadata(item, '目标、消息 ID 与状态详情'));
    nodes.push(node);
  }
  if (!page.items?.length) nodes.push(element('p', 'quiet', '当前页没有输入记录。'));
  const pagination = element('div', 'queue-pagination');
  const prev = element('button', 'text-button', '上一页'); prev.disabled = offset === 0;
  const next = element('button', 'text-button', '下一页'); next.disabled = !page.has_more;
  prev.addEventListener('click', () => run('queue', () => showQueue(Math.max(0, queueOffset - 16))));
  next.addEventListener('click', () => run('queue', () => showQueue(page.next_offset)));
  pagination.append(prev, next); nodes.push(pagination);
  $('details-content').replaceChildren(...nodes); if (!$('details-dialog').open) $('details-dialog').showModal();
}
$('queue-button').addEventListener('click', () => run('queue', () => showQueue()));
async function showThreads(offset = 0) {
  const page = await client.readState('threads', { offset, limit: 16 });
  setText('details-title', '当前连接的线程');
  const nodes = [element('p', 'tiny', '直接读取当前会话的 agent 名单。分页不会改变任何任务或其他端的输入草稿。')];
  for (const agent of (page.agents ?? []).slice(0, 32)) {
    const node = element('article', 'queue-item');
    node.append(element('h3', '', agent.name || shortId(agent.agent_id)), element('p', 'tiny mono', agent.agent_id), metadata(agent, `${agent.role ?? 'agent'} · ${agent.status ?? '状态未提供'}`));
    if (client.hasCommand('chat.send')) {
      const choose = element('button', 'text-button', '选择为接收方');
      choose.addEventListener('click', () => { renderComposer(); $('chat-target').value = agent.agent_id; $('details-dialog').close(); setPanel('conversation'); renderComposer(); $('chat-text').focus(); });
      node.append(choose);
    }
    nodes.push(node);
  }
  if (!page.agents?.length) nodes.push(element('p', 'quiet', '这一页没有已连接的 agent。'));
  const pagination = element('div', 'queue-pagination');
  const prev = element('button', 'text-button', '上一页'); prev.disabled = offset === 0;
  const next = element('button', 'text-button', '下一页'); next.disabled = !page.has_more;
  prev.addEventListener('click', () => run('threads', () => showThreads(Math.max(0, offset - 16))));
  next.addEventListener('click', () => run('threads', () => showThreads(page.next_offset)));
  pagination.append(prev, next); nodes.push(pagination);
  $('details-content').replaceChildren(...nodes); if (!$('details-dialog').open) $('details-dialog').showModal();
}
$('threads-button').addEventListener('click', () => run('threads', () => showThreads(client.queries.get('threads')?.offset ?? 0)));
document.addEventListener('visibilitychange', () => {
  if (document.hidden && client.connection) saveDrafts();
  // No lease release or command cancellation is inferred from tab visibility.
});
renderAll();
if (gateway.authenticated) {
  void run('auth', async () => {
    try { await listSessions(); }
    catch {
      gateway.clearToken();
      renderAll();
      throw new Error('模块配对失败，请从模块管理器重新打开 SSMCP。');
    }
  });
}
