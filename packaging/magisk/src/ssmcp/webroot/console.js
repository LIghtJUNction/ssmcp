import { el, hasCredential, terminal as localTerminal, usesOfficialRelay } from './device-api.js';
import { hasManagerTransport, managerTerminal, openManagerSetup } from './module-entry.js';

const field = el('input');
const screen = el('screen');
const direct = el('direct-input');
const cursor = el('terminal-cursor');
const plugin = el('plugin');
const ctrl = el('keys').querySelector('[data-key=Ctrl]');
const defaultPlaceholder = field.placeholder;
const inManager = !hasCredential && hasManagerTransport();
const terminal = (action, tab, args = {}) => inManager
  ? managerTerminal({ action, tab, ...args })
  : localTerminal(action, tab, args);

let setupControl = el('open-setup');
if (inManager) {
  const button = document.createElement('button');
  button.type = 'button';
  button.id = setupControl.id;
  button.className = setupControl.className;
  button.textContent = setupControl.textContent;
  button.hidden = true;
  button.onclick = async () => {
    if (button.disabled) return;
    button.disabled = true;
    try { await openManagerSetup(); }
    catch (error) { status(`无法打开安装引导：${error.message}`); }
    finally { button.disabled = false; }
  };
  setupControl.replaceWith(button);
  setupControl = button;
}

function setView(view) {
  document.body.dataset.view = view;
  el('console').hidden = view !== 'console';
  setupControl.hidden = true;
}
let statusSource = '';
function status(message, source = 'action') {
  statusSource = message ? source : '';
  el('status').textContent = message;
}

let selected = null;
let generation = 0;
let tabs = [];
let changingTabs = false;
let listRequest = 0;
const drafts = new Map();
const writes = new Map();
const submitting = new Set();
const isCurrent = version => version === generation && document.body.dataset.view === 'console';
function setConnection(state, message, detail = '') {
  const indicator = el('connection');
  indicator.dataset.state = state;
  indicator.title = detail;
  if (indicator.textContent !== message) indicator.textContent = message;
}
function setCtrl(armed) {
  ctrl.setAttribute('aria-pressed', String(armed));
  field.placeholder = armed ? '输入一个英文字母，再发送' : defaultPlaceholder;
}
function updateSend() {
  el('send').disabled = !field.value || submitting.has(selected);
}
function selectTab(id) {
  if (selected === id) return;
  direct.blur();
  directMode = false;
  screenCursor = null;
  cursor.hidden = true;
  selected = id;
  generation++;
  if (!drafts.has(id)) drafts.set(id, { text: '', edits: 0 });
  field.value = drafts.get(id).text;
  setCtrl(false);
  updateSend();
  status('');
  screen.textContent = '';
  screen.scrollTop = screen.scrollLeft = 0;
  setConnection('reading', '读取会话…');
  idleReads = 0;
  readAgain = false;
  scheduleResize();
  poll();
}
function tabButton(label, action) {
  const button = document.createElement('button');
  button.type = 'button';
  button.textContent = label;
  button.disabled = changingTabs;
  button.onclick = action;
  return button;
}
function renderTabs() {
  const buttons = tabs.map(id => {
    const button = tabButton(id === 0 ? '启动会话' : `会话 ${id + 1}`, () => {
      if (id === selected) return;
      selectTab(id);
      const version = generation;
      for (const tab of el('tabs').querySelectorAll('[data-tab]')) {
        tab.setAttribute('aria-pressed', String(Number(tab.dataset.tab) === id));
      }
      const close = el('tabs').querySelector('[data-action=close]');
      if (close) close.disabled = id === 0;
      refreshTabs(id).catch(error => { if (isCurrent(version)) status(error.message); });
    });
    button.dataset.tab = String(id);
    button.setAttribute('aria-pressed', String(id === selected));
    return button;
  });
  const open = tabButton('新建', () => changeTabs('open'));
  open.dataset.action = 'open';
  open.disabled = open.disabled || tabs.length >= 8;
  buttons.push(open);
  if (selected !== 0 || tabs.length > 1) {
    const close = tabButton('关闭', () => changeTabs('close'));
    close.dataset.action = 'close';
    close.disabled = close.disabled || selected === 0;
    buttons.push(close);
  }
  el('tabs').textContent = '';
  el('tabs').append(...buttons);
}
async function refreshTabs(preferred = selected) {
  const request = ++listRequest;
  const result = await terminal('list', selected ?? 0);
  if (request !== listRequest) return;
  if (!result.target) {
    selected = null;
    generation++;
    drafts.clear();
    tabs = [];
    setView('notice');
    setupControl.hidden = false;
    status('此设备尚未完成首次配置。');
    return;
  }
  el('target').textContent = result.target;
  el('mcp').textContent = result.mcp;
  el('traffic').hidden = !usesOfficialRelay(result.mcp);
  const changed = tabs.length !== result.tabs.length || tabs.some((id, index) => result.tabs[index] !== id);
  tabs = result.tabs;
  for (const id of drafts.keys()) if (!tabs.includes(id)) drafts.delete(id);
  if (document.body.dataset.view !== 'console') plugin.open = false;
  setView('console');
  selectTab(tabs.includes(preferred) ? preferred : tabs[0]);
  if (changed) renderTabs();
}
async function changeTabs(action) {
  if (changingTabs || (action === 'close' && selected === 0)) return;
  changingTabs = true;
  listRequest++;
  renderTabs();
  try {
    if (action === 'close' && writes.has(selected)) await writes.get(selected);
    const result = await terminal(action, selected);
    await refreshTabs(action === 'open' ? result.tab : selected);
  } catch (error) {
    status(error.message);
  } finally {
    changingTabs = false;
    renderTabs();
  }
}

let grid;
let screenCursor;
function positionCursor() {
  const point = screenCursor;
  if (!grid || !point?.visible || !Number.isInteger(point.col) || !Number.isInteger(point.row)) {
    cursor.hidden = true;
    return;
  }
  const left = grid.left + point.col * grid.width - screen.scrollLeft;
  const top = grid.top + point.row * grid.height - screen.scrollTop;
  cursor.hidden = point.col < 0 || point.row < 0 || left < 0 || top < 0 ||
    left >= screen.clientWidth || top >= screen.clientHeight;
  if (!cursor.hidden) {
    const style = screen.parentElement.style;
    style.setProperty('--cursor-x', `${screen.offsetLeft + screen.clientLeft + left}px`);
    style.setProperty('--cursor-y', `${screen.offsetTop + screen.clientTop + top}px`);
  }
}
screen.addEventListener('scroll', positionCursor, { passive: true });
function dimensions() {
  // An out-of-flow probe inherits the exact terminal font, spacing and line height.
  const probe = document.createElement('span');
  probe.setAttribute('aria-hidden', 'true');
  Object.assign(probe.style, {
    position: 'fixed', top: '0', left: '0', visibility: 'hidden', whiteSpace: 'pre',
    width: 'max-content', pointerEvents: 'none',
  });
  probe.textContent = '0'.repeat(64) + '\n0';
  screen.append(probe);
  const cell = probe.getBoundingClientRect();
  probe.remove();
  const style = getComputedStyle(screen);
  const left = parseFloat(style.paddingLeft);
  const top = parseFloat(style.paddingTop);
  const width = screen.clientWidth - left - parseFloat(style.paddingRight);
  const height = screen.clientHeight - top - parseFloat(style.paddingBottom);
  if (Math.min(width, height, cell.width, cell.height) <= 0) return null;
  grid = { left, top, width: cell.width / 64, height: cell.height / 2 };
  screen.parentElement.style.setProperty('--cell-width', grid.width + 'px');
  screen.parentElement.style.setProperty('--cell-height', grid.height + 'px');
  positionCursor();
  return {
    cols: Math.max(32, Math.min(200, Math.floor(width / grid.width))),
    rows: Math.max(10, Math.min(80, Math.floor(height / grid.height))),
  };
}
let sizeTimer;
let resizing = false;
let resizeAgain = false;
let lastSize = '';
function scheduleResize() {
  clearTimeout(sizeTimer);
  sizeTimer = setTimeout(resize, 40);
}
async function resize() {
  const version = generation;
  if (!isCurrent(version) || selected === null) return;
  if (resizing) { resizeAgain = true; return; }
  const size = dimensions();
  if (!size) return;
  const key = `${version}/${size.cols}/${size.rows}`;
  if (lastSize === key) return;
  resizing = true;
  try {
    await terminal('resize', selected, size);
    lastSize = key;
  } catch (error) {
    if (isCurrent(version)) status(error.message);
  } finally {
    resizing = false;
    if (resizeAgain) { resizeAgain = false; scheduleResize(); }
  }
}
function viewportChanged() {
  const viewport = window.visualViewport;
  if (viewport && viewport.scale === 1) {
    document.documentElement.style.setProperty('--viewport-height', `${viewport.height}px`);
  }
  scheduleResize();
}
window.addEventListener('resize', viewportChanged);
window.visualViewport?.addEventListener('resize', viewportChanged);
if (window.ResizeObserver) new ResizeObserver(scheduleResize).observe(screen);
document.fonts?.ready.then(scheduleResize);
document.fonts?.addEventListener('loadingdone', scheduleResize);
viewportChanged();

let pollTimer;
let polling = -1;
let idleReads = 0;
let readAgain = false;
let lastRead = 0;
function inputActivity(submitted = false) {
  const wasIdle = inManager && idleReads > 2;
  idleReads = 0;
  if ((!submitted && !wasIdle) || document.hidden || !isCurrent(generation)) return;
  if (polling === generation) { readAgain = true; return; }
  clearTimeout(pollTimer);
  pollTimer = setTimeout(poll, Math.max(30, 180 - (performance.now() - lastRead)));
}
async function poll() {
  clearTimeout(pollTimer);
  const version = generation;
  if (!isCurrent(version) || document.hidden || polling === version) return;
  polling = version;
  lastRead = performance.now();
  try {
    const snapshot = await terminal('screen', selected);
    if (!isCurrent(version)) return;
    const next = snapshot.exited ? null : snapshot.cursor;
    const moved = next?.row !== screenCursor?.row || next?.col !== screenCursor?.col || next?.visible !== screenCursor?.visible;
    if (snapshot.screen === screen.textContent) idleReads = moved ? 0 : idleReads + 1;
    else { screen.textContent = snapshot.screen; idleReads = 0; }
    screenCursor = next;
    positionCursor();
    if (snapshot.exited) setConnection('exited', '会话已退出');
    else if (snapshot.connected_once) setConnection('first', '已完成首次连接', '模块记录了首次中继连接。当前连接状态未知。');
    else setConnection('waiting', '等待首次连接');
    if (statusSource === 'poll') status('');
  } catch (error) {
    if (isCurrent(version)) {
      idleReads++;
      setConnection('error', '读取失败', error.message);
      if (!statusSource || statusSource === 'poll') status(error.message, 'poll');
    }
  } finally {
    if (polling === version) polling = -1;
    if (isCurrent(version) && !document.hidden) {
      const interval = inManager ? Math.min(1800, 450 + Math.max(0, idleReads - 2) * 250) : 450;
      pollTimer = setTimeout(poll, readAgain ? 60 : interval);
      readAgain = false;
    }
  }
}
let checkingEntry = false;
async function recheckEntry() {
  if (checkingEntry || document.hidden || setupControl.hidden) return;
  checkingEntry = true;
  try { await refreshTabs(); } catch (error) { status(error.message); }
  finally { checkingEntry = false; }
}
window.addEventListener('focus', recheckEntry);
document.addEventListener('visibilitychange', () => {
  if (document.hidden) clearTimeout(pollTimer);
  else if (!setupControl.hidden) recheckEntry();
  else { idleReads = 0; viewportChanged(); poll(); }
});
document.addEventListener('click', event => {
  if (plugin.open && !plugin.contains(event.target)) plugin.open = false;
});
document.addEventListener('keydown', event => {
  if (event.key !== 'Escape' || !plugin.open) return;
  const returnFocus = plugin.contains(document.activeElement);
  event.preventDefault();
  event.stopPropagation();
  plugin.open = false;
  if (returnFocus) plugin.querySelector('summary').focus();
}, true);

const sequences = {
  Enter: '\r', Escape: '\x1b', Tab: '\t', Backspace: '\x7f',
  ArrowUp: '\x1b[A', ArrowDown: '\x1b[B', ArrowRight: '\x1b[C', ArrowLeft: '\x1b[D',
};
const directSequences = {
  ...sequences, Home: '\x1b[H', End: '\x1b[F', Delete: '\x1b[3~',
  Insert: '\x1b[2~', PageUp: '\x1b[5~', PageDown: '\x1b[6~',
};
async function write(tab, text) {
  // Keep rapid key presses in order, tied to the tab that received the gesture.
  const previous = writes.get(tab) || Promise.resolve();
  const pending = previous.then(
    () => terminal('input', tab, { text }),
    () => { throw Error('上一条输入失败，后续排队的输入均未发送。'); },
  );
  writes.set(tab, pending);
  try { await pending; if (tab === selected) inputActivity(true); }
  finally { if (writes.get(tab) === pending) writes.delete(tab); }
}
function shortcut(text) {
  const version = generation;
  if (!isCurrent(version) || changingTabs) return;
  write(selected, text).then(() => {
    if (isCurrent(version) && statusSource === 'write') status('');
  }).catch(error => { if (isCurrent(version)) status(error.message, 'write'); });
}

// A sentinel makes a backspace observable on mobile keyboards even with no draft.
const sentinel = '\u200b';
let directVersion = -1;
let directMode = false;
let preedit = false;
let preeditEnded = -Infinity;
let commitTimer;
function resetDirect() {
  direct.value = sentinel;
  direct.setSelectionRange(1, 1);
}
function directText(text) {
  if (!text || directVersion !== generation) return;
  if (ctrl.getAttribute('aria-pressed') === 'true') {
    setCtrl(false);
    if (!/^[a-z]$/i.test(text)) return status('Ctrl 组合键需要一个英文字母。');
    shortcut(String.fromCharCode(text.toUpperCase().charCodeAt(0) & 31));
    return;
  }
  const bytes = text.replace(/\r\n|\n/g, '\r');
  const encoder = new TextEncoder();
  if (encoder.encode(bytes).length <= 4096) { shortcut(bytes); return; }
  let chunk = '';
  let length = 0;
  for (const character of bytes) {
    const size = encoder.encode(character).length;
    if (length + size > 4096) { shortcut(chunk); chunk = ''; length = 0; }
    chunk += character;
    length += size;
  }
  if (chunk) shortcut(chunk);
}
function commitDirect() {
  if (directVersion !== generation || preedit) return;
  const text = direct.value.startsWith(sentinel) ? direct.value.slice(1) : direct.value;
  resetDirect();
  directText(text);
}
direct.addEventListener('focus', () => {
  directMode = true;
  directVersion = generation;
  resetDirect();
});
direct.addEventListener('blur', () => {
  directVersion = -1;
  preedit = false;
  clearTimeout(commitTimer);
  resetDirect();
});
screen.addEventListener('click', () => {
  if (window.getSelection()?.isCollapsed !== false) direct.focus({ preventScroll: true });
});
direct.addEventListener('compositionstart', () => { preedit = true; clearTimeout(commitTimer); });
direct.addEventListener('compositionend', () => {
  preedit = false;
  preeditEnded = performance.now();
  // Some IMEs dispatch the final input before this event and others dispatch it after.
  commitTimer = setTimeout(commitDirect, 0);
});
direct.addEventListener('input', event => {
  if (preedit || event.isComposing || directVersion !== generation) return;
  if (event.inputType === 'deleteContentBackward' && !direct.value) shortcut(sequences.Backspace);
  commitDirect();
});
function directKey(event) {
  if (event.isComposing || preedit || event.keyCode === 229 || event.metaKey) return;
  if (event.key === 'Tab' && (event.shiftKey || event.currentTarget === screen)) return;
  if (event.ctrlKey && event.key.toLowerCase() === 'v') return;
  if (event.ctrlKey && event.key.toLowerCase() === 'c' && window.getSelection()?.isCollapsed === false) return;
  if (event.key === 'Enter' && performance.now() - preeditEnded < 50) { event.preventDefault(); return; }
  let bytes = directSequences[event.key];
  if (event.ctrlKey && !event.altKey && /^[a-z@[\\\]^_]$/i.test(event.key)) {
    bytes = String.fromCharCode(event.key.toUpperCase().charCodeAt(0) & 31);
  }
  if (event.ctrlKey && event.key === ' ') bytes = '\x00';
  if (event.altKey && !event.ctrlKey && event.key.length === 1) bytes = '\x1b' + event.key;
  if (event.currentTarget === screen && (bytes || event.key.length === 1)) {
    direct.focus({ preventScroll: true });
    if (!bytes) { event.preventDefault(); directText(event.key); return; }
  }
  if (bytes && directVersion === generation) { event.preventDefault(); shortcut(bytes); }
}
direct.addEventListener('keydown', directKey);
screen.addEventListener('keydown', directKey);
field.addEventListener('focus', () => { directMode = false; });

async function submit(allowEmpty = false) {
  const tab = selected;
  const draft = drafts.get(tab);
  if (!draft || changingTabs || submitting.has(tab) || (!allowEmpty && !field.value)) return;
  const text = field.value;
  const edits = draft.edits;
  const armed = ctrl.getAttribute('aria-pressed') === 'true';
  if (armed && !/^[a-z]$/i.test(text)) return status('Ctrl 组合键需要一个英文字母。');
  const bytes = armed ? String.fromCharCode(text.toUpperCase().charCodeAt(0) & 31) : text + '\r';
  if (new TextEncoder().encode(bytes).length > 4096) return status('单次输入最多 4096 字节，请缩短后再发送。');
  submitting.add(tab);
  updateSend();
  try {
    await write(tab, bytes);
    if (draft.edits === edits) {
      draft.text = '';
      if (tab === selected) field.value = '';
    }
    if (tab === selected) { setCtrl(false); status(''); }
  } catch (error) {
    if (tab === selected) status(`发送失败：${error.message}`);
  } finally {
    submitting.delete(tab);
    updateSend();
  }
}
ctrl.onclick = () => {
  setCtrl(ctrl.getAttribute('aria-pressed') !== 'true');
  (directMode ? direct : field).focus({ preventScroll: true });
};
for (const control of [el('keys'), el('send')]) {
  control.addEventListener('mousedown', event => {
    if (document.activeElement === field || document.activeElement === direct) event.preventDefault();
  });
}
for (const key of el('keys').querySelectorAll('button:not([data-key=Ctrl])')) {
  key.onclick = event => {
    if (key.dataset.key === 'Enter' && !directMode) { if (event.detail < 2) submit(true); }
    else shortcut(sequences[key.dataset.key]);
  };
}
el('send').onclick = () => submit();
field.addEventListener('input', () => {
  const draft = drafts.get(selected);
  if (draft) { draft.text = field.value; draft.edits++; }
  updateSend();
  inputActivity();
});
let composing = false;
let compositionEnded = -Infinity;
let enterPressed = -Infinity;
field.addEventListener('compositionstart', () => { composing = true; });
field.addEventListener('compositionend', () => { composing = false; compositionEnded = performance.now(); });
field.addEventListener('keydown', event => {
  if (composing || event.isComposing || event.keyCode === 229 || event.metaKey || event.altKey) return;
  if (event.key === 'Enter') {
    event.preventDefault();
    enterPressed = performance.now();
    if (!event.repeat && performance.now() - compositionEnded > 50) submit(true);
  } else if (event.ctrlKey && /^[a-z]$/i.test(event.key)) {
    const letter = event.key.toLowerCase();
    const selection = field.selectionStart !== field.selectionEnd;
    if (letter === 'v' || ('cx'.includes(letter) && selection) || ('azy'.includes(letter) && field.value)) return;
    event.preventDefault();
    shortcut(String.fromCharCode(letter.toUpperCase().charCodeAt(0) & 31));
  } else if (!event.shiftKey && !event.ctrlKey && event.key in sequences) {
    if (field.value && ['ArrowLeft', 'ArrowRight', 'Backspace'].includes(event.key)) return;
    event.preventDefault();
    shortcut(sequences[event.key]);
  }
});
field.addEventListener('beforeinput', event => {
  if (composing || event.isComposing || !['insertParagraph', 'insertLineBreak'].includes(event.inputType)) return;
  event.preventDefault();
  if (performance.now() - Math.max(compositionEnded, enterPressed) > 50) submit(true);
});

if (hasCredential || inManager) {
  status('正在读取设备配置…');
  refreshTabs().catch(error => status(error.message));
} else {
  setView('notice');
  status('请从模块管理器打开 ssmcp 终端。使用 Magisk 时，请点击模块的「操作」。');
}
