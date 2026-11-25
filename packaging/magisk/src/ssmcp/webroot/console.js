import { el, hasCredential, terminal as localTerminal, TerminalWriter } from './device-api.js';
import { hasManagerTransport, managerTerminal, openManagerSetup, openManagerWorkbench } from './module-entry.js';

import { drawFrame } from './terminal-frame.js';

const screen = el('screen');
const direct = el('direct-input');
const cursor = el('terminal-cursor');
const ctrl = el('keys').querySelector('[data-key=Ctrl]');
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
function status(message, source = 'action', detail = '') {
  statusSource = message ? source : '';
  const output = el('status');
  if (output.textContent !== message) output.textContent = message;
  output.title = detail;
}

let selected = null;
let generation = 0;
let tabs = [];
let changingTabs = false;
let listRequest = 0;
const writes = new Map();
const drafts = new Map();
const isCurrent = version => version === generation && document.body.dataset.view === 'console';
function setCtrl(armed) {
  ctrl.setAttribute('aria-pressed', String(armed));
}
function selectTab(id) {
  if (selected === id) return;
  direct.blur();
  frameRevision = undefined;
  screenCursor = null;
  cursor.hidden = true;
  selected = id;
  renderDraft();
  generation++;
  setCtrl(false);
  status('');
  screen.textContent = '';
  screen.scrollTop = screen.scrollLeft = 0;
  idleReads = 0;
  readFailures = 0;
  sizeFailures = 0;
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
      listRequest++;
      selectTab(id);
      renderTabs();
    });
    button.dataset.tab = String(id);
    button.setAttribute('aria-pressed', String(id === selected));
    return button;
  });
  const open = tabButton('+', () => changeTabs('open'));
  open.dataset.action = 'open';
  open.setAttribute('aria-label', '新建会话');
  open.title = '新建会话';
  open.disabled = open.disabled || tabs.length >= 8;
  buttons.push(open);
  if (selected !== 0) {
    const close = tabButton('×', () => changeTabs('close'));
    close.dataset.action = 'close';
    close.setAttribute('aria-label', '关闭当前会话');
    close.title = '关闭当前会话';
    buttons.push(close);
  }
  const workbench = tabButton('Zuse', async () => {
    if (!inManager) { location.assign('/zuse'); return; }
    try { await openManagerWorkbench(); }
    catch (error) { status('无法打开 Zuse 工作台，请检查手机上的 zuse web 服务。', 'workbench', error.message); }
  });
  workbench.setAttribute('aria-label', '打开 Zuse 工作台');
  buttons.push(workbench);
  const nav = el('tabs');
  const hadFocus = nav.contains(document.activeElement);
  nav.textContent = '';
  nav.append(...buttons);
  const current = nav.querySelector('[aria-pressed="true"]');
  current?.scrollIntoView({ block: 'nearest', inline: 'nearest' });
  if (hadFocus && current && !current.disabled) current.focus({ preventScroll: true });
}
async function refreshTabs(preferred = selected) {
  const request = ++listRequest;
  const result = await terminal('list', selected ?? 0);
  if (request !== listRequest) return;
  if (!result.target) {
    selected = null;
    generation++;
    tabs = [];
    setView('notice');
    setupControl.hidden = false;
    status('此设备尚未完成首次配置。');
    return;
  }
  const changed = tabs.length !== result.tabs.length || tabs.some((id, index) => result.tabs[index] !== id);
  tabs = result.tabs;
  setView('console');
  selectTab(tabs.includes(preferred) ? preferred : tabs[0]);
  if (changed) { renderTabs(); renderDraft(); }
}
async function changeTabs(action) {
  if (changingTabs || (action === 'close' && selected === 0)) return;
  changingTabs = true;
  listRequest++;
  renderTabs();
  try {
    if (action === 'close') {
      await writes.get(selected)?.done;
      if (drafts.has(selected)) return;
    }
    const result = await terminal(action, selected);
    await refreshTabs(action === 'open' ? result.tab : selected);
    if (statusSource === 'tabs') status('');
  } catch (error) {
    status('会话操作未确认，请检查当前标签。', 'tabs', error.message);
  } finally {
    changingTabs = false;
    renderTabs();
  }
}

let grid;
let terminalSize;
let screenCursor;
let frameRevision;
const scaleKey = 'ssmcp-terminal-font-scale';
let fontScale = 1;
try {
  const saved = Number(localStorage.getItem(scaleKey));
  if (saved >= 0.25 && saved <= 3) fontScale = saved;
} catch { /* Zoom still works when the WebView disables local storage. */ }

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
  const style = getComputedStyle(screen);
  const left = parseFloat(style.paddingLeft);
  const top = parseFloat(style.paddingTop);
  const width = screen.clientWidth - left - parseFloat(style.paddingRight);
  const height = screen.clientHeight - top - parseFloat(style.paddingBottom);
  let cell = probe.getBoundingClientRect();
  if (Math.min(width, height, cell.width, cell.height) <= 0) { probe.remove(); return null; }
  const base = parseFloat(style.getPropertyValue('--terminal-base-size'));
  const current = parseFloat(style.fontSize);
  // The PTY has a 32-column minimum. Use the actual glyph advance, including WebView font rounding.
  const max = Math.min(24, current * width / (32 * cell.width / 64));
  const requested = Math.round(4 * Math.max(9, base * fontScale)) / 4;
  let px = Math.max(0.25, Math.min(requested, Math.floor(4 * max) / 4));
  if (px !== current) {
    screen.style.fontSize = px + 'px';
    cell = probe.getBoundingClientRect();
  }
  while (32 * cell.width / 64 > width && px > 0.25) {
    px -= 0.25;
    screen.style.fontSize = px + 'px';
    cell = probe.getBoundingClientRect();
  }
  probe.remove();
  grid = { left, top, width: cell.width / 64, height: cell.height / 2 };
  screen.parentElement.style.setProperty('--cell-width', grid.width + 'px');
  screen.parentElement.style.setProperty('--cell-height', grid.height + 'px');
  positionCursor();
  terminalSize = {
    cols: Math.max(32, Math.min(200, Math.floor(width / grid.width))),
    rows: Math.max(10, Math.min(80, Math.floor(height / grid.height))),
  };
  return terminalSize;
}
// This is a live TUI screen, not scrollback: send SGR mouse coordinates to
// the PTY so the native UI can hit-test cards and scroll the touched panel.
function mousePoint(clientX, clientY) {
  if (!grid || !terminalSize) return null;
  const rect = screen.getBoundingClientRect();
  const x = Math.floor((clientX - rect.left - screen.clientLeft - grid.left + screen.scrollLeft) / grid.width);
  const y = Math.floor((clientY - rect.top - screen.clientTop - grid.top + screen.scrollTop) / grid.height);
  if (x < 0 || y < 0 || x >= terminalSize.cols || y >= terminalSize.rows) return null;
  return { x: x + 1, y: y + 1 };
}
function mouseReport(button, point, release = false) {
  return `\x1b[<${button};${point.x};${point.y}${release ? 'm' : 'M'}`;
}
function tap(point) {
  if (!point) return;
  shortcut(mouseReport(0, point) + mouseReport(0, point, true));
  if (screenCursor?.visible && point.y === screenCursor.row + 1) direct.focus({ preventScroll: true });
}
let swipe = null;
let pinch = null;
let zoomFrame = 0;
let ignoreClickUntil = 0;
function fontRatio() {
  const style = getComputedStyle(screen);
  return parseFloat(style.fontSize) / parseFloat(style.getPropertyValue('--terminal-base-size'));
}
function fingerDistance(event) {
  if (event.touches.length !== 2) return 0;
  const [a, b] = event.touches;
  if (!screen.contains(a.target) || !screen.contains(b.target)) return 0;
  return Math.hypot(a.clientX - b.clientX, a.clientY - b.clientY);
}
function finishPinch() {
  if (!pinch) return;
  ignoreClickUntil = performance.now() + 350;
  if (pinch.moved) {
    cancelAnimationFrame(zoomFrame);
    zoomFrame = 0;
    dimensions();
    fontScale = fontRatio();
    try { localStorage.setItem(scaleKey, String(fontScale)); } catch { /* Keep the in-memory scale. */ }
    scheduleResize();
  }
  pinch = null;
}
screen.addEventListener('touchstart', event => {
  if (event.touches.length === 1) {
    const finger = event.touches[0];
    swipe = { x: finger.clientX, y: finger.clientY, lastY: finger.clientY, moved: false, version: generation };
    return;
  }
  swipe = null;
  ignoreClickUntil = performance.now() + 350;
  const distance = fingerDistance(event);
  if (!distance) { finishPinch(); return; }
  if (event.cancelable) event.preventDefault();
  pinch = { distance, scale: fontRatio(), moved: false };
}, { passive: false });
screen.addEventListener('touchmove', event => {
  if (!pinch) {
    if (!swipe || swipe.version !== generation || event.touches.length !== 1) return;
    const finger = event.touches[0];
    if (!swipe.moved && Math.hypot(finger.clientX - swipe.x, finger.clientY - swipe.y) < 8) return;
    swipe.moved = true;
    if (event.cancelable) event.preventDefault();
    const step = Math.max(12, grid?.height ?? 18);
    const delta = finger.clientY - swipe.lastY;
    const count = Math.min(8, Math.floor(Math.abs(delta) / step));
    const point = mousePoint(swipe.x, swipe.y);
    if (point && count) shortcut(mouseReport(delta > 0 ? 64 : 65, point).repeat(count));
    swipe.lastY += Math.sign(delta) * count * step;
    ignoreClickUntil = performance.now() + 350;
    return;
  }
  const distance = fingerDistance(event);
  if (!distance) { finishPinch(); return; }
  if (event.cancelable) event.preventDefault();
  if (!pinch.moved && Math.abs(distance - pinch.distance) < 4) return;
  pinch.moved = true;
  fontScale = pinch.scale * distance / pinch.distance;
  if (!zoomFrame) zoomFrame = requestAnimationFrame(() => {
    zoomFrame = 0;
    dimensions();
    if (!sizeTimer) sizeTimer = setTimeout(resize, 120);
  });
}, { passive: false });
screen.addEventListener('touchend', event => {
  if (swipe && swipe.version === generation && !swipe.moved && !pinch && !event.touches.length
      && window.getSelection()?.isCollapsed !== false) {
    const finger = event.changedTouches[0];
    if (finger && Math.hypot(finger.clientX - swipe.x, finger.clientY - swipe.y) < 8) {
      if (event.cancelable) event.preventDefault();
      tap(mousePoint(finger.clientX, finger.clientY));
    }
  }
  ignoreClickUntil = performance.now() + 350;
  swipe = null;
  finishPinch();
}, { passive: false });
screen.addEventListener('touchcancel', () => {
  swipe = null;
  ignoreClickUntil = performance.now() + 350;
  finishPinch();
});
screen.addEventListener('wheel', event => {
  const point = mousePoint(event.clientX, event.clientY);
  if (!point || !event.deltaY) return;
  event.preventDefault();
  const count = Math.min(8, Math.max(1, Math.ceil(Math.abs(event.deltaY) / (event.deltaMode === 0 ? 40 : 1))));
  shortcut(mouseReport(event.deltaY < 0 ? 64 : 65, point).repeat(count));
}, { passive: false });

let sizeTimer;
let resizing = false;
let resizeAgain = false;
let lastSize = '';
let sizeFailures = 0;
function scheduleResize() {
  clearTimeout(sizeTimer);
  sizeTimer = setTimeout(resize, 40);
}
async function resize() {
  sizeTimer = null;
  const version = generation;
  if (!isCurrent(version) || selected === null || document.hidden) return;
  if (resizing) { resizeAgain = true; return; }
  const size = dimensions();
  if (!size) return;
  const key = `${version}/${size.cols}/${size.rows}`;
  if (lastSize === key) return;
  resizing = true;
  try {
    await terminal('resize', selected, size);
    lastSize = key;
    if (isCurrent(version)) {
      sizeFailures = 0;
      if (statusSource === 'resize') status('');
    }
  } catch (error) {
    if (isCurrent(version) && !document.hidden) {
      sizeFailures++;
      if (sizeFailures >= 3 && !statusSource) status('暂时无法调整终端尺寸，正在重试。', 'resize', error.message);
      clearTimeout(sizeTimer);
      sizeTimer = setTimeout(resize, 2000);
    }
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
let readFailures = 0;
let readAgain = false;
let lastInput = -Infinity;
function inputActivity() {
  idleReads = 0;
  lastInput = performance.now();
  if (document.hidden || !isCurrent(generation)) return;
  if (polling === generation) { readAgain = true; return; }
  clearTimeout(pollTimer);
  pollTimer = setTimeout(poll, 0);
}
async function poll() {
  clearTimeout(pollTimer);
  const version = generation;
  if (!isCurrent(version) || document.hidden || polling === version) return;
  polling = version;
  try {
    const snapshot = await terminal('screen', selected, { styled: true, since: frameRevision });
    if (!isCurrent(version)) return;
    const next = snapshot.exited ? null : snapshot.cursor;
    const moved = next?.row !== screenCursor?.row || next?.col !== screenCursor?.col || next?.visible !== screenCursor?.visible;
    const changed = !snapshot.unchanged && drawFrame(screen, snapshot);
    frameRevision = snapshot.revision;
    idleReads = changed || moved ? 0 : idleReads + 1;
    screenCursor = next;
    positionCursor();
    readFailures = 0;
    if (statusSource === 'poll') status('');
  } catch (error) {
    if (isCurrent(version)) {
      idleReads++;
      readFailures++;
      if (readFailures >= 3 && (!statusSource || statusSource === 'poll' || statusSource === 'resize')) {
        status('暂时无法读取终端，正在重试。', 'poll', error.message);
      }
      if (readFailures === 3) {
        // Another manager window may have closed this tab. Reconcile the session list once.
        refreshTabs().catch(() => {});
      }
    }
  } finally {
    if (polling === version) polling = -1;
    if (isCurrent(version) && !document.hidden) {
      const interval = inManager ? Math.min(1800, 450 + Math.max(0, idleReads - 2) * 250) : 450;
      pollTimer = setTimeout(poll, readAgain ? 0 : performance.now() - lastInput < 200 ? 32 : interval);
      readAgain = false;
    }
  }
}
let checkingEntry = false;
let entryTimer;
let entryFailures = 0;
async function recheckEntry() {
  if (checkingEntry || document.hidden || document.body.dataset.view === 'console' || (!hasCredential && !inManager)) return;
  clearTimeout(entryTimer);
  checkingEntry = true;
  try {
    await refreshTabs();
    entryFailures = 0;
    if (statusSource === 'entry') status('');
  } catch (error) {
    if (error.status === 401 || error.status === 403) {
      status('请返回模块管理器，重新打开 SSMCP 的 WebUI。', 'entry', error.message);
      return;
    }
    entryFailures++;
    if (entryFailures >= 3) status('暂时无法打开终端，正在重试。', 'entry', error.message);
    if (!document.hidden) entryTimer = setTimeout(recheckEntry, Math.min(2400, 600 * 2 ** (entryFailures - 1)));
  } finally {
    checkingEntry = false;
  }
}
window.addEventListener('focus', recheckEntry);
document.addEventListener('visibilitychange', () => {
  if (document.hidden) { clearTimeout(pollTimer); clearTimeout(entryTimer); }
  else if (document.body.dataset.view !== 'console') recheckEntry();
  else { idleReads = 0; viewportChanged(); poll(); }
});

const sequences = {
  Enter: '\r', Escape: '\x1b', Tab: '\t', Backspace: '\x7f',
  ArrowUp: '\x1b[A', ArrowDown: '\x1b[B', ArrowRight: '\x1b[C', ArrowLeft: '\x1b[D',
};
const directSequences = {
  ...sequences, Home: '\x1b[H', End: '\x1b[F', Delete: '\x1b[3~',
  Insert: '\x1b[2~', PageUp: '\x1b[5~', PageDown: '\x1b[6~',
};
const draftPanel = el('input-recovery');
const uncertainField = el('uncertain-input');
const unsentField = el('unsent-input');
let shownDraft = null;
let finishingComposition = null;

function renderDraft() {
  // Another manager can close a tab. Keep its draft reachable from the current tab.
  shownDraft = drafts.has(selected) ? selected : [...drafts.keys()].find(id => !tabs.includes(id));
  const draft = drafts.get(shownDraft);
  draftPanel.hidden = !draft;
  direct.readOnly = drafts.has(selected) && !preedit;
  uncertainField.value = draft?.uncertain || '';
  uncertainField.parentElement.hidden = !uncertainField.value;
  unsentField.value = draft?.unsent || '';
  unsentField.parentElement.hidden = !unsentField.value;
  if (draft) el('draft-tab').textContent = shownDraft === 0 ? '启动会话' : '会话 ' + (shownDraft + 1);
}
function saveDraft(tab, uncertain, unsent) {
  // A later transport failure precedes any input already parked by backpressure.
  if (tab === selected && preedit) finishingComposition = tab;
  const previous = drafts.get(tab);
  drafts.set(tab, { uncertain, unsent: unsent + (previous?.unsent || '') });
  renderDraft();
}
el('dismiss-draft').onclick = () => {
  if (finishingComposition === shownDraft) finishingComposition = null;
  drafts.delete(shownDraft);
  renderDraft();
};
function shortcut(text) {
  if (!isCurrent(generation) || changingTabs || drafts.has(selected) || !text) return;
  const tab = selected;
  let writer = writes.get(tab);
  if (!writer) {
    writer = new TerminalWriter(async part => {
      await terminal('input', tab, { text: part });
      if (tab === selected) inputActivity();
    }, (uncertain, unsent) => saveDraft(tab, uncertain, unsent), () => writes.delete(tab));
    writes.set(tab, writer);
  }
  if (!writer.append(text)) saveDraft(tab, '', text);
}

// A sentinel makes a backspace observable on mobile keyboards even with no draft.
const sentinel = '\u200b';
let directVersion = -1;
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
  const normalized = text.replace(/\r\n|\n/g, '\r');
  if (drafts.has(selected)) {
    // An IME may finish a composition after a write has failed.
    if (finishingComposition !== selected) return;
    finishingComposition = null;
    drafts.get(selected).unsent += normalized;
    renderDraft();
  } else {
    shortcut(normalized);
  }
}
function commitDirect() {
  if (directVersion !== generation || preedit) return;
  const text = direct.value.startsWith(sentinel) ? direct.value.slice(1) : direct.value;
  resetDirect();
  directText(text);
}
direct.addEventListener('focus', () => {
  directVersion = generation;
  resetDirect();
});
direct.addEventListener('blur', () => {
  directVersion = -1;
  preedit = false;
  clearTimeout(commitTimer);
  resetDirect();
});
screen.addEventListener('click', event => {
  if (performance.now() < ignoreClickUntil) return;
  if (window.getSelection()?.isCollapsed !== false) {
    // Keyboard/assistive clicks have no meaningful cell coordinates.
    if (event.detail) tap(mousePoint(event.clientX, event.clientY));
    else direct.focus({ preventScroll: true });
  }
});
direct.addEventListener('compositionstart', () => { preedit = true; clearTimeout(commitTimer); });
direct.addEventListener('compositionend', () => {
  preedit = false;
  preeditEnded = performance.now();
  // Some IMEs dispatch the final input before this event and others dispatch it after.
  commitTimer = setTimeout(() => {
    commitDirect();
    if (drafts.has(selected)) { finishingComposition = null; renderDraft(); }
  }, 0);
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
el('open-keyboard').onclick = () => direct.focus({ preventScroll: true });
ctrl.onclick = () => {
  setCtrl(ctrl.getAttribute('aria-pressed') !== 'true');
  direct.focus({ preventScroll: true });
};
el('keys').addEventListener('mousedown', event => {
  if (document.activeElement === direct) event.preventDefault();
});
for (const key of el('keys').querySelectorAll('button[data-key]:not([data-key=Ctrl])')) {
  key.onclick = () => shortcut(sequences[key.dataset.key]);
}

if (hasCredential || inManager) {
  recheckEntry();
} else {
  setView('notice');
  status('请在支持 WebUI 的模块管理器中找到 SSMCP，打开 WebUI 进入设备终端。');
}
