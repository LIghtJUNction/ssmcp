// Protocol tests for the real code_mode worker. No desktop, network or model is used.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { createInterface } from 'node:readline';
import test from 'node:test';

const worker = process.env.SSMCP_CODE_MODE_SCRIPT
  ?? fileURLToPath(new URL('../src/code_mode.js', import.meta.url));
const png = {
  type: 'image', mimeType: 'image/png',
  data: 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aJXcAAAAASUVORK5CYII=',
};

function run(code, reply) {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [worker], { stdio: ['pipe', 'pipe', 'pipe'] });
    const frames = [];
    const requests = [];
    let stderr = '';
    let failure;
    const stop = error => {
      failure ??= error;
      child.kill();
    };
    const timer = setTimeout(() => stop(new Error('code_mode fixture timed out')), 10000);
    child.on('error', stop);
    child.stdin.on('error', stop);
    child.stderr.on('data', data => { stderr += data; });
    const input = createInterface({ input: child.stdout });
    input.on('line', line => {
      try {
        const frame = JSON.parse(line);
        if (frame.type === 'call') {
          requests.push(frame.request);
          const response = reply(frame.request);
          child.stdin.write(JSON.stringify({
            type: 'reply', id: frame.id, ...response,
          }) + '\n');
        } else {
          frames.push(frame);
        }
      } catch (error) { stop(error); }
    });
    child.on('close', (exitCode, signal) => {
      clearTimeout(timer);
      input.close();
      if (failure) reject(failure);
      else resolve({ frames, requests, exitCode, signal, stderr });
    });
    child.stdin.write(JSON.stringify({
      type: 'run', code, clients: [{ id: 'one', type: 'shell', name: 'fixture' }],
    }) + '\n');
  });
}

const result = record => record.frames.find(frame => frame.type === 'result')?.value;
const error = record => record.frames.find(frame => frame.type === 'error')?.message;

test('CUA routes desktop and browser operations through the existing native bridge', async () => {
  const commands = [
    ['status', {}],
    ['session_start', { width: 1280, height: 800 }],
    ['launch', { command: ['fixture-app', '--new-instance'] }],
    ['monitors', {}],
    ['windows', {}],
    ['focus', { window_id: '0x1234' }],
    ['screenshot', { region: { x: -100, y: 20, width: 400, height: 300 } }],
    ['move', { x: -10, y: 50 }],
    ['click', { x: 20, y: 30, button: 'right', count: 2 }],
    ['drag', { path: [{ x: 1, y: 2 }, { x: 30, y: 40 }], duration_ms: 100 }],
    ['key', { key: 'Return', modifiers: ['ctrl'] }],
    ['type', { text: '中文 ` $() \\ Unicode' }],
    ['scroll', { direction: 'down', steps: 2 }],
    ['session_stop', {}],
  ];
  const browserCommands = [
    ['launch', {}],
    ['open', { url: 'about:blank' }],
    ['navigate', { target_id: 't', url: 'about:blank' }],
    ['act', { target_id: 't', actions: [
      { type: 'fill', ref: 's1e1', text: '中文表单' },
      { type: 'click', ref: 's1e2' },
      { type: 'wait', selector: '#result', text: 'Ready', timeout_ms: 5000 },
    ] }],
    ['wait', { target_id: 't', selector: '#result', state: 'visible' }],
  ];
  const record = await run(`
    const answers = [];
    for (const [name, args] of ${JSON.stringify(commands)}) {
      answers.push(await cua[name]('one', args));
    }
    for (const [name, args] of ${JSON.stringify(browserCommands)}) {
      answers.push(await cua.browser[name]('one', args));
    }
    return answers;
  `, request => ({
    ok: true,
    value: {
      content: [{ type: 'text', text: 'native-result' }],
      structuredContent: { name: request.name, arguments: request.arguments },
      isError: false, _meta: { native: ['kept', 1] }, futureField: { version: 2 },
    },
  }));
  assert.equal(record.exitCode, 0, record.stderr || error(record));
  const expectedNames = [
    ...commands.map(([name]) => 'ssmcp.computer.' + name),
    ...browserCommands.map(([name]) => 'ssmcp.browser.' + name),
  ];
  assert.deepEqual(record.requests.map(request => request.name), expectedNames);
  record.requests.forEach(request => {
    assert.equal(request.op, 'call');
    assert.equal(request.client_id, 'one');
  });
  commands.forEach(([, args], index) => assert.deepEqual(record.requests[index].arguments, args));
  browserCommands.forEach(([, args], index) => {
    assert.deepEqual(record.requests[commands.length + index].arguments, args);
  });
  result(record).forEach(value => {
    assert.equal(value.isError, false);
    assert.deepEqual(value._meta, { native: ['kept', 1] });
    assert.deepEqual(value.futureField, { version: 2 });
  });
});

test('CUA preserves partial browser batch errors without replay or desktop fallback', async () => {
  const native = {
    isError: true,
    content: [{ type: 'text', text: 'ref is not from the latest snapshot' }],
    structuredContent: {
      target_id: 't', completed_count: 1,
      completed: [{ index: 0, type: 'click', result: { clicked: true } }],
      phase: 'actions', failed_index: 1, replay_safe: false,
    },
  };
  const actions = [
    { type: 'click', ref: 's1e1' },
    { type: 'click', ref: 'stale-ref' },
    { type: 'click', ref: 's1e2' },
  ];
  const record = await run(`
    return await cua.browser.act('one', {target_id:'t', actions:${JSON.stringify(actions)}});
  `, request => {
    assert.equal(request.name, 'ssmcp.browser.act');
    return { ok: true, value: native };
  });
  assert.equal(record.exitCode, 0, record.stderr || error(record));
  assert.deepEqual(result(record), native);
  assert.deepEqual(record.requests.map(request => request.name), ['ssmcp.browser.act']);
  assert.deepEqual(record.requests[0].arguments, { target_id: 't', actions });
});

test('CUA screenshots keep image bytes out of text logs and preserve coordinate metadata', async () => {
  const metadata = {
    image_to_desktop: { origin_x: -100, origin_y: 20, scale_x: 2, scale_y: 2 },
    width: 1, height: 1,
  };
  const record = await run(`
    const shot = await cua.screenshot('one', {max_width:800});
    image(shot);
    print('screenshot captured');
    return shot.structuredContent;
  `, request => {
    assert.equal(request.name, 'ssmcp.computer.screenshot');
    return { ok: true, value: { isError: false, content: [png], structuredContent: metadata } };
  });
  assert.equal(record.exitCode, 0, record.stderr || error(record));
  assert.deepEqual(result(record), metadata);
  assert.deepEqual(record.frames.filter(frame => frame.type === 'image').map(frame => frame.content), [png]);
  assert.deepEqual(record.frames.filter(frame => frame.type === 'log').map(frame => frame.line), ['screenshot captured']);
  assert(!JSON.stringify(record.frames.filter(frame => frame.type !== 'image')).includes(png.data));
});

test('native CUA errors remain observable and failed screenshots are not displayed', async () => {
  const native = {
    isError: true, content: [{ type: 'text', text: 'desktop permission unavailable' }, png],
    structuredContent: { permissions: { screenshot: false } }, _meta: { native: 'error' },
  };
  const record = await run(`
    const failed = await cua.screenshot('one');
    let displayError;
    try { image(failed); } catch (error) { displayError = error.message; }
    return {failed, displayError};
  `, () => ({ ok: true, value: native }));
  assert.equal(record.exitCode, 0, record.stderr || error(record));
  assert.deepEqual(result(record).failed, native);
  assert.match(result(record).displayError, /failed native.*desktop permission unavailable/);
  assert(!record.frames.some(frame => frame.type === 'image'));
});

test('CUA preserves transport failures without starting any fallback model', async () => {
  const record = await run(`
    try { await cua.click('one', {x:1,y:2}); }
    catch (error) { return {reason:error.message}; }
    throw new Error('expected a native transport error');
  `, () => ({ ok: false, error: 'client disconnected' }));
  assert.equal(record.exitCode, 0, record.stderr || error(record));
  assert.deepEqual(result(record), { reason: 'client disconnected' });
  assert.deepEqual(record.requests.map(request => request.name), ['ssmcp.computer.click']);
});

test('CUA shares existing implementations and does not alter discovered Codex MCP results', async () => {
  const native = {
    content: [png], structuredContent: { provider: 'fixture-cua', nativeShape: ['untouched'] },
    isError: false, _meta: { untouched: true },
  };
  const record = await run(`
    const response = await call('one','codex.mcp.fixture-cua.screenshot',{screen:0});
    image(response);
    return {
      response,
      same:cua.screenshot===computer.screenshot && cua.browser===browser,
      frozen:Object.isFrozen(cua) && Object.isFrozen(cua.browser),
      hidden:typeof __ssmcp_host
    };
  `, request => {
    assert.equal(request.name, 'codex.mcp.fixture-cua.screenshot');
    return { ok: true, value: native };
  });
  assert.equal(record.exitCode, 0, record.stderr || error(record));
  assert.deepEqual(result(record).response, native);
  assert.equal(result(record).same, true);
  assert.equal(result(record).frozen, true);
  assert.equal(result(record).hidden, 'undefined');
  assert.deepEqual(record.frames.filter(frame => frame.type === 'image').map(frame => frame.content), [png]);
});
