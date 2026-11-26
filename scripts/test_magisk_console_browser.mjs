// Exercise the packaged manager entry in mobile Chromium without a manager bridge.
import assert from 'node:assert/strict';
import { readFile, readdir } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { resolve } from 'node:path';

const specifier = process.env.PLAYWRIGHT_MODULE || 'playwright';
const { default: { chromium } } = await import(createRequire(import.meta.url).resolve(specifier));
const root = resolve('packaging/magisk/src/ssmcp/webroot');
const assets = resolve('packaging/magisk/assets');
const entry = 'http://module.test/index.html';
const destination = 'http://127.0.0.1:42387/';
assert.deepEqual(await readdir(root), ['index.html'], 'the manager WebUI contains a single file');
const html = await readFile(resolve(root, 'index.html'), 'utf8');

const browser = await chromium.launch({ executablePath: process.env.CHROMIUM_PATH, headless: true });
try {
  for (const width of [320, 390]) {
    const page = await browser.newPage({ viewport: { width, height: 740 }, isMobile: true,
      hasTouch: true, javaScriptEnabled: false });
    const unexpected = [];
    await page.route('**/*', route => {
      const url = route.request().url();
      if (url === entry || url === entry + '?manual') {
        // A sandbox without allow-scripts suppresses automatic refresh, so the fallback can be tapped.
        const headers = url === entry ? {} : { 'Content-Security-Policy': 'sandbox' };
        return route.fulfill({ body: html, contentType: 'text/html; charset=utf-8', headers });
      }
      if (url === destination) return route.fulfill({ body: 'Workbench', contentType: 'text/plain' });
      unexpected.push(url);
      return route.abort();
    });
    await page.goto(entry, { waitUntil: 'commit' });
    await page.waitForURL(destination, { timeout: 5000 });
    await page.goto(entry + '?manual');
    const link = page.getByRole('link', { name: /\S/ });
    assert.equal(await link.getAttribute('href'), destination);
    assert.equal(await page.locator('script').count(), 0);
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
    await link.tap();
    await page.waitForURL(destination, { timeout: 5000 });
    assert.deepEqual(unexpected, [], 'the manager entry requests only its destination');
    await page.close();
  }
  const onboarding = await browser.newPage({ viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true });
  const token = '9a'.repeat(32), base = 'http://127.0.0.1:42387', writes = [];
  const files = {
    '/setup': resolve(root, '../onboarding/index.html'),
    '/setup.js': resolve(root, '../onboarding/setup.js'),
    '/console.css': resolve(assets, 'console.css'), '/device-api.js': resolve(assets, 'device-api.js'),
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
  console.log('Magisk WebUI redirects and opens its fallback at 320px and 390px. First-run onboarding passed.');
} finally {
  await browser.close();
}
