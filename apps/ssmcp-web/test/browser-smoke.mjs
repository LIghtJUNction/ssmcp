// Optional development check. Uses an already-installed Playwright + Chromium.
// All API data here is an explicitly isolated test fixture, never shipped by the app.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import { setTimeout as delay } from 'node:timers/promises';
import { capabilities, response, eventPage, observation, GENERATION } from './fixtures.mjs';

const require = createRequire(import.meta.url);
const { chromium } = require(process.env.PLAYWRIGHT_MODULE || 'playwright');
const port = Number(process.env.WEB_TEST_PORT || 14173);
const server = spawn(process.execPath, [fileURLToPath(new URL('../dev-server.mjs', import.meta.url)), '--port', String(port)], { stdio: ['ignore', 'pipe', 'pipe'] });
let browser;
try {
  await new Promise((resolve, reject) => { server.stdout.once('data', resolve); server.once('error', reject); server.once('exit', code => reject(new Error(`Static server exited: ${code}`))); });
  browser = await chromium.launch({ executablePath: process.env.CHROMIUM_PATH || undefined, headless: true, args: ['--no-sandbox'] });
  const page = await browser.newPage({ viewport: { width: 1365, height: 900 } });
  const errors = [], commands = [], requests = [], pending = [];
  let cursor = '9007199254740993', queue = [], failTerminal = false;
  page.on('pageerror', error => errors.push(error.message));
  await page.addInitScript(() => {
    window.cspViolations = [];
    document.addEventListener('securitypolicyviolation', event => window.cspViolations.push(event.violatedDirective));
  });
  const caps = capabilities({ resources: ['runtime', 'threads', 'conversation', 'chat.queue', 'terminals', 'workers'], commands: ['chat.send', 'terminal.write', 'terminal.resize', 'workers.resize'] });
  const source = { kind: 'main', client_id: 'real.fixture.client', connection: '9007199254740993', request_id: '12' };
  const worker = { slot: 0, epoch: '4', state: 'working', task_id: 'fixture.actual.task', task_summary: '核对原生工作状态', progress: '正在读取 fixture', registered: true, total_work_ms: '42000', current_task_elapsed_ms: '3000', logs_omitted: true };
  const states = resource => ({
    runtime: { backend_active: true, backend_generation: '2', live_cursor: cursor, retention: 'live_process_memory' },
    threads: { client_id: 'fixture.client', connected: true, chat_enabled: true, can_send: true, reloading: false, agents: [{ agent_id: 'actual.agent', name: '主协调者', role: 'main', status: 'working' }], next_offset: 1, has_more: false },
    conversation: { schema_version: 1, scope: { kind: 'process' }, events: [{ sequence: '9007199254740992', kind: 'answer', text: '来自原生历史的 fixture 文本', source, value: { type: 'ssmcp.message' } }], next_cursor: '9007199254740993', start_cursor: '9007199254740992', end_cursor: '9007199254740993', has_more: false, gap: false, retention: 'live_process_memory' },
    'chat.queue': { client_id: 'fixture.client', items: queue, next_offset: queue.length, has_more: false },
    terminals: { terminals: [{ terminal_id: 'actual.terminal', name: '主终端', cwd: '/fixture/project', selected: true }], selected: 'actual.terminal', default_terminal_id: 'actual.terminal', max_terminals: 4 },
    workers: { pool_generation: 'actual.pool', total: 1, desired_total: 1, registered_workers: 1, working: 1, idle: 0, offline: 0, retiring: 0, queued: 0, slots: [worker], slot_count: 1, offset: 0, next_offset: null },
  })[resource];
  const fulfill = (route, body) => route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(body) });
  await page.route('**/api/gui/v1/**', async route => {
    const req = route.request(); const url = new URL(req.url()); const body = req.postDataJSON();
    requests.push({ path: url.pathname, authorization: req.headers().authorization, body });
    if (url.pathname.endsWith('/sessions')) return fulfill(route, { sessions: [{ name: '真实会话 fixture', cwd: '/fixture/project', manager_active: true, manager_state: 'active', connection_state: 'connected', phase: 'active', generation: '2', stop_requested: false }], next_offset: 1, has_more: false });
    if (url.pathname === '/api/gui/v1/connections') return fulfill(route, { status: 'connected', connection_id: 'fixture.connection', session: body.session, read_only: body.read_only, capabilities: response({ id: body.request_id, operation: { op: 'capabilities' } }, caps), duplicate: false });
    if (url.pathname.endsWith('/bridge')) {
      const op = body.operation;
      if (op.op === 'capabilities') return fulfill(route, response(body, caps));
      if (op.op === 'state') return fulfill(route, response(body, { resource: op.resource, schema_version: 1, value: states(op.resource) }));
      if (op.op === 'command') {
        commands.push(body);
        if (op.command === 'terminal.write' && failTerminal) return route.abort('failed');
        let value = { received: true };
        if (op.command === 'chat.send') {
          value = { request_id: body.id, message_id: 'fixture.message', status: 'sending', target: op.arguments.target, duplicate: false, confirmation: 'local_queue_admission' };
          queue = [{ id: value.message_id, text: op.arguments.text, target: value.target, status: value.status, created_at_ms: '9007199254740993', updated_at_ms: '9007199254740993', uncertain: false, retarget_required: false, error: null }];
        }
        if (op.command === 'workers.resize') value = { resize_pending: true, desired_total: op.arguments.total, pool_generation: 'actual.pool' };
        return fulfill(route, response(body, { command: op.command, value }));
      }
    }
    if (url.pathname.endsWith('/events')) return new Promise(resolve => pending.push({ route, request: body, resolve }));
    if (req.method() === 'DELETE') return fulfill(route, { closed: true, session_stopped: false });
    throw new Error(`Unhandled fixture endpoint: ${url.pathname}`);
  });

  async function emit(text, count = 1) {
    for (let attempt = 0; !pending.length && attempt < 200; attempt += 1) await delay(10);
    assert.ok(pending.length, 'observer must have an outstanding read');
    const item = pending.shift();
    const events = [];
    for (let index = 0; index < count; index += 1) {
      const sequence = cursor; cursor = (BigInt(cursor) + 1n).toString();
      events.push({ sequence, topic: 'ssmcp.event', value: { kind: 'event', source, value: { type: 'ssmcp.message', kind: 'answer', text: count === 1 ? text : `${text} ${index}` } } });
    }
    await fulfill(item.route, observation(item.request, eventPage({ start: '9007199254740992', next: cursor, events })));
    item.resolve();
  }
  const initial = await page.goto(`http://127.0.0.1:${port}`);
  assert.match(initial.headers()['content-security-policy'], /script-src 'self'/);
  assert.equal(initial.headers()['content-security-policy'].includes('unsafe-inline'), false);
  await page.locator('#welcome-connect').click();
  await page.locator('#pairing-token').fill('fixture-pairing-only');
  await page.locator('#authenticate').click();
  await page.locator('.session-item').click();
  await page.locator('#connection-mode').selectOption('control');
  await page.locator('#connect-button').click();
  await page.getByText('来自原生历史的 fixture 文本', { exact: true }).waitFor();
  await page.getByText('Worker 任务快照 · 站点 0', { exact: true }).waitFor();
  assert.equal(commands.length, 0, 'connection/state reads must not dispatch commands');
  assert.equal(await page.locator('#pairing-token').inputValue(), '');
  assert.deepEqual(await page.evaluate(() => [localStorage.length, sessionStorage.length]), [0, 0]);

  const draft = '中文输入法草稿，仍在组合';
  await page.locator('#chat-text').fill(draft); await page.locator('#chat-text').focus();
  await page.locator('#chat-text').evaluate(node => { node.setSelectionRange(4, 4); node.dispatchEvent(new CompositionEvent('compositionstart', { bubbles: true, data: '中' })); });
  await emit('组合期间真实事件刷新'); await page.getByText('组合期间真实事件刷新', { exact: true }).waitFor();
  await page.keyboard.press('Control+Enter');
  assert.equal(commands.length, 0, 'IME composition must block a send shortcut');
  assert.equal(await page.locator('#chat-text').inputValue(), draft);
  assert.deepEqual(await page.locator('#chat-text').evaluate(node => [document.activeElement === node, node.selectionStart]), [true, 4]);
  await page.locator('#chat-text').evaluate(node => node.dispatchEvent(new CompositionEvent('compositionend', { bubbles: true, data: '中' })));
  await page.keyboard.press('Control+Enter');
  await page.waitForFunction(() => document.getElementById('chat-text').value === '');
  assert.equal(commands.length, 1); assert.equal(commands[0].operation.command, 'chat.send');
  assert.equal(commands[0].operation.arguments.text, draft); assert.equal(commands[0].operation.arguments.request_id, commands[0].id);

  for (let pageIndex = 0; pageIndex < 3; pageIndex += 1) {
    await emit(`滚动检查 ${pageIndex}`, 16);
    await page.getByText(`滚动检查 ${pageIndex} 15`, { exact: true }).waitFor();
  }
  assert.ok(await page.locator('#timeline > article').count() <= 40);
  await page.locator('#timeline-scroller').evaluate(node => { node.scrollTop = 0; });
  const visibleIDs = await page.locator('#timeline > article').evaluateAll(nodes => nodes.map(node => node.dataset.row));
  await emit('用户向上阅读时的新页', 16);
  await page.locator('#new-events').waitFor({ state: 'visible' });
  assert.deepEqual(await page.locator('#timeline > article').evaluateAll(nodes => nodes.map(node => node.dataset.row)), visibleIDs);
  assert.equal(await page.locator('#timeline-scroller').evaluate(node => node.scrollTop), 0);
  await page.locator('#new-events').click();
  await page.getByText('用户向上阅读时的新页 15', { exact: true }).waitFor();

  await page.locator('#tab-terminal').click(); await page.locator('#terminal-input').fill('echo retained');
  failTerminal = true; await page.locator('#terminal-send').click();
  await page.getByText('结果未确定。请在操作记录中核对原请求 ID；页面不会自动重发。', { exact: true }).waitFor();
  assert.equal(await page.locator('#terminal-input').inputValue(), 'echo retained');
  assert.equal(commands.filter(item => item.operation.command === 'terminal.write').length, 1);
  await page.locator('#refresh-state').click();
  assert.equal(commands.filter(item => item.operation.command === 'terminal.write').length, 1);

  for (const width of [390, 320]) {
    await page.setViewportSize({ width, height: 844 });
    for (const tab of ['conversation', 'terminal', 'workers']) {
      await page.locator(`#tab-${tab}`).click();
      assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), `${tab} must fit ${width}px viewport`);
      const panel = await page.locator(`#panel-${tab}`).boundingBox();
      assert.ok(panel && panel.x >= 0 && panel.x + panel.width <= width + 1);
    }
  }
  await page.locator('#tab-conversation').click();
  await page.locator('#menu-button').click();
  assert.equal(await page.locator('#menu-button').getAttribute('aria-expanded'), 'true');
  await page.keyboard.press('Escape');
  assert.equal(await page.locator('#menu-button').getAttribute('aria-expanded'), 'false');
  assert.deepEqual(errors, []);
  assert.deepEqual(await page.evaluate(() => window.cspViolations), []);
  assert.ok(requests.every(request => request.authorization === 'Bearer fixture-pairing-only'));
  assert.equal(requests.some(request => JSON.stringify(request.body).includes('fixture-pairing-only')), false);
  assert.equal(commands.some(item => item.operation.command === 'terminal.resize'), false, 'viewport changes are not terminal commands');
  process.stdout.write('Browser fixture smoke passed: CSP, protocol envelopes, IME/focus, bounded DOM, scroll continuity, queue receipt, unknown command, mobile panels.\n');
} finally {
  if (browser) await browser.close();
  server.kill('SIGTERM');
}
