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
  const context = await browser.newContext({ viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true });
  const deviceToken = '9a'.repeat(32), guiToken = '8b'.repeat(32);
  const base = 'http://127.0.0.1:42387', setupUrl = base + '/setup';
  const calls = [], writes = [], unexpected = [];
  let state = { target: null, configured: null, mcp: null };
  let conflict = false;
  const files = {
    '/setup': resolve(root, '../onboarding/index.html'),
    '/setup.js': resolve(root, '../onboarding/setup.js'),
    '/console.css': resolve(assets, 'console.css'), '/device-api.js': resolve(assets, 'device-api.js'),
  };
  await context.route('**/*', async route => {
    const request = route.request(), url = new URL(request.url()), path = url.pathname;
    if (url.origin === base && request.method() === 'POST' && ['/setup', '/terminal'].includes(path)) {
      const body = request.postDataJSON(), authorization = request.headers().authorization;
      calls.push({ path, body, authorization });
      if (authorization !== 'Bearer ' + deviceToken) return route.fulfill({status: 401, json: {error: 'unauthorized'}});
      if (path === '/terminal' && body.action === 'list' && body.tab === 0) return route.fulfill({json: state});
      if (path === '/setup') {
        writes.push(body);
        state = {target: conflict ? 'existing@relay.example' : body.target, mcp: 'https://relay.example/mcp', configured: null};
        return conflict ? route.fulfill({status: 409, json: {error: 'already configured'}})
          : route.fulfill({json: {ok: true}});
      }
    }
    if (url.origin === base && request.method() === 'GET' && Object.hasOwn(files, path)) {
      return route.fulfill({body: await readFile(files[path]), contentType: path === '/setup' ? 'text/html; charset=utf-8'
        : path.endsWith('.css') ? 'text/css' : 'application/javascript'});
    }
    unexpected.push(request.method() + ' ' + url.origin + path);
    return route.abort();
  });
  async function openSetup(token) {
    const page = await context.newPage();
    page.setDefaultTimeout(5000);
    await page.goto(setupUrl + (token ? '#' + token : ''));
    return page;
  }
  async function expectComplete(page, target) {
    await page.locator('#complete').waitFor();
    await page.getByText('请从 Magisk 模块操作打开工作台。', {exact: true}).waitFor();
    assert.equal(await page.locator('#complete-target').textContent(), target);
    assert.equal(await page.locator('#setup').isHidden(), true);
    assert.equal(page.url(), setupUrl);
    assert.deepEqual(await page.evaluate(() => [localStorage.getItem('ssmcp-console'), sessionStorage.getItem('ssmcp-console')]), [null, null]);
    assert.equal(await page.locator('a[href*="#' + deviceToken + '"]').count(), 0);
  }
  async function expectBare(page) {
    await page.getByText('请在设备上运行模块的 onboarding/open.sh 重新打开设备设置。', {exact: true}).waitFor();
    assert.equal(await page.locator('#setup').isHidden(), true);
    assert.equal(await page.locator('#complete').isHidden(), true);
    // A synthetic click also goes through the production view-state guard.
    await page.locator('#connect').evaluate(button => button.click());
    assert.equal(await page.evaluate(async () => {
      const { api } = await import('/device-api.js');
      try { await api('/setup', {}); return true; } catch { return false; }
    }), false, 'the bearer-less API client also blocks direct calls');
  }

  const naked = await openSetup();
  await expectBare(naked);
  assert.deepEqual(calls, []);
  await naked.close();

  const wrongScope = await openSetup(guiToken);
  await wrongScope.locator('#status').filter({hasText: '授权已失效'}).waitFor();
  await wrongScope.locator('#connect').evaluate(button => button.click());
  assert.equal(await wrongScope.locator('#setup').isHidden(), true);
  assert.deepEqual(calls.map(call => [call.path, call.authorization]), [['/terminal', 'Bearer ' + guiToken]]);
  assert.deepEqual(writes, []);
  await wrongScope.close();

  const onboarding = await openSetup(deviceToken);
  await onboarding.locator('#setup').waitFor();
  assert.equal(onboarding.url(), setupUrl);
  const beforeGui = calls.length;
  assert.equal(await onboarding.evaluate(async () => {
    const { api } = await import('/device-api.js');
    try { await api('/api/gui/v1/register', {}); return true; } catch { return false; }
  }), false, 'a device credential cannot call a GUI endpoint');
  assert.equal(calls.length, beforeGui);
  assert.equal(await onboarding.locator('#server').inputValue(), 'ssmcp.lmm.best');
  assert.equal(await onboarding.locator('a').filter({hasText:'GitHub'}).count(), 1);
  assert.equal(await onboarding.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
  for (const theme of ['light', 'dark']) {
    await onboarding.emulateMedia({colorScheme:theme});
    const ink = await onboarding.locator('html').evaluate(el => getComputedStyle(el).color);
    assert.equal(ink, theme === 'light' ? 'rgb(25, 25, 25)' : 'rgb(240, 240, 240)');
    assert.equal(await onboarding.locator('#root').evaluate(el => getComputedStyle(el).accentColor), ink);
    if (process.env.WEB_TEST_EVIDENCE) await onboarding.screenshot({path:resolve(process.env.WEB_TEST_EVIDENCE, 'magisk-onboarding-' + theme + '.png')});
  }
  await onboarding.emulateMedia({colorScheme:'light'});
  if (process.env.WEB_TEST_EVIDENCE) await onboarding.screenshot({ path: resolve(process.env.WEB_TEST_EVIDENCE, 'magisk-first-run.png') });
  await onboarding.locator('#name').fill('my-user');
  await onboarding.locator('#connect').tap();
  await onboarding.locator('#status').filter({hasText:'Confirm that this is an account you want to allow.'}).waitFor();
  await onboarding.locator('#account-confirmed').check();
  await onboarding.locator('#connect').tap();
  await onboarding.locator('#status').filter({hasText:'Confirm the Android root access.'}).waitFor();
  await onboarding.locator('#root').check();
  await onboarding.locator('#connect').tap();
  await onboarding.getByText('请先阅读并同意用户协议。', {exact:true}).waitFor();
  assert.deepEqual(writes, []);
  await onboarding.locator('#server').fill('relay.example');
  assert.equal(await onboarding.locator('#register-account').getAttribute('href'), 'https://relay.example/#account');
  assert.equal(await onboarding.locator('#account-confirmed').isChecked(), false);
  assert.equal(await onboarding.locator('#root').isChecked(), false);
  await onboarding.locator('#account-confirmed').check();
  await onboarding.locator('#root').check();
  await onboarding.locator('#connect').tap();
  await expectComplete(onboarding, 'my-user@relay.example');
  assert.deepEqual(writes, [{target:'my-user@relay.example',account_confirmed:true,root:true,terms:false,privacy:false}]);

  await onboarding.locator('#complete-mcp').filter({hasText: 'https://relay.example/mcp'}).waitFor();
  // An upgraded tab may still contain the previous release's storage entry.
  await onboarding.evaluate(token => sessionStorage.setItem('ssmcp-console', token), deviceToken);
  const beforeReload = calls.length;
  await onboarding.reload();
  await expectBare(onboarding);
  assert.equal(await onboarding.evaluate(() => sessionStorage.getItem('ssmcp-console')), null);
  assert.equal(calls.length, beforeReload, 'reloading the clean URL must not reuse the device bearer');
  await onboarding.close();

  const existing = await openSetup(deviceToken);
  await expectComplete(existing, 'my-user@relay.example');
  assert.equal(writes.length, 1, 'already configured devices require no setup write');
  await existing.close();

  state = {target: null, configured: 'prefilled@relay.example', mcp: null};
  conflict = true;
  const competing = await openSetup(deviceToken);
  await competing.locator('#setup').waitFor();
  assert.equal(await competing.locator('#name').inputValue(), 'prefilled');
  assert.equal(await competing.locator('#server').inputValue(), 'relay.example');
  await competing.locator('#account-confirmed').check();
  await competing.locator('#root').check(); await competing.locator('#connect').tap();
  await expectComplete(competing, 'existing@relay.example');
  assert.equal(writes.length, 2, 'a conflict reads the effective account without repeating the write');
  assert.deepEqual(unexpected, [], 'onboarding never navigates to the workbench or calls a GUI endpoint');
  await context.close();
  console.log('Magisk manager entry and scoped device onboarding passed at mobile widths.');
} finally {
  await browser.close();
}
