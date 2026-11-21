import { officialSources } from '../src/tool_repository/fixture.mjs';
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
      tool_sources: officialSources(), type: 'run', code, clients: [{ id: 'one', type: 'shell', name: 'fixture' }],
    }) + '\n');
  });
}

const result = record => record.frames.find(frame => frame.type === 'result')?.value;
const error = record => record.frames.find(frame => frame.type === 'error')?.message;

test('computer routes only Zuse lifecycle operations through the native bridge', async () => {
  const commands = [
    ['version', {}],
    ['download', {}],
    ['download', { version: 'v0.3.0' }],
    ['install', { download_id: 'download-fixture' }],
    ['uninstall', { installation_id: 'installation-fixture' }],
  ];
  const record = await run(`
    const answers = [];
    for (const [name, args] of ${JSON.stringify(commands)}) {
      answers.push(await computer[name]('one', args));
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
  assert.deepEqual(record.requests.map(request => request.name),
    commands.map(([name]) => 'ssmcp.computer.' + name));
  record.requests.forEach((request, index) => {
    assert.equal(request.op, 'call');
    assert.equal(request.client_id, 'one');
    assert.deepEqual(request.arguments, commands[index][1]);
  });
  result(record).forEach(value => {
    assert.equal(value.isError, false);
    assert.deepEqual(value._meta, { native: ['kept', 1] });
    assert.deepEqual(value.futureField, { version: 2 });
  });
});

test('retired embedded control facades are absent and cannot dispatch a fallback', async () => {
  const record = await run(`
    return {
      methods: Object.keys(computer).sort(),
      frozen: Object.isFrozen(computer),
      browser: typeof browser,
      cua: typeof cua,
      omarchy: typeof omarchy,
      actions: ['status', 'toggle', 'desktop_start', 'screenshot', 'click', 'key', 'type']
        .map(name => typeof computer[name]),
      hidden: typeof __ssmcp_host
    };
  `, () => { throw new Error('retired facades must not send a native request'); });
  assert.equal(record.exitCode, 0, record.stderr || error(record));
  assert.deepEqual(result(record), {
    methods: ['download', 'install', 'uninstall', 'version'],
    frozen: true,
    browser: 'undefined',
    cua: 'undefined',
    omarchy: 'undefined',
    actions: Array(7).fill('undefined'),
    hidden: 'undefined',
  });
  assert.deepEqual(record.requests, []);
});

test('native images keep bytes out of text logs and preserve metadata', async () => {
  const metadata = { width: 1, height: 1, source: 'fixture.png' };
  const record = await run(`
    const picture = await call('one', 'ssmcp.fs.read_image', {path:'fixture.png'});
    image(picture);
    print('image read');
    return picture.structuredContent;
  `, request => {
    assert.equal(request.name, 'ssmcp.fs.read_image');
    return { ok: true, value: { isError: false, content: [png], structuredContent: metadata } };
  });
  assert.equal(record.exitCode, 0, record.stderr || error(record));
  assert.deepEqual(result(record), metadata);
  assert.deepEqual(record.frames.filter(frame => frame.type === 'image').map(frame => frame.content), [png]);
  assert.deepEqual(record.frames.filter(frame => frame.type === 'log').map(frame => frame.line), ['image read']);
  assert(!JSON.stringify(record.frames.filter(frame => frame.type !== 'image')).includes(png.data));
});

test('native errors remain observable and failed images are not displayed', async () => {
  const native = {
    isError: true, content: [{ type: 'text', text: 'file permission unavailable' }, png],
    structuredContent: { readable: false }, _meta: { native: 'error' },
  };
  const record = await run(`
    const failed = await call('one', 'ssmcp.fs.read_image', {path:'fixture.png'});
    let displayError;
    try { image(failed); } catch (error) { displayError = error.message; }
    return {failed, displayError};
  `, () => ({ ok: true, value: native }));
  assert.equal(record.exitCode, 0, record.stderr || error(record));
  assert.deepEqual(result(record).failed, native);
  assert.match(result(record).displayError, /failed native.*file permission unavailable/);
  assert(!record.frames.some(frame => frame.type === 'image'));
});

test('computer preserves transport failures without starting any fallback', async () => {
  const record = await run(`
    try { await computer.install('one', {download_id:'download-fixture'}); }
    catch (error) { return {reason:error.message}; }
    throw new Error('expected a native transport error');
  `, () => ({ ok: false, error: 'client disconnected' }));
  assert.equal(record.exitCode, 0, record.stderr || error(record));
  assert.deepEqual(result(record), { reason: 'client disconnected' });
  assert.deepEqual(record.requests.map(request => request.name), ['ssmcp.computer.install']);
});

test('generic calls do not alter discovered MCP results or native images', async () => {
  const native = {
    content: [png], structuredContent: { provider: 'fixture-images', nativeShape: ['untouched'] },
    isError: false, _meta: { untouched: true },
  };
  const record = await run(`
    const response = await call('one','codex.mcp.fixture-images.read',{path:'fixture.png'});
    image(response);
    return {response, hidden:typeof __ssmcp_host};
  `, request => {
    assert.equal(request.name, 'codex.mcp.fixture-images.read');
    return { ok: true, value: native };
  });
  assert.equal(record.exitCode, 0, record.stderr || error(record));
  assert.deepEqual(result(record).response, native);
  assert.equal(result(record).hidden, 'undefined');
  assert.deepEqual(record.frames.filter(frame => frame.type === 'image').map(frame => frame.content), [png]);
});

test('image forwards GIF87a, GIF89a and WebP native image blocks on the image channel', async () => {
  const gif89Bytes = Buffer.from('R0lGODlhAQABAIAAAAAAAP///ywAAAAAAQABAAACAUwAOw==', 'base64');
  const gif87Bytes = Buffer.from(gif89Bytes);
  gif87Bytes.write('GIF87a', 0);
  // Header validation matches the native validator; this bridge does not decode codecs.
  const images = [
    { type: 'image', mimeType: 'image/gif', data: gif87Bytes.toString('base64') },
    { type: 'image', mimeType: 'image/gif', data: gif89Bytes.toString('base64') },
    { type: 'image', mimeType: 'image/webp', data: Buffer.from('RIFF\0\0\0\0WEBP').toString('base64') },
  ];
  const record = await run(`
    const response = await call('one', 'ssmcp.fs.read_image', {path:'fixture'});
    image(response);
    return response.structuredContent;
  `, () => ({
    ok: true, value: { content: images, structuredContent: { count: images.length } },
  }));
  assert.equal(record.exitCode, 0, record.stderr || error(record));
  assert.deepEqual(result(record), { count: images.length });
  assert.deepEqual(record.frames.filter(frame => frame.type === 'image').map(frame => frame.content), images);
  const nonImages = JSON.stringify(record.frames.filter(frame => frame.type !== 'image'));
  for (const image of images) assert(!nonImages.includes(image.data));
});

test('image rejects mismatched, truncated and unsupported raster headers and noncanonical base64', async () => {
  const block = (mimeType, data) => ({ type: 'image', mimeType, data });
  const base64 = text => Buffer.from(text).toString('base64');
  const cases = [
    [block('image/gif', png.data), /declared raster MIME/],
    [block('image/webp', base64('GIF89a')), /declared raster MIME/],
    [block('image/jpeg', base64('RIFF\0\0\0\0WEBP')), /declared raster MIME/],
    [block('image/gif', base64('GIF88a')), /declared raster MIME/],
    [block('image/gif', base64('GIF87')), /declared raster MIME/],
    [block('image/webp', base64('RIFF\0\0\0\0WAVE')), /declared raster MIME/],
    [block('image/webp', base64('RIFF\0\0\0\0WEB')), /declared raster MIME/],
    [block('image/svg+xml', base64('<svg/>')), /PNG, JPEG, GIF or WebP/],
    [block('image/gif', base64('GIF89a') + '\n'), /canonical base64/],
    // Nonzero pad bits decode to the same bytes but are not canonical base64.
    [block('image/gif', 'R0lGODlhAB=='), /canonical base64/],
  ];
  for (const [content, expected] of cases) {
    const record = await run(`
      const response = await call('one', 'ssmcp.fs.read_image', {path:'fixture'});
      try { image(response); } catch (error) { return {reason:error.message}; }
      throw new Error('expected image rejection');
    `, () => ({ ok: true, value: { content: [content] } }));
    assert.equal(record.exitCode, 0, record.stderr || error(record));
    assert.match(result(record).reason, expected, JSON.stringify(content));
    assert(!record.frames.some(frame => frame.type === 'image'));
  }
});

test('GIF and WebP retain decoded size, image count and encoded aggregate budgets', async () => {
  const webp = size => {
    const bytes = Buffer.alloc(size);
    bytes.write('RIFF', 0);
    bytes.write('WEBP', 8);
    return { type: 'image', mimeType: 'image/webp', data: bytes.toString('base64') };
  };
  const gif = { type: 'image', mimeType: 'image/gif', data: Buffer.from('GIF89a').toString('base64') };
  const cases = [
    { content: webp(400 * 1024), attempts: 1, accepted: 1, reason: null },
    { content: webp(400 * 1024 + 1), attempts: 1, accepted: 0, reason: /within 400 KiB/ },
    { content: gif, attempts: 5, accepted: 4, reason: /4 images or 2 MiB/ },
    { content: webp(400 * 1024), attempts: 4, accepted: 3, reason: /4 images or 2 MiB/ },
  ];
  for (const fixture of cases) {
    const record = await run(`
      const response = await call('one', 'ssmcp.fs.read_image', {path:'fixture'});
      let accepted = 0;
      try {
        for (let i = 0; i < ${fixture.attempts}; i++) { image(response); accepted++; }
      } catch (error) { return {accepted, reason:error.message}; }
      return {accepted, reason:null};
    `, () => ({ ok: true, value: { content: [fixture.content] } }));
    assert.equal(record.exitCode, 0, record.stderr || error(record));
    assert.equal(result(record).accepted, fixture.accepted);
    assert.equal(record.frames.filter(frame => frame.type === 'image').length, fixture.accepted);
    if (fixture.reason) assert.match(result(record).reason, fixture.reason);
    else assert.equal(result(record).reason, null);
  }
});

test('ui.present preserves typed arguments and the native acceptance receipt', async () => {
  const document = {
    title: '中文结果', kind: 'answer', idempotency_key: 'presentation_1',
    blocks: [{ type: 'metrics', items: [{ label: '检查', value: '42' }] }],
  };
  const receipt = {
    shown: true, accepted: true, message_id: 'message_native',
    history_saved: true, duplicate: false, format: 'intelligence_ui.v1', block_count: 1,
  };
  const record = await run('return await ui.present("one", ' + JSON.stringify(document) + ');',
    request => {
      assert.equal(request.name, 'ssmcp.ui.present');
      assert.equal(request.client_id, 'one');
      assert.deepEqual(request.arguments, document);
      return { ok: true, value: { structuredContent: receipt, isError: false } };
    });
  assert.equal(record.exitCode, 0, record.stderr || error(record));
  assert.deepEqual(result(record), receipt);
  assert.equal(record.requests.length, 1);
  const rejected = await run(
    'try { await ui.present("one", {}); } catch (error) { return {reason:error.message}; }',
    () => ({ ok: true, value: {
      isError: true, content: [{ type: 'text', text: 'invalid presentation: title required' }],
    } }));
  assert.equal(rejected.exitCode, 0, rejected.stderr || error(rejected));
  assert.deepEqual(result(rejected), { reason: 'invalid presentation: title required' });
});
