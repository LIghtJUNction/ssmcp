// Optional development check. Uses an already-installed Playwright + Chromium.
// All API data here is an explicitly isolated test fixture, never shipped by the app.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createRequire } from 'node:module';
import { mkdir, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
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
  const foreground = 'live:' + '71'.repeat(16);
  let newCount = 0, activeTarget = foreground, cwd = '/fixture/project';
  const created = [], owned = new Map();
  let dropFirstReceipt = true;
  const endedTargets = [];
  let reportFirstEnd;
  const firstEnd = new Promise(resolve => { reportFirstEnd = resolve; });
  const errors = [], commands = [], requests = [], pending = [];
  let cursor = '9007199254740993', queue = [], failTerminal = false;
  page.on('pageerror', error => errors.push(error.message));
  await page.addInitScript(() => {
    window.cspViolations = [];
    document.addEventListener('securitypolicyviolation', event => window.cspViolations.push(event.violatedDirective));
  });
  const caps = capabilities({ resources: ['runtime', 'threads', 'conversation', 'chat.queue', 'terminals', 'workers'], commands: ['session.new', 'session.cd', 'chat.send', 'terminal.write', 'terminal.resize', 'workers.resize', 'client.update'] });
  let activeCaps = caps;
  const source = { kind: 'main', client_id: 'real.fixture.client', connection: '9007199254740993', request_id: '12' };
  const productivity = {
    members: 1,
    calls_per_minute: ['0', '0', '0', '1', '0', '0', '0', '0', '0', '0'],
    plot: ['       1 |      *', '         |     | |', '         |     | |',
      '       0 |*-*-*| |*-*-*-*-*-*', '         + -10m -------- now'],
  };
  const worker = { slot: 0, epoch: '4', state: 'working', task_id: 'fixture.actual.task', task_summary: '核对原生工作状态', progress: '正在读取 fixture', registered: true, total_work_ms: '42000', current_task_elapsed_ms: '3000', logs_omitted: true };
  const inputCases = [
    { origin: 'human', text: '用户要求：检查多行输入\n@fixture-missing.txt $fixture_skill /reload' },
    { origin: 'system_hook', text: '系统提示：继续当前任务' },
    { origin: 'agent_timer', text: '定时提示：原任务仍在执行' },
    { origin: 'worker_event', text: 'Worker 事件：检查已经完成' },
    { origin: 'maintenance', text: '维护提示：处理有界待办' },
  ].map((value, index) => ({ ...value, id: 'native_' + String(index + 1).padStart(32, '0') }));
  const inputHistory = inputCases.map((item, index) => ({
    sequence: (9007199254740987n + BigInt(index)).toString(), kind: 'input', text: item.text,
    source: { ...source, kind: item.origin === 'human' ? 'local_human' : 'thread',
      agent_id: item.origin === 'human' ? null : 'actual.agent' },
    value: { type: item.origin === 'human' ? 'ssmcp.user_input' : 'ssmcp.message',
      kind: 'input', role: item.origin === 'human' ? 'user' : 'tool', text: item.text,
      message_id: item.id, input_origin: item.origin, completion_permission: false },
  }));
  const states = resource => ({
    runtime: { cwd, backend_active: true, backend_generation: '2', live_cursor: cursor, retention: 'live_process_memory' },
    threads: { client_id: 'fixture.client', connected: true, chat_enabled: true, can_send: true, reloading: false, agents: [{ agent_id: 'actual.agent', name: '主协调者', role: 'main', status: 'working' }], next_offset: 1, has_more: false },
    conversation: { schema_version: 1, scope: { kind: 'process' }, events: activeTarget !== foreground ? [] : [...inputHistory, { sequence: '9007199254740992', kind: 'answer', text: '来自原生历史的 fixture 文本', source, value: { type: 'ssmcp.message', kind: 'answer', message_id: 'native_00000000000000000000000000000006' } }], next_cursor: '9007199254740993', start_cursor: '9007199254740987', end_cursor: '9007199254740993', has_more: false, gap: false, retention: 'live_process_memory' },
    'chat.queue': { client_id: 'fixture.client', items: queue, next_offset: queue.length, has_more: false },
    terminals: { terminals: [{ terminal_id: 'actual.terminal', name: '主终端', cwd: '/fixture/project', selected: true }], selected: 'actual.terminal', default_terminal_id: 'actual.terminal', max_terminals: 4 },
    workers: { pool_generation: 'actual.pool', total: 2, desired_total: 2, registered_workers: 2, working: 1, idle: 1, dreaming: 1, offline: 0, retiring: 0, queued: 0, productivity, slots: [worker,
      { slot: 1, epoch: '4', state: 'idle', display_state: 'dreaming', registered: true, total_work_ms: '0' }], slot_count: 2, offset: 0, next_offset: null },
  })[resource];
  const fulfill = (route, body) => route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(body) });
  await page.route('**/api/gui/v1/**', async route => {
    const req = route.request(); const url = new URL(req.url()); const body = req.postDataJSON();
    requests.push({ path: url.pathname, authorization: req.headers().authorization, body });
    if (url.pathname.startsWith('/api/gui/v1/owned-sessions/')) {
      assert.equal(req.method(), 'DELETE');
      const target = decodeURIComponent(url.pathname.split('/').at(-1));
      endedTargets.push(target);
      if (endedTargets.length === 1) return new Promise(done => {
        reportFirstEnd(async () => { await route.abort('failed'); done(); });
      });
      return fulfill(route, {session:target, status:'closed'});
    }
    if (url.pathname.endsWith('/owned-sessions')) {
      if (!owned.has(body.request_id)) {
        newCount++;
        owned.set(body.request_id, 'live:' + newCount.toString(16).padStart(32, '0'));
      }
      if (dropFirstReceipt) { dropFirstReceipt = false; return route.abort('failed'); }
      return fulfill(route, { request_id: body.request_id, session: owned.get(body.request_id) });
    }
    if (url.pathname.endsWith('/sessions')) return fulfill(route, { sessions: [{ name: '真实会话 fixture', cwd: '/fixture/project' }, { name: '真实会话 fixture', cwd: '/fixture/project', target: foreground, manager_active: true, manager_state: 'active', connection_state: 'connected', phase: 'active', generation: '2', stop_requested: false }, ...created], next_offset: 2 + created.length, has_more: false });
    if (url.pathname === '/api/gui/v1/connections') { activeTarget = body.session; activeCaps = body.read_only ? { ...caps, mode: 'observe', commands: [] } : caps; return fulfill(route, { status: 'connected', connection_id: 'fixture.connection', session: body.session, read_only: body.read_only, capabilities: response({ id: body.request_id, operation: { op: 'capabilities' } }, activeCaps), duplicate: false }); }
    if (url.pathname.endsWith('/bridge')) {
      const op = body.operation;
      if (op.op === 'capabilities') return fulfill(route, response(body, activeCaps));
      if (op.op === 'state') return fulfill(route, response(body, { resource: op.resource, schema_version: 1, value: states(op.resource) }));
      if (op.op === 'command') {
        commands.push(body);
        if (op.command === 'terminal.write' && failTerminal) return route.abort('failed');
        let value = { received: true };
        if (op.command === 'chat.send') {
          value = { request_id: body.id, message_id: 'fixture.message', status: 'sending', target: op.arguments.target, duplicate: false, confirmation: 'local_queue_admission' };
          queue = [{ id: value.message_id, text: op.arguments.text, target: value.target, status: value.status, created_at_ms: '9007199254740993', updated_at_ms: '9007199254740993', uncertain: false, retarget_required: false, error: null }];
        }
        assert.notEqual(op.command, 'session.new');
        if (op.command === 'session.cd') { cwd = op.arguments.path; value = {cwd}; }
        if (op.command === 'client.update') value = { ok: true, summary: 'Fixture update check complete', details: 'Shared backend receipt' };
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
  const token = '9a'.repeat(32);
  // A legacy explicit observation link can open the directory on a separate user gesture.
  const initial = await page.goto(`http://127.0.0.1:${port}/?session=${foreground}#${token}`);
  assert.match(initial.headers()['content-security-policy'], /script-src 'self'/);
  assert.equal(initial.headers()['content-security-policy'].includes('unsafe-inline'), false);
  await page.locator('#launch-form').waitFor();
  assert.equal(requests.length, 0);
  await page.locator('#launch-back').click();
  const sessionRows = page.locator('.session-item');
  await sessionRows.first().waitFor();
  assert.deepEqual(await sessionRows.locator('strong').allTextContents(), ['真实会话 fixture', '真实会话 fixture']);
  await sessionRows.nth(0).click();
  assert.equal(await sessionRows.nth(0).getAttribute('aria-current'), 'true');
  assert.equal(await sessionRows.nth(1).getAttribute('aria-current'), 'false');
  await sessionRows.nth(1).click();
  await page.locator('#connection-mode').selectOption('control');
  await page.locator('#connect-button').click();
  await page.getByText('来自原生历史的 fixture 文本', { exact: true }).waitFor();
  await page.getByText('Worker 任务快照 · 站点 0', { exact: true }).waitFor();
  assert.equal(requests.find(item => item.path === '/api/gui/v1/connections').body.session, foreground);
  assert.equal(await page.locator('#session-title').textContent(), '真实会话 fixture');
  assert.equal(await sessionRows.nth(0).getAttribute('aria-current'), 'false');
  assert.equal(await sessionRows.nth(1).getAttribute('aria-current'), 'true');
  assert.equal(commands.length, 0, 'connection/state reads must not dispatch commands');
  assert.equal(await page.locator('input[type=password], input[type=file]').count(), 0);
  assert.deepEqual(await page.evaluate(() => [localStorage.length, sessionStorage.length]), [0, 0]);

  async function viewport(width, height) {
    await page.setViewportSize({ width, height });
    await page.waitForFunction(() => {
      const box = document.getElementById('sidebar').getBoundingClientRect();
      return innerWidth <= 700 ? box.right <= 1 : Math.abs(box.left) <= 1;
    });
  }
  const layoutEvidence = [];
  assert.equal(await page.locator('.timeline-row.is-input').count(), inputCases.length);
  for (const width of [1365, 390]) {
    await viewport(width, 1600);
    const bounds = await page.locator('#timeline').boundingBox();
    for (const item of inputCases) {
      const row = page.locator('.timeline-row').filter({ has: page.getByText(item.text, { exact: true }) });
      const box = await row.boundingBox();
      assert.equal(await row.evaluate(node => node.classList.contains('is-input')), true);
      assert.ok(box.x > bounds.x && Math.abs(box.x + box.width - bounds.x - bounds.width) <= 2,
        item.origin + ' must align to the right edge at ' + width);
      assert.equal(await row.evaluate(node => node._row.messageId), item.id);
      layoutEvidence.push({ width, origin: item.origin, message_id: item.id, bounds: box });
    }
  }
  await viewport(1365, 1600);
  const originalDraft = '保留原草稿🦀\n第二行回答';
  const human = inputCases[0];
  const humanRow = page.locator('.timeline-row').filter({ has: page.getByText(human.text, { exact: true }) });
  await page.locator('#chat-text').fill(originalDraft);
  await humanRow.locator('.reply-button').click();
  const quotedHuman = '> 回复消息 ' + human.id + '\n> ' + human.text.replaceAll('\n', '\n> ') + '\n\n' + originalDraft;
  assert.equal(await page.locator('#chat-text').inputValue(), quotedHuman);
  assert.equal(commands.length, 0, 'reply button preserves draft without sending');
  assert.equal(await page.locator('#chat-text').evaluate(node => document.activeElement === node
    && node.selectionStart === node.value.length && node.selectionEnd === node.value.length), true);

  const system = inputCases[1];
  const systemRow = page.locator('.timeline-row').filter({ has: page.getByText(system.text, { exact: true }) });
  await page.locator('#chat-text').fill('选段回复草稿');
  await systemRow.locator('.event-text').evaluate(node => {
    const start = node.textContent.indexOf('继续当前任务');
    const range = document.createRange(); range.setStart(node.firstChild, start);
    range.setEnd(node.firstChild, start + '继续当前任务'.length);
    window.getSelection().removeAllRanges(); window.getSelection().addRange(range);
  });
  await systemRow.locator('.reply-button').click();
  assert.equal(await page.locator('#chat-text').inputValue(),
    '> 回复消息 ' + system.id + '\n> 继续当前任务\n\n选段回复草稿');
  await page.evaluate(() => window.getSelection().removeAllRanges());
  await page.locator('#chat-text').fill(originalDraft);
  await systemRow.focus(); await page.keyboard.press('r');
  assert.equal(await page.locator('#chat-text').inputValue(),
    '> 回复消息 ' + system.id + '\n> ' + system.text + '\n\n' + originalDraft);
  assert.equal(commands.length, 0, 'R shortcut changes only the editable draft');
  await page.locator('#chat-text').fill(quotedHuman);
  if (process.env.WEB_TEST_EVIDENCE) {
    await mkdir(process.env.WEB_TEST_EVIDENCE, { recursive: false });
    for (const [label, width] of [['desktop', 1365], ['narrow', 390]]) {
      await viewport(width, 1600);
      await page.locator('#timeline-scroller').evaluate(node => { node.scrollTop = 0; });
      await page.locator('#chat-text').evaluate(node => { node.scrollTop = 0; });
      await page.screenshot({ path: join(process.env.WEB_TEST_EVIDENCE, 'inputs-reply-' + label + '.png'), animations: 'disabled' });
    }
    await writeFile(join(process.env.WEB_TEST_EVIDENCE, 'input-layout.json'), JSON.stringify({
      scope: 'Actual Chromium renderer with isolated explicit API fixture; no live account delivery claim',
      layouts: layoutEvidence, quoted_text: quotedHuman, commands_before_send: commands.length,
    }, null, 2) + '\n');
  }
  await viewport(1365, 900);
  await page.locator('#timeline-scroller').evaluate(node => { node.scrollTop = node.scrollHeight; });
  await page.locator('#chat-text').fill('');
  const chatDraft = page.locator('#chat-text'), slashMenu = page.getByRole('listbox', { name: '斜杠命令' });
  await chatDraft.fill('/');
  assert.deepEqual(await slashMenu.getByRole('option').locator('code').allTextContents(), ['/new', '/cd']);
  await chatDraft.press('ArrowDown');
  assert.equal(await slashMenu.getByRole('option', {selected:true}).locator('code').textContent(), '/cd');
  await chatDraft.press('ArrowUp');
  await chatDraft.press('Enter');
  assert.equal(await chatDraft.inputValue(), '/new');
  assert.equal(await slashMenu.isVisible(), false);
  assert.equal(await page.locator('#launch-setup').isVisible(), false, 'completion must not execute /new');
  await chatDraft.fill('/c');
  assert.equal(await slashMenu.getByRole('option').count(), 1);
  await chatDraft.press('Tab');
  assert.equal(await chatDraft.inputValue(), '/cd ');
  assert.equal(await chatDraft.evaluate(node => node === document.activeElement && node.selectionStart === 4), true);
  await chatDraft.fill('/c');
  await chatDraft.press('Escape');
  await emit('斜杠草稿刷新'); await page.getByText('斜杠草稿刷新', {exact:true}).waitFor();
  assert.equal(await slashMenu.isVisible(), false);
  assert.equal(await chatDraft.inputValue(), '/c');
  await chatDraft.fill('/unrecognized');
  assert.equal(await slashMenu.isVisible(), false);
  await chatDraft.press('Enter');
  assert.equal(await chatDraft.inputValue(), '/unrecognized\n', 'ordinary Enter still inserts a line break');
  await chatDraft.fill('/n');
  await chatDraft.dispatchEvent('compositionstart');
  assert.equal(await slashMenu.isVisible(), false);
  await chatDraft.press('Control+Enter');
  assert.equal(await chatDraft.inputValue(), '/n');
  await chatDraft.dispatchEvent('compositionend');
  assert.equal(await slashMenu.getByRole('option').count(), 1);
  assert.equal(commands.length, 0, 'choosing, dismissing and composing never dispatch');
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
  await page.locator('#chat-text').fill(quotedHuman);
  await page.locator('#chat-send').click();
  await page.waitForFunction(() => document.getElementById('chat-text').value === '');
  assert.equal(commands.length, 2);
  assert.equal(commands[1].operation.command, 'chat.send');
  assert.equal(commands[1].operation.arguments.text, quotedHuman);
  assert.equal(commands[1].operation.arguments.request_id, commands[1].id);
  assert.equal(commands[1].operation.arguments.target, commands[0].operation.arguments.target);
  assert.equal(commands.some(item => item.operation.command !== 'chat.send'), false,
    'quoted @/$/slash text remains in an ordinary chat.send payload');

  // Scrolling to the bottom of a retained page does not select the latest page.
  // Enter that state deliberately, then follow the same action as a reader.
  await viewport(1365, 650);
  await page.locator('#timeline-scroller').evaluate(node => { node.scrollTop = 0; });
  await emit('返回最新页的定位事件');
  await page.waitForFunction(expected => document.getElementById('status-cursor').textContent === `cursor ${expected}`
    && !document.getElementById('timeline-newer').disabled, cursor);
  assert.equal(await page.getByText('返回最新页的定位事件', { exact: true }).count(), 0);
  await page.locator('#new-events').click();
  await page.getByText('返回最新页的定位事件', { exact: true }).waitFor();
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

  await page.locator('#tab-workers').click();
  assert.equal(await page.locator('#worker-productivity').textContent(), productivity.plot.join('\n'));
  assert.equal(await page.locator('#worker-summary .metric').first().textContent(), '1Dreaming');
  assert.equal(await page.locator('#worker-list .worker-card').nth(1).locator('.pill').textContent(), '☾ Dreaming');
  await page.locator('#update-button').click();
  await page.getByRole('button', { name: '检查更新' }).click();
  await page.getByText('Fixture update check complete', { exact: false }).waitFor();
  assert.deepEqual(commands.filter(item => item.operation.command === 'client.update').map(item => item.operation.arguments), [{ check: true }]);
  await page.locator('[data-close="details-dialog"]').click();
  const metricsBox = await page.locator('#worker-summary').boundingBox();
  const plotBox = await page.locator('.worker-productivity').boundingBox();
  assert.ok(plotBox.x >= metricsBox.x + metricsBox.width);

  for (const width of [390, 320]) {
    await viewport(width, 844);
    await page.locator('#menu-button').click();
    const drawer = page.locator('#sidebar');
    const settings = drawer.locator('#sidebar-settings');
    assert.equal(await settings.evaluate(el => el.open), false);
    assert.equal(await page.locator('#close-menu').evaluate(el => el === document.activeElement), true);
    assert.equal(await drawer.evaluate(el => el.scrollWidth <= el.clientWidth), true);
    assert.ok((await drawer.locator('[aria-selected="true"]').boundingBox()).height >= 44);
    await settings.locator('summary').click();
    assert.equal(await drawer.locator('#diagnostics-button').isVisible(), true);
    await settings.locator('summary').click();
    await page.locator('#dismiss-sidebar').click({ position: { x: width - 8, y: 40 } });
    assert.equal(await page.locator('#menu-button').evaluate(el => el === document.activeElement), true);
    for (const tab of ['conversation', 'terminal', 'workers', 'waits']) {
      await page.locator('#menu-button').click();
      await page.locator(`#tab-${tab}`).click();
      assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), `${tab} must fit ${width}px viewport`);
      const panel = await page.locator(`#panel-${tab}`).boundingBox();
      assert.ok(panel && panel.x >= 0 && panel.x + panel.width <= width + 1);
    }
  }
  await page.locator('#menu-button').click();
  await page.locator('#tab-conversation').click();
  await page.locator('#menu-button').click();
  assert.equal(await page.locator('#menu-button').getAttribute('aria-expanded'), 'true');
  await page.keyboard.press('Escape');
  assert.equal(await page.locator('#menu-button').getAttribute('aria-expanded'), 'false');
  // Complete a command in place with an actual touch. Preserve the existing path and caret.
  await chatDraft.fill('/c   已有 路径');
  await chatDraft.evaluate(node => node.setSelectionRange(2, 2));
  const cdOption = slashMenu.getByRole('option', {name:'/cd 切换工作目录'});
  const touchBox = await cdOption.boundingBox();
  assert.ok(touchBox && touchBox.height >= 44);
  const touch = await page.context().newCDPSession(page);
  await touch.send('Emulation.setTouchEmulationEnabled', {enabled:true});
  await touch.send('Input.dispatchTouchEvent', {type:'touchStart', touchPoints:[{x:touchBox.x + 20, y:touchBox.y + touchBox.height / 2}]});
  await touch.send('Input.dispatchTouchEvent', {type:'touchEnd', touchPoints:[]});
  await touch.send('Emulation.setTouchEmulationEnabled', {enabled:false});
  await touch.detach();
  await page.waitForFunction(() => document.querySelector('#chat-text').value === '/cd   已有 路径');
  assert.equal(await chatDraft.evaluate(node => node === document.activeElement && node.selectionStart === 6), true);
  assert.equal(commands.some(item => item.operation.command === 'session.cd'), false);
  // A mobile hotkey is one native terminal write and does not submit the message draft.
  failTerminal = false;
  await page.locator('#chat-text').fill('留在原会话');
  await page.locator('#terminal-keys [data-key="up"]').click();
  await page.waitForFunction(() => !document.querySelector('#new-session').disabled);
  for (let i=0; i<100 && !commands.some(item => item.operation.arguments.data === '\x1b[A'); i++) await delay(10);
  assert.equal(commands.at(-1).operation.arguments.data, '\x1b[A');
  assert.equal(await page.locator('#chat-text').inputValue(), '留在原会话');
  await page.locator('#new-session').click();
  await page.locator('#launch-name').fill('session1');
  await page.locator('#launch-relay').fill('relay.example');
  await page.locator('#launch-connect').click();
  await page.getByText('创建结果未确定，请点“核对上次创建”重试原请求。').waitFor();
  assert.equal(await page.locator('#launch-name').isDisabled(), true);
  assert.equal(requests.filter(r => r.path.endsWith('/owned-sessions')).length, 1);
  await page.locator('#launch-connect').click();
  await page.locator('.session-tab[aria-selected="true"]').filter({hasText:'session1@relay.example'}).waitFor();
  assert.equal(await page.locator('#chat-text').inputValue(), '');
  const firstOwned = [...owned.values()][0];
  const opens = () => requests.filter(r => r.path === '/api/gui/v1/connections' && r.body.session === firstOwned);
  assert.deepEqual(opens().map(r => r.body.read_only), [true]);
  await page.locator('#take-control').click();
  await page.waitForFunction(() => document.querySelector('#take-control').hidden && !document.querySelector('#connected-actions').hidden);
  assert.deepEqual(opens().map(r => r.body.read_only), [true, false]);
  await page.locator('#chat-text').fill('/cd /fixture/目录 with space');
  await page.locator('#chat-send').click();
  await page.waitForFunction(() => document.querySelector('#session-cwd').textContent === '/fixture/目录 with space');
  await page.locator('#chat-text').fill('/new');
  await page.locator('#chat-send').click();
  await page.locator('#launch-name').fill('session2');
  await page.locator('#launch-relay').fill('relay.example');
  await page.locator('#launch-connect').click();
  await page.locator('.session-tab[aria-selected="true"]').filter({hasText:'session2@relay.example'}).waitFor();
  assert.equal(newCount, 2);
  await page.locator('.session-tab').first().click();
  await page.waitForFunction(() => document.getElementById('chat-text').value === '留在原会话');
  assert.equal(commands.filter(item => item.operation.command === 'chat.send').length, 2);
  if (process.env.WEB_TEST_EVIDENCE) {
    await viewport(390, 844);
    await page.screenshot({ path: join(process.env.WEB_TEST_EVIDENCE, 'session-mobile.png') });
    await page.locator('#menu-button').click();
    await page.screenshot({ path: join(process.env.WEB_TEST_EVIDENCE, 'session-mobile-sidebar.png') });
    await page.locator('#close-menu').click();
  }
  await viewport(1365, 900);
  assert.equal(await page.locator('#end-session').isVisible(), false, 'a manually attached CLI is never page-owned');
  assert.equal(endedTargets.length, 0, 'switching tabs and disconnecting do not end sessions');
  const firstTab = page.locator('.session-tab').filter({hasText:'session1@relay.example'});
  await firstTab.click();
  await page.locator('#end-session').click();
  assert.equal(await page.locator('#end-session-target').textContent(), firstOwned);
  await page.locator('#end-session-cancel').click();
  assert.equal(endedTargets.length, 0);
  await page.locator('#end-session').click(); await page.locator('#end-session-confirm').click();
  const loseEndReply = await firstEnd;
  await page.keyboard.press('Escape');
  assert.equal(await page.locator('#end-session-dialog').evaluate(dialog => dialog.open), true);
  assert.equal(await page.locator('#end-session-confirm').isDisabled(), true);
  await loseEndReply();
  await page.getByText(/结束结果未确定。可对上方同一会话重试/).waitFor();
  assert.deepEqual(endedTargets, [firstOwned]);
  await page.locator('#end-session-cancel').click();
  await page.locator('.session-tab').first().click();
  await page.locator('#end-session').waitFor({state:'hidden'});
  await firstTab.click();
  await page.locator('#disconnect-button').click();
  await page.waitForFunction(() => document.querySelector('#connected-actions').hidden);
  assert.deepEqual(endedTargets, [firstOwned]);
  await page.getByRole('button', {name:'重试结束此会话'}).click(); await page.locator('#end-session-confirm').click();
  await page.waitForFunction(() => !document.querySelector('#end-session-dialog').open && document.querySelector('#end-session').hidden);
  assert.equal(await firstTab.count(), 0);
  assert.equal(await page.locator('.session-tab').count(), 2);
  assert.equal(await page.locator('#connect-strip').isVisible(), false);
  assert.deepEqual(endedTargets, [firstOwned, firstOwned]);
  const secondTab = page.locator('.session-tab').filter({hasText:'session2@relay.example'});
  await secondTab.click(); await page.locator('#end-session').click();
  const secondOwned = [...owned.values()][1];
  assert.equal(await page.locator('#end-session-target').textContent(), secondOwned);
  await page.locator('#end-session-confirm').click();
  await page.waitForFunction(() => !document.querySelector('#end-session-dialog').open && document.querySelector('#connected-actions').hidden);
  assert.equal(await page.locator('.session-tab').count(), 1, 'only the manual CLI tab remains');
  assert.equal(await page.locator('#chat-text').inputValue(), '');
  assert.deepEqual(endedTargets, [firstOwned, firstOwned, secondOwned]);
  assert.deepEqual(errors, []);
  assert.deepEqual(await page.evaluate(() => window.cspViolations), []);
  assert.ok(requests.every(request => request.authorization === 'Bearer ' + token));
  assert.equal(requests.some(request => JSON.stringify(request.body).includes(token)), false);
  assert.equal(commands.some(item => item.operation.command === 'terminal.resize'), false, 'viewport changes are not terminal commands');
  // Only the Magisk build marks its loopback landing document.
  const moduleTab = await browser.newPage();
  const moduleHome = 'http://127.0.0.1:42387/';
  const bootstrapRequests = [];
  await moduleTab.route(moduleHome + '**', async route => {
    const request = route.request(), path = new URL(request.url()).pathname;
    if (path.startsWith('/api/')) {
      bootstrapRequests.push(path);
      assert.equal(path, '/api/gui/v1/owned-sessions'); assert.equal(request.method(), 'POST');
      assert.equal(request.headers().authorization, 'Bearer ' + token);
      return route.fulfill({status:503, contentType:'application/json',
        body:JSON.stringify({error:{code:'unavailable',effect:'none',message:'缺少普通用户运行器。'}})});
    }
    const asset = await fetch('http://127.0.0.1:' + port + path);
    assert.equal(asset.status, 200, path);
    const source = await asset.text();
    return route.fulfill({ status: 200, contentType: asset.headers.get('content-type'),
      body: path === '/' ? source.replace('<html ', '<html data-ssmcp-magisk ') : source });
  });
  await moduleTab.goto(moduleHome);
  await moduleTab.getByText(/Magisk 管理器.*执行操作/).waitFor();
  assert.equal(await moduleTab.locator('#launch-setup').isVisible(), false);
  assert.deepEqual(bootstrapRequests, []);
  await moduleTab.goto('about:blank');
  await moduleTab.goto(moduleHome + '#' + token);
  await moduleTab.waitForFunction(() => document.querySelector('#auth-status')?.textContent === '已设置认证', null, { timeout: 8000 });
  await moduleTab.locator('#launch-setup').waitFor();
  assert.equal(await moduleTab.locator('#launch-connect').isVisible(), true);
  assert.equal(await moduleTab.locator('#launch-back').isVisible(), false);
  assert.equal(moduleTab.url(), moduleHome);
  assert.deepEqual(bootstrapRequests, []);
  await moduleTab.locator('#launch-name').fill('module');
  await moduleTab.locator('#launch-relay').fill('relay.example');
  await moduleTab.locator('#launch-connect').click();
  await moduleTab.getByText('缺少普通用户运行器。', {exact:true}).waitFor();
  assert.deepEqual(bootstrapRequests, ['/api/gui/v1/owned-sessions']);
  assert.equal(await moduleTab.locator('#workbench-shell').isVisible(), false);
  await moduleTab.close();
  const mobile = await browser.newPage({
    viewport:{width:320,height:640},
    userAgent:'Mozilla/5.0 (Linux; Android 14; Fixture Build/TOY; wv) AppleWebKit/537.36 Version/4.0 Chrome/120.0 Mobile Safari/537.36'
  });
  let mobileApiCalls = 0;
  await mobile.route('**/api/gui/v1/**', r => { mobileApiCalls++; return r.abort(); });
  await mobile.goto(`http://127.0.0.1:${port}/`);
  await mobile.locator('#entry-hint').getByText('请从设备上的 SSMCP 应用打开。').waitFor();
  assert.equal(await mobile.locator('input[type=file], input[type=password]').count(), 0);
  assert.equal(await mobile.locator('#launch-form').isVisible(), false);
  assert.equal(await mobile.evaluate(() => document.documentElement.dataset.ssmcpPairingReady), '1');
  await mobile.evaluate(token => new Promise(resolve => {
    window.addEventListener('message', () => setTimeout(resolve, 0), {once:true});
    window.postMessage(token, location.origin);
  }), token);
  await mobile.evaluate(token => window.dispatchEvent(new MessageEvent('message', {data:token,source:null})), token);
  assert.equal(await mobile.locator('#launch-form').isVisible(), false);
  assert.equal(mobileApiCalls, 0);
  await mobile.close();
  const setup = await browser.newPage({viewport:{width:390,height:844}});
  const starts = [];
  await setup.route('**/api/gui/v1/**', async route => {
    const path = new URL(route.request().url()).pathname;
    assert.equal(path, '/api/gui/v1/owned-sessions');
    assert.equal(route.request().headers().authorization, 'Bearer ' + token);
    const args = route.request().postDataJSON();
    starts.push(args);
    if (args.destination.endsWith('@ssmcp.lmm.best') && !(args.terms && args.privacy)) {
      return route.fulfill({status:400, contentType:'application/json',
        body:JSON.stringify({error:{code:'invalid_request', effect:'none', message:"confirm the hosted relay's terms and privacy policy"}})});
    }
    await route.fulfill({status:400, contentType:'application/json', body:JSON.stringify({error:{code:'denied', effect:'none', message:'测试账号未注册'}})});
  });
  await setup.goto(`http://127.0.0.1:${port}/?setup=1#${token}`);
  await setup.locator('#launch-name').waitFor();
  assert.equal(await setup.locator('#launch-relay').inputValue(), 'ssmcp.lmm.best');
  assert.equal(starts.length, 0);
  if (process.env.WEB_TEST_EVIDENCE) await setup.screenshot({path:join(process.env.WEB_TEST_EVIDENCE, 'first-connection-mobile.png')});
  assert.deepEqual(await setup.locator('#launch-form input:visible').evaluateAll(inputs => inputs.map(input => input.id)), ['launch-name','launch-relay']);
  assert.deepEqual(await setup.locator('#launch-form button:visible').allTextContents(), ['连接']);
  assert.equal(await setup.locator('#launch-name').evaluate(e => e === document.activeElement), true);
  await setup.setViewportSize({width:320,height:640});
  assert.equal(await setup.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
  await setup.locator('#launch-name').fill('new-user');
  await setup.locator('#launch-name').press('Enter');
  await setup.locator('#launch-hosted').waitFor();
  assert.equal(starts.length, 1);
  assert.deepEqual([starts[0].terms,starts[0].privacy], [false,false]);
  for (const id of ['launch-terms','launch-privacy']) assert.equal(await setup.locator('#'+id).isChecked(), false);
  await setup.locator('#launch-terms').check(); await setup.locator('#launch-privacy').check();
  await setup.locator('#launch-connect').click();
  await setup.getByText('测试账号未注册').waitFor();
  assert.deepEqual([starts[1].terms,starts[1].privacy], [true,true]);
  await setup.locator('#launch-relay').fill('my.example');
  assert.equal(await setup.locator('#launch-hosted').isVisible(), false);
  await setup.locator('#launch-relay').press('Enter');
  await setup.waitForFunction(() => !document.querySelector('#launch-connect').disabled);
  assert.deepEqual(starts[2], {request_id:starts[2].request_id, destination:'new-user@my.example', terms:false, privacy:false});
  await setup.close();
  process.stdout.write('Browser fixture smoke passed: CSP, protocol envelopes, IME/focus, bounded DOM, scroll continuity, queue receipt, five right-aligned input origins, original-ID quote button/selected excerpt/R shortcut, preserved multiline draft and ordinary quoted delivery, unknown command, mobile panels.\n');
} finally {
  if (browser) await browser.close();
  server.kill('SIGTERM');
}
