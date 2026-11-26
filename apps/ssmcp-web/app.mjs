import { Gateway, launchOptions, takeLaunchPairing } from './src/gateway.mjs';
import { BridgeClient } from './src/client.mjs';
import { byteLength } from './src/protocol.mjs';
import { renderWaitContent } from './src/wait-view.mjs';
import { plain, shortId, duration, redact, mayClearDraft, quoteReply } from './src/presentation.mjs';

const systemTheme = matchMedia('(prefers-color-scheme: dark)');
function followSystemTheme() {
  document.documentElement.dataset.theme = systemTheme.matches ? 'dark' : 'light';
}
followSystemTheme();
systemTheme.onchange = followSystemTheme;

const $ = id => document.getElementById(id);
const gateway = new Gateway();
const modulePage = document.documentElement.hasAttribute('data-ssmcp-magisk');
{
  const pairing = takeLaunchPairing(location, history, modulePage);
  if (pairing) gateway.setToken(pairing);
}
const client = new BridgeClient(gateway);
const busy = new Set();
const drafts = new Map();
const opened = new Map();
let sessions = [], sessionOffset = 0, sessionsNext = null, selectedSession = null;
let selectedTerminal = null, selectedThread = 'process', timelineOffset = 0, activePanel = 'conversation';
let lastRenderedRows = [], localError = null, workerDraftEdited = false, queueOffset = 0;
const composing = new WeakSet();
const slashCommands = [
  { word: '/new', name: 'session.new', description: '新建会话' },
  { word: '/cd', name: 'session.cd', description: '切换工作目录', takesPath: true },
];
let directoryRead = 0, endTarget = null, submittingEnd = false;

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
function saveDrafts() {
  if (!client.connection) return;
  drafts.delete(client.connection.session);
  drafts.set(client.connection.session, { chat: $('chat-text').value, terminal: $('terminal-input').value, target: $('chat-target').value });
  while (drafts.size > 8) drafts.delete(drafts.keys().next().value);
}
function restoreDrafts(session) {
  const draft = drafts.get(session);
  $('chat-text').value = draft?.chat ?? ''; $('terminal-input').value = draft?.terminal ?? '';
}
function restoreRecipient(session) {
  const target = drafts.get(session)?.target;
  if (target && [...$('chat-target').options].some(option => option.value === target)) $('chat-target').value = target;
}

async function listSessions(offset = 0) {
  const read = ++directoryRead;
  const page = await gateway.sessions({ offset, limit: 16 });
  if (read !== directoryRead) return;
  sessions = page.sessions; sessionOffset = offset; sessionsNext = page.has_more ? page.next_offset : null;
  renderSessions(); renderStatus();
}
function sessionTarget(row) { return row?.target ?? row?.name; }
function currentTarget() { return client.connection?.session ?? sessionTarget(selectedSession); }
const mobileNavigation = matchMedia('(max-width: 700px)');
function syncNavigationLayout() {
  $('sidebar-settings').open = !mobileNavigation.matches;
  setSidebar(false);
}
function setSidebar(open) {
  const wasOpen = $('sidebar').classList.contains('is-open');
  $('sidebar').classList.toggle('is-open', open);
  document.querySelector('.main').inert = open;
  if (open) $('close-menu').focus({ preventScroll: true });
  else if (wasOpen && mobileNavigation.matches) $('menu-button').focus({ preventScroll: true });
  $('menu-button').setAttribute('aria-expanded', String(open));
  show('dismiss-sidebar', open);
}
syncNavigationLayout();
mobileNavigation.addEventListener('change', syncNavigationLayout);
function renderSessionTabs() {
  const current = client.connection?.session;
  const nodes = [...opened].map(([target, tab]) => {
    const button = element('button', 'session-tab', tab.name);
    button.type = 'button'; button.setAttribute('role', 'tab');
    button.setAttribute('aria-selected', String(current === target));
    button.title = target;
    button.disabled = busy.has('session-select') || busy.has('session.new');
    button.addEventListener('click', () => run('session-select', () => enterSession(tab, tab.readOnly)));
    return button;
  });
  $('session-tabs').replaceChildren(...nodes);
}
async function enterSession(info, readOnly) {
  const target = sessionTarget(info);
  setSidebar(false);
  if (client.connection?.session === target) return;
  if (client.connection) { saveDrafts(); await client.disconnect(); }
  localError = null; selectedSession = info; timelineOffset = 0;
  selectedTerminal = null; selectedThread = 'process'; lastRenderedRows = []; workerDraftEdited = false;
  restoreDrafts(target);
  await client.connect(target, readOnly);
  opened.set(target, { ...info, readOnly });
  restoreRecipient(target); setPanel('conversation'); renderAll();
  $('session-tabs').querySelector('[aria-selected="true"]')?.scrollIntoView({ block: 'nearest', inline: 'nearest' });
  void client.startObservation();
}
function renderSessions() {
  const listed = new Set(sessions.map(sessionTarget));
  const rows = [...sessions, ...[...opened.values()].filter(item => !listed.has(sessionTarget(item)))];
  const nodes = rows.map(session => {
    const target = sessionTarget(session);
    const button = element('button', 'session-item'); button.type = 'button';
    button.disabled = ['connecting', 'disconnecting'].includes(client.status) || busy.has('session-select');
    button.setAttribute('aria-current', String(target === (client.connection?.session ?? sessionTarget(selectedSession))));
    const detail = session.cwd || session.phase || session.connection_state || '状态未提供';
    const sameName = sessions.some(item => item.name === session.name && sessionTarget(item) !== target);
    button.append(element('strong', '', session.name), element('small', 'mono', sameName ? `${detail} · ${target}` : detail));
    button.title = [session.name, session.cwd, target !== session.name ? target : ''].filter(Boolean).join('\n');
    button.addEventListener('click', () => run('session-select', async () => {
      if (opened.has(target)) return enterSession(opened.get(target), opened.get(target).readOnly);
      if (client.connection) { saveDrafts(); await client.disconnect(); }
      selectedSession = session; timelineOffset = 0; selectedTerminal = null; workerDraftEdited = false;
      localError = null; renderAll();
      setSidebar(false);
    }));
    return button;
  });
  if (!nodes.length) nodes.push(element('p', 'quiet', gateway.authenticated ? '暂无会话' : '未连接'));
  $('session-list').replaceChildren(...nodes);
  renderSessionTabs();
  show('session-pagination', sessionOffset > 0 || sessionsNext !== null);
  $('sessions-prev').disabled = sessionOffset === 0; $('sessions-next').disabled = sessionsNext === null;
}

const connectionNames = { disconnected: '未连接', connecting: '正在连接', connected: '已连接', paused: '连接需检查', disconnecting: '正在断开' };
const observationNames = { observing: '正在观察', paused: '观察已暂停', backoff: '断线退避', resyncing: '正在同步状态', unsupported: '无事件能力' };
function renderStatus() {
  const connected = Boolean(client.connection);
  setText('auth-status', gateway.authenticated ? '已设置认证' : '尚未认证');
  $('auth-dot').classList.toggle('good', gateway.authenticated);
  setText('connection-label', connected ? `SSMCP / ${client.capabilities?.mode === 'control' ? 'CONTROL' : 'OBSERVE'}` : 'WORKSPACE');
  const info = opened.get(client.connection?.session) ?? selectedSession;
  setText('session-title', info?.name ?? 'ssmcp');
  $('session-title').classList.toggle('sr-only', connected);
  setText('session-cwd', client.states.get('runtime')?.cwd ?? info?.cwd ?? '');
  show('new-session', gateway.authenticated);
  const ownership = gateway.ownedSessionStatus(currentTarget());
  show('end-session', Boolean(ownership));
  setText('end-session', ownership === 'unknown' ? '重试结束此会话' : '结束此会话');
  $('end-session').disabled = submittingEnd || busy.has('connect') || busy.has('disconnect') || busy.has('session-select')
    || ['connecting', 'disconnecting'].includes(client.status);
  show('take-control', client.connection?.read_only === true);
  $('take-control').disabled = client.status !== 'connected' || busy.has('connect') || busy.has('session-select') || busy.has('disconnect');
  $('new-session').disabled = busy.has('session.new') || busy.has('session-select');
  for (const button of $('session-tabs').children) button.disabled = busy.has('session.new') || busy.has('session-select');
  show('connect-strip', Boolean(selectedSession) && !connected);
  show('connected-actions', connected);
  $('connect-button').disabled = busy.has('connect') || client.status === 'connecting' || !gateway.authenticated;
  $('disconnect-button').disabled = busy.has('disconnect');
  $('refresh-state').disabled = busy.has('refresh') || !client.capabilities;
  $('refresh-sessions').disabled = busy.has('sessions') || !gateway.authenticated;
  document.querySelectorAll('.session-item').forEach(button => { button.disabled = ['connecting', 'disconnecting'].includes(client.status) || (busy.has('session-select') || busy.has('session.new')); });
  show('update-button', client.hasCommand('client.update'));
  $('update-button').disabled = busy.has('client.update');
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
  const chat = client.hasCommand('chat.send');
  const supported = chat || client.hasCommand('session.new') || client.hasCommand('session.cd');
  show('chat-form', supported);
  renderSlashOptions();
  fillAgents($('chat-target'), [['next', '下一个合法接收者']], $('chat-target').value, true);
  fillAgents($('thread-filter'), [['process', '整个会话'], ['main', '主对话']], selectedThread, true);
  const validTarget = $('chat-target').selectedOptions[0]?.disabled !== true;
  const input = $('chat-text').value;
  const command = sessionCommand(input);
  const canSend = command ? slashAvailable(command) : chat && threads?.can_send === true && validTarget;
  $('chat-send').disabled = !canSend || busy.has('chat.send') || busy.has('session.new') || busy.has('session.cd') || busy.has('session-select') || !input.trim() || byteLength(input) > 16_384;
  const note = !client.connection ? '' : !supported ? '只读会话' : !validTarget ? '请选择接收方。'
    : byteLength(input) > 16_384 ? '输入超过 16 KiB。'
    : !command && threads?.can_send !== true ? '正在等待会话接受输入。' : '';
  setText('composer-note', note); show('composer-note', Boolean(note));
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
  mark.setAttribute('aria-hidden', 'true');
  const body = element('div'); const heading = element('div', 'event-heading');
  heading.append(element('strong', '', row.title || row.kind), element('span', 'source', sourceLabel(row)));
  if (row.status) heading.append(element('span', `pill ${['unknown', 'confirmed', 'error', 'done'].includes(row.status) ? row.status : ''}`, row.kind === 'worker_snapshot' ? `最后观察 · ${row.status}` : row.kind === 'input' ? `队列 · ${row.status}` : stateLabels[row.status] ?? String(row.status)));

  const textNode = element('div', 'event-text', plain(row.text, 16384));
  body.append(heading, textNode);
  if (row.messageId) {
    article.tabIndex = 0;
    const reply = element('button', 'text-button reply-button', '引用');
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
    heading.append(reply);
  }
  const more = element('button', 'message-info', '···');
  more.type = 'button'; more.setAttribute('aria-label', '消息详情'); more.title = '消息详情';
  more.addEventListener('click', () => details('消息详情', {sequence: row.sequence, source: row.source, metadata: row.metadata}));
  heading.append(more);
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
  show('retention-note', Boolean(client.connection) && client.timeline.dropped > 0);
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
  const output = chunks.join('').slice(-65_536) || '等待终端输出…';
  const screen = $('terminal-output'); const bottom = screen.scrollHeight - screen.scrollTop - screen.clientHeight < 50;
  if (screen.textContent !== output) { screen.textContent = output; if (bottom) screen.scrollTop = screen.scrollHeight; }
  const writable = Boolean(selected) && client.hasCommand('terminal.write');
  show('terminal-form', writable);
  show('terminal-keys', writable && ['conversation', 'terminal'].includes(activePanel));
  $('terminal-keys').setAttribute('aria-label', '发送到终端 ' + (selected?.name || selectedTerminal || '') + ' 的快捷键');
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
  const counts = [['working', '工作中'], ['idle', '空闲'], ['offline', '离线'], ['queued', '排队任务'], ['dreaming', 'Dreaming'], ['retiring', '退役中'], ['registered_workers', '已登记'], ['total', '总容量']];
  if (Number.isSafeInteger(state.desired_total) && state.desired_total !== state.total) counts.push(['desired_total', '目标容量']);
  $('worker-summary').replaceChildren(...counts.map(([key, title]) => {
    const node = element('div', 'metric'); node.append(element('strong', '', state[key] ?? '—'), element('span', '', title)); return node;
  }));
  const productivity = state.productivity;
  const chart = $('worker-productivity');
  chart.textContent = Array.isArray(productivity?.plot) ? productivity.plot.join('\n') : '暂无调用记录';
  const samples = productivity?.calls_per_minute;
  const hasTrend = Array.isArray(samples) && samples.length === 10 && samples.every(count =>
    (typeof count === 'number' || (typeof count === 'string' && /^\d+$/.test(count))) &&
    Number.isSafeInteger(Number(count)) && Number(count) >= 0);
  show('worker-trend', hasTrend);
  show('worker-peak', hasTrend);
  show('worker-productivity', !hasTrend);
  if (hasTrend) {
    const peak = Math.max(...samples);
    const scale = Math.log1p(peak || 1);
    const points = samples.map((count, index) => `${2 + index * 176 / 9},${38 - 34 * Math.log1p(count) / scale}`);
    $('worker-trend-line').setAttribute('points', points.join(' '));
    $('worker-trend').querySelector('svg').setAttribute('aria-label',
      `最近 10 分钟工具调用，对数刻度。每分钟由早到晚：${samples.join('、')}`);
    setText('worker-peak', `峰值 ${peak}`);
  }
  show('worker-resize', client.hasCommand('workers.resize') && typeof state.pool_generation === 'string');
  if (!workerDraftEdited && document.activeElement !== $('worker-total')) $('worker-total').value = state.desired_total ?? '';
  $('worker-resize').querySelector('button').disabled = busy.has('workers.resize');
  const slots = Array.isArray(state.slots) ? state.slots.slice(0, 32) : [];
  const signature = JSON.stringify(slots);
  if ($('worker-list').dataset.signature !== signature) {
    $('worker-list').dataset.signature = signature;
    $('worker-list').replaceChildren(...slots.map(slot => {
      const card = element('article', 'worker-card'); const heading = element('div', 'worker-card-header');
      const station = Number.isSafeInteger(slot.slot) ? '#' + String(slot.slot + 1).padStart(2, '0') : '—';
      const status = slot.state === 'idle' && slot.display_state === 'dreaming' ? '☾ Dreaming' : slot.state || '未提供状态';
      heading.append(element('h3', '', `站点 ${station}`), element('span', 'pill', status));
      if (slot.quiet_10m === true || (slot.display_state === 'offline' && slot.state && slot.state !== 'offline')) {
        heading.append(element('span', 'pill', '10 分钟无工具调用'));
      }
      if (slot.retire_requested && slot.state !== 'retiring') heading.append(element('span', 'pill', '已请求退役'));
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

$('welcome-connect').addEventListener('click', showLaunch);
document.querySelectorAll('[data-close]').forEach(button => button.addEventListener('click', () => $(button.dataset.close).close()));
$('refresh-sessions').addEventListener('click', () => run('sessions', () => listSessions(sessionOffset)));
$('sessions-prev').addEventListener('click', () => run('sessions', () => listSessions(Math.max(0, sessionOffset - 16))));
$('sessions-next').addEventListener('click', () => run('sessions', () => listSessions(sessionsNext)));
$('connect-button').addEventListener('click', () => run('connect', async () => {
  await enterSession(selectedSession, $('connection-mode').value === 'observe');
}));
$('end-session').addEventListener('click', () => {
  const target = currentTarget();
  if ($('end-session').disabled || !gateway.ownedSessionStatus(target)) return;
  endTarget = target;
  setText('end-session-target', target); setText('end-session-error', '');
  $('end-session-dialog').showModal();
});
$('end-session-dialog').addEventListener('cancel', event => { if (submittingEnd) event.preventDefault(); });
$('end-session-dialog').addEventListener('close', () => { endTarget = null; });
$('end-session-confirm').addEventListener('click', async () => {
  const target = endTarget;
  if (submittingEnd || !target || target !== currentTarget() || !gateway.ownedSessionStatus(target)) return;
  submittingEnd = true;
  $('end-session-confirm').disabled = $('end-session-cancel').disabled = true;
  setText('end-session-error', '');
  try {
    await gateway.endOwnedSession(target);
    // A directory read begun before the close receipt cannot reintroduce this target.
    ++directoryRead; opened.delete(target); drafts.delete(target);
    sessions = sessions.filter(row => sessionTarget(row) !== target);
    const closingCurrent = currentTarget() === target;
    let detachError;
    if (client.connection?.session === target) {
      try { await client.disconnect(); } catch (error) { detachError = error; }
    }
    if (sessionTarget(selectedSession) === target && (!client.connection || client.connection.session === target)) {
      selectedSession = null; selectedTerminal = null; selectedThread = 'process'; timelineOffset = 0; lastRenderedRows = [];
      workerDraftEdited = false; queueOffset = 0; client.timeline.clear(); restoreDrafts(null);
    }
    if (closingCurrent) localError = detachError ? new Error('会话已结束。前端连接释放回执未确认。') : null;
    $('end-session-dialog').close(); renderAll();
  } catch (error) {
    if (!gateway.ownedSessionStatus(target)) { $('end-session-dialog').close(); return; }
    setText('end-session-error', error.effect === 'may_have_occurred'
      ? '结束结果未确定。可对上方同一会话重试，或取消并稍后核对。' : error.message);
  } finally {
    submittingEnd = false; $('end-session-confirm').disabled = $('end-session-cancel').disabled = false;
    renderStatus();
  }
});
$('take-control').addEventListener('click', () => run('connect', async () => {
  const info = opened.get(client.connection?.session);
  if (!info || !client.connection.read_only) return;
  saveDrafts();
  await client.disconnect();
  selectedSession = info; $('connection-mode').value = 'control';
  await enterSession(info, false);
}));
$('disconnect-button').addEventListener('click', () => run('disconnect', async () => { saveDrafts(); await client.disconnect(); renderAll(); }));
$('observe-button').addEventListener('click', () => run('observe', async () => {
  if (client.observation === 'paused') { await client.pauseObservation(); localError = null; void client.startObservation(); }
  else await client.pauseObservation();
}));
$('update-button').addEventListener('click', () => {
  const context = client.context();
  setText('details-title', 'SSMCP 更新');
  const body = $('details-content');
  const result = element('pre', '');
  const actions = element('div', 'form-actions');
  for (const [check, label] of [[true, '检查更新'], [false, '安装更新']]) {
    const button = element('button', check ? 'secondary' : 'primary', label);
    button.addEventListener('click', () => run('client.update', async () => {
      if (!client.current(context) || !client.hasCommand('client.update')) return;
      for (const child of actions.children) child.disabled = true;
      result.textContent = check ? '正在检查更新…' : '正在更新会话所在设备…';
      try {
        const record = await client.command('client.update', { check });
        if (!client.current(context)) return;
        if (record.status !== 'confirmed') {
          result.textContent = record.error?.message ?? '更新结果未确认。';
          return;
        }
        const receipt = record.receipt;
        result.textContent = [receipt?.summary, receipt?.details].filter(value => typeof value === 'string' && value).join('\n\n') || '更新回执未提供说明。';
      } finally {
        for (const child of actions.children) child.disabled = false;
      }
    }));
    actions.append(button);
  }
  body.replaceChildren(element('p', 'muted', '此操作在会话所在设备运行。'), actions, result);
  if (!$('details-dialog').open) $('details-dialog').showModal();
});
$('refresh-state').addEventListener('click', () => run('refresh', async () => { localError = null; await client.refresh(); renderAll(); }));
$('menu-button').addEventListener('click', () => setSidebar(!$('sidebar').classList.contains('is-open')));
for (const id of ['close-menu', 'dismiss-sidebar']) $(id).addEventListener('click', () => setSidebar(false));
document.addEventListener('keydown', event => {
  if (event.key === 'Escape' && $('sidebar').classList.contains('is-open')) setSidebar(false);
});
$('theme-button').addEventListener('click', () => {
  systemTheme.onchange = null;
  const root = document.documentElement;
  root.dataset.theme = root.dataset.theme === 'dark' ? 'light' : 'dark';
});

const tabButtons = [...document.querySelectorAll('[data-panel]')];
function setPanel(name) {
  activePanel = name; setSidebar(false); renderTerminals();
  tabButtons.forEach(button => { const active = button.dataset.panel === name; button.setAttribute('aria-selected', String(active)); button.tabIndex = active ? 0 : -1; show(`panel-${button.dataset.panel}`, active); });
  if (name === 'conversation') renderTimeline();
  if (name === 'waits') { renderWaits(); void refreshWaits(); }
}
tabButtons.forEach(button => {
  button.addEventListener('click', () => setPanel(button.dataset.panel));
  button.addEventListener('keydown', event => {
    if (!['ArrowUp', 'ArrowDown', 'Home', 'End'].includes(event.key)) return;
    event.preventDefault(); const index = tabButtons.indexOf(button);
    const next = event.key === 'Home' ? 0 : event.key === 'End' ? tabButtons.length - 1 : (index + (event.key === 'ArrowDown' ? 1 : -1) + tabButtons.length) % tabButtons.length;
    setPanel(tabButtons[next].dataset.panel);
    if (!mobileNavigation.matches) tabButtons[next].focus();
  });
});
$('thread-filter').addEventListener('change', () => { selectedThread = $('thread-filter').value; timelineOffset = 0; lastRenderedRows = []; renderTimeline({ preservePage: false }); });
$('timeline-older').addEventListener('click', () => { timelineOffset += 40; renderTimeline({ preservePage: false }); $('timeline-scroller').scrollTop = 0; });
$('timeline-newer').addEventListener('click', () => { timelineOffset = Math.max(0, timelineOffset - 40); renderTimeline({ preservePage: false }); });
$('new-events').addEventListener('click', () => { timelineOffset = 0; lastRenderedRows = []; renderTimeline({ preservePage: false }); $('timeline-scroller').scrollTop = $('timeline-scroller').scrollHeight; show('new-events', false); });
$('timeline-scroller').addEventListener('scroll', () => { const scroll = $('timeline-scroller'); if (timelineOffset === 0 && scroll.scrollHeight - scroll.scrollTop - scroll.clientHeight < 90) show('new-events', false); }, { passive: true });
$('history-more').addEventListener('click', () => run('history', () => client.loadHistory()));

const chatInput = $('chat-text');
const slashList = $('slash-options');
let selectedSlash = null;
let slashDismissed = false;

function slashAvailable(command) {
  return command.name === 'session.new' ? gateway.authenticated : client.hasCommand(command.name);
}
function slashWord() {
  if (chatInput.selectionStart !== chatInput.selectionEnd || /[\r\n]/.test(chatInput.value)) return null;
  const match = /^(\s*)(\/\S*)/.exec(chatInput.value);
  if (!match) return null;
  const start = match[1].length, end = match[0].length, caret = chatInput.selectionStart;
  return caret > start && caret <= end ? { start, end, prefix: chatInput.value.slice(start, caret) } : null;
}
function renderSlashOptions() {
  const query = !slashDismissed && !composing.has(chatInput) && !$('chat-form').hidden
    && document.activeElement === chatInput && slashWord();
  const choices = query ? slashCommands.filter(item => item.word.startsWith(query.prefix) && slashAvailable(item)) : [];
  if (!choices.includes(selectedSlash)) selectedSlash = choices[0] ?? null;
  slashList.hidden = !selectedSlash;
  for (const [index, item] of slashCommands.entries()) {
    const option = slashList.children[index];
    option.hidden = !choices.includes(item);
    option.setAttribute('aria-selected', String(item === selectedSlash));
    if (item === selectedSlash) chatInput.setAttribute('aria-activedescendant', option.id);
  }
  if (!selectedSlash) chatInput.removeAttribute('aria-activedescendant');
  return choices;
}
function completeSlash(item) {
  const query = slashWord();
  if (!query || !item.word.startsWith(query.prefix) || !slashAvailable(item)) return;
  const separator = /^[ \t]+/.exec(chatInput.value.slice(query.end))?.[0] ?? '';
  const replacement = item.word + (item.takesPath && !separator ? ' ' : '');
  const size = byteLength(chatInput.value.slice(0, query.start) + replacement + chatInput.value.slice(query.end));
  if (size > 16_384) return;
  chatInput.setRangeText(replacement, query.start, query.end, 'end');
  const caret = chatInput.selectionEnd + separator.length;
  chatInput.setSelectionRange(caret, caret);
  chatInput.focus({ preventScroll: true });
  slashDismissed = true;
  renderComposer();
}
for (const item of slashCommands) {
  const option = element('button', 'slash-option');
  option.type = 'button'; option.tabIndex = -1;
  option.id = 'slash-option-' + item.word.slice(1);
  option.setAttribute('role', 'option');
  option.append(element('code', '', item.word), element('span', '', item.description));
  // Keep the caret (and the mobile keyboard) in the draft while choosing.
  option.addEventListener('pointerdown', event => event.preventDefault());
  option.addEventListener('click', () => completeSlash(item));
  slashList.append(option);
}
chatInput.addEventListener('focus', () => { slashDismissed = false; renderSlashOptions(); });
chatInput.addEventListener('blur', () => { slashDismissed = true; renderSlashOptions(); });
chatInput.addEventListener('selectionchange', renderSlashOptions);
chatInput.addEventListener('input', () => { slashDismissed = false; });
chatInput.addEventListener('keydown', event => {
  if (event.isComposing || event.keyCode === 229 || composing.has(chatInput)
      || event.ctrlKey || event.metaKey || event.altKey || event.shiftKey) return;
  if (!['ArrowDown', 'ArrowUp', 'Tab', 'Enter', 'Escape'].includes(event.key)) return;
  const choices = renderSlashOptions();
  if (!choices.length) return;
  event.preventDefault();
  if (event.key === 'Tab' || event.key === 'Enter') { completeSlash(selectedSlash); return; }
  if (event.key === 'Escape') slashDismissed = true;
  else {
    const step = event.key === 'ArrowDown' ? 1 : -1;
    selectedSlash = choices[(choices.indexOf(selectedSlash) + step + choices.length) % choices.length];
  }
  renderSlashOptions();
});
for (const [textareaId, formId] of [['chat-text', 'chat-form'], ['terminal-input', 'terminal-form']]) {
  const textarea = $(textareaId);
  textarea.addEventListener('input', renderComposer);
  textarea.addEventListener('compositionstart', () => { composing.add(textarea); renderSlashOptions(); });
  textarea.addEventListener('compositionend', () => { composing.delete(textarea); renderComposer(); });
  textarea.addEventListener('keydown', event => {
    if (event.key === 'Enter' && (event.ctrlKey || event.metaKey) && !event.isComposing && event.keyCode !== 229 && !composing.has(textarea)) {
      event.preventDefault(); if (!$(formId).querySelector('button[type="submit"]').disabled) $(formId).requestSubmit();
    }
  });
}
$('chat-target').addEventListener('change', renderComposer); $('terminal-newline').addEventListener('change', renderComposer);
function sessionCommand(text) {
  const match = /^(\/\S+)(?:[ \t]+([^\r\n]*))?$/.exec(text.trim());
  const item = match && slashCommands.find(command => command.word === match[1]);
  return item ? { name: item.name, argument: (match[2] ?? '').trim() } : null;
}
async function applySessionCommand(command, textarea) {
  if (command.name === 'session.new') {
    if (command.argument) throw new Error('用法：/new');
    showLaunch(); return;
  }
  if (!command.argument) throw new Error('用法：/cd 路径');
  const context = client.context();
  const record = await send(command.name, { path: command.argument }, textarea);
  if (record.status !== 'confirmed' || !client.current(context)) return;
  const cwd = record.receipt?.cwd;
  if (typeof cwd !== 'string' || !cwd) throw new Error('目录回执缺少路径。');
  selectedSession.cwd = cwd; setText('session-cwd', cwd);
}
$('new-session').addEventListener('click', showLaunch);
$('chat-form').addEventListener('submit', event => {
  event.preventDefault(); if ($('chat-send').disabled || composing.has($('chat-text'))) return;
  const command = sessionCommand($('chat-text').value);
  if (command) void run(command.name, () => applySessionCommand(command, $('chat-text')));
  else void run('chat.send', () => send('chat.send', { text: $('chat-text').value, target: $('chat-target').value }, $('chat-text')));
});
const keyBytes = { escape: '\x1b', tab: '\t', interrupt: '\x03', left: '\x1b[D', down: '\x1b[B', up: '\x1b[A', right: '\x1b[C' };
$('terminal-keys').addEventListener('click', event => {
  const button = event.target.closest('button[data-key]');
  if (!button || !selectedTerminal || !client.hasCommand('terminal.write')) return;
  void run('terminal.write', () => send('terminal.write', { terminal_id: selectedTerminal, data: keyBytes[button.dataset.key] }));
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
function hostedRelay() {
  const relay = $('launch-relay').value.trim();
  try {
    const url = new URL(relay.includes('://') ? relay : 'https://' + relay);
    return url.hostname.toLowerCase().replace(/\.+$/, '') === 'ssmcp.lmm.best';
  } catch { return false; }
}
function consentStep(visible) {
  show('launch-hosted', visible);
  for (const id of ['launch-terms', 'launch-privacy']) {
    $(id).required = visible;
    if (!visible) $(id).checked = false;
  }
}
$('launch-relay').addEventListener('input', () => consentStep(false));

function showLaunch() {
  show('launch-entry', !gateway.authenticated);
  show('workbench-shell', false);
  show('launch-setup', gateway.authenticated);
  if (!gateway.authenticated) {
    setText('entry-hint', modulePage ? '请从 Magisk 管理器的 SSMCP 模块执行操作。'
      : /Android|iPhone|iPad/.test(navigator.userAgent) ? '请从设备上的 SSMCP 应用打开。' : '请在本机运行 ssmcp gui open。');
    return;
  }
  if (!gateway.pendingOwnedRequest) consentStep(false);
  const manualList = !modulePage && Boolean(launchOptions(location)?.session);
  show('launch-back', opened.size > 0 || manualList);
  setText('launch-back', opened.size ? '返回工作台' : '浏览现有会话');
  setText('launch-error', gateway.pendingOwnedRequest ? '上次连接结果未确定。' : '');
  updateLaunchControls(false);
  if (!$('launch-name').disabled) $('launch-name').focus({preventScroll:true});
}
function updateLaunchControls(sending) {
  const pending = Boolean(gateway.pendingOwnedRequest);
  for (const id of ['launch-name', 'launch-relay', 'launch-terms', 'launch-privacy']) $(id).disabled = sending || pending;
  $('launch-connect').disabled = sending;
  $('launch-back').disabled = sending || (pending && !opened.size);
  show('launch-request-id', pending);
  setText('launch-request-id', pending ? '请求 ID ' + gateway.pendingOwnedRequest.request_id : '');
  setText('launch-connect', pending ? '核对上次创建' : '连接');
}
$('launch-back').addEventListener('click', async () => {
  if ($('launch-back').disabled || !gateway.authenticated || (modulePage && !opened.size)) return;
  updateLaunchControls(true);
  try {
    if (!opened.size) await listSessions();
    show('launch-setup', false); show('workbench-shell', true); renderAll();
  } catch (error) { setText('launch-error', error.message); }
  finally { updateLaunchControls(false); }
});
$('launch-form').addEventListener('submit', async event => {
  event.preventDefault();
  if (!gateway.authenticated || $('launch-connect').disabled) return;
  const args = { destination: $('launch-name').value.trim() + '@' + $('launch-relay').value.trim(),
    terms: $('launch-terms').checked, privacy: $('launch-privacy').checked };
  updateLaunchControls(true); setText('launch-error', '');
  try {
    const target = await gateway.createOwnedSession(args);
    const info = { name: args.destination, target, readOnly: true };
    opened.set(target, info);
    show('launch-setup', false); show('workbench-shell', true);
    selectedSession = info; $('connection-mode').value = 'observe'; renderAll();
    await enterSession(info, true);
  } catch (error) {
    if ($('launch-setup').hidden) { fail(error); renderAll(); }
    else {
      // Request policy acknowledgement only when the hosted relay explicitly requires it.
      const needsPolicy = hostedRelay() && error.effect === 'none' && error.status === 400
        && ['invalid', 'invalid_request'].includes(error.code)
        && error.message === "confirm the hosted relay's terms and privacy policy";
      if (needsPolicy) {
        consentStep(true);
        updateLaunchControls(false);
        $('launch-terms').focus();
      }
      const message = error.effect === 'may_have_occurred' ? '创建结果未确定，请点“核对上次创建”重试原请求。'
        : error.status === 401 ? '本次授权已失效，请从原入口重新打开。'
        : needsPolicy ? '请确认该服务的用户协议和隐私政策。' : error.message;
      setText('launch-error', message);
    }
  } finally { updateLaunchControls(false); }
});
renderAll();
showLaunch();
// Android delivers its in-memory bearer to this exact origin via WebView.postWebMessage.
// Ordinary window.postMessage events have a source window and cannot complete this handoff.
if (!gateway.authenticated && !modulePage && window.top === window && location.pathname === '/'
    && !location.search && /Android/.test(navigator.userAgent) && /; wv\)/.test(navigator.userAgent)) {
  function acceptNativePairing(event) {
    if (!event.isTrusted || event.source !== null || typeof event.data !== 'string'
        || event.data.length !== 64 || !/^[0-9a-f]{64}$/.test(event.data)) return;
    window.removeEventListener('message', acceptNativePairing);
    gateway.setToken(event.data);
    renderAll(); showLaunch();
    document.documentElement.dataset.ssmcpPairingReady = 'paired';
  }
  window.addEventListener('message', acceptNativePairing);
  document.documentElement.dataset.ssmcpPairingReady = '1';
}
