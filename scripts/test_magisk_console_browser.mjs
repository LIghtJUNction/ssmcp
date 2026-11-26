// Real manager entry in mobile Chromium, with an isolated KernelSU bridge.
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { resolve } from 'node:path';

const { chromium } = await import(process.env.PLAYWRIGHT_MODULE || 'playwright');
const root = resolve('packaging/magisk/src/ssmcp/webroot');
const server = createServer(async (req, res) => {
  const file = { '/': 'index.html', '/open.js': 'open.js' }[req.url];
  if (!file) { res.writeHead(404).end(); return; }
  try {
    res.setHeader('Content-Type', file.endsWith('.js') ? 'text/javascript' : 'text/html; charset=utf-8');
    res.end(await readFile(resolve(root, file)));
  } catch { res.writeHead(500).end(); }
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
let browser;
try {
  browser = await chromium.launch({ executablePath: process.env.CHROMIUM_PATH || '/usr/bin/chromium', headless: true });
  for (const width of [320, 390]) {
    const page = await browser.newPage({ viewport: { width, height: 740 }, isMobile: true, hasTouch: true });
    await page.addInitScript(() => {
      window.commands = [];
      window.ksu = { exec(command, options, callback) {
        window.commands.push(command);
        window[callback](window.commands.length === 1 ? 1 : 0, '', '');
      } };
    });
    await page.goto('http://127.0.0.1:' + server.address().port);
    const base = '/system/bin/sh /data/adb/modules/ssmcp/';
    assert.deepEqual(await page.evaluate(() => window.commands), [base + 'open.sh']);
    assert.match(await page.locator('#status').textContent(), /失败/);
    assert.equal(await page.locator('textarea, pre, iframe').count(), 0);
    await page.locator('#open').tap();
    assert.match(await page.locator('#status').textContent(), /已在系统浏览器/);
    await page.locator('#setup').tap();
    assert.deepEqual(await page.evaluate(() => window.commands), [base + 'open.sh', base + 'open.sh', base + 'onboarding/open.sh']);
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
    await page.close();
  }
  const fallback = await browser.newPage();
  await fallback.goto('http://127.0.0.1:' + server.address().port);
  assert.match(await fallback.locator('#status').textContent(), /操作/);
  const onboarding = await browser.newPage({ viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true });
  const token = '9a'.repeat(32), base = 'http://127.0.0.1:42387', writes = [];
  const files = {
    '/setup': resolve(root, '../onboarding/index.html'),
    '/setup.js': resolve(root, '../onboarding/setup.js'),
    '/console.css': resolve(root, 'console.css'), '/device-api.js': resolve(root, 'device-api.js'),
    '/src/gateway.mjs': resolve('apps/ssmcp-web/src/gateway.mjs'), '/src/protocol.mjs': resolve('apps/ssmcp-web/src/protocol.mjs'),
  };
  await onboarding.route(base + '/**', async route => {
    const request = route.request(), path = new URL(request.url()).pathname;
    if (request.method() === 'POST') {
      assert.equal(request.headers().authorization, 'Bearer ' + token);
      if (path === '/terminal') return route.fulfill({json: {target: null, configured: null}});
      assert.equal(path, '/setup'); writes.push(request.postDataJSON());
      return route.fulfill({json: {ok: true}});
    }
    if (path === '/') return route.fulfill({body: '<html lang="zh-CN"></html>', contentType: 'text/html'});
    assert.ok(files[path], path);
    return route.fulfill({body: await readFile(files[path]), contentType: path === '/setup' ? 'text/html; charset=utf-8'
      : path.endsWith('.css') ? 'text/css' : 'application/javascript'});
  });
  await onboarding.goto(base + '/setup#' + token);
  await onboarding.locator('#setup').waitFor();
  assert.equal(new URL(onboarding.url()).hash, '');
  assert.equal(await onboarding.locator('#server').inputValue(), 'ssmcp.lmm.best');
  assert.equal(await onboarding.locator('a').filter({hasText:'GitHub'}).count(), 1);
  assert.equal(await onboarding.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
  if (process.env.WEB_TEST_EVIDENCE) await onboarding.screenshot({ path: resolve(process.env.WEB_TEST_EVIDENCE, 'magisk-first-run.png') });
  await onboarding.locator('#name').fill('my-user'); await onboarding.locator('#root').check();
  await onboarding.locator('#connect').tap();
  await onboarding.getByText('请先阅读并同意用户协议。', {exact:true}).waitFor();
  assert.deepEqual(writes, []);
  await onboarding.locator('#server').fill('relay.example');
  await onboarding.locator('#connect').tap();
  await onboarding.waitForURL(base + '/#' + token);
  assert.deepEqual(writes, [{target:'my-user@relay.example',root:true,terms:false,privacy:false}]);
  await onboarding.close();
  console.log('Magisk launcher passed at 320px and 390px, including retry, setup and fallback.');
} finally {
  if (browser) await browser.close();
  await new Promise(resolve => server.close(resolve));
}
