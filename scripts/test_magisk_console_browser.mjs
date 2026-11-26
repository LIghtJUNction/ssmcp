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
  console.log('Magisk launcher passed at 320px and 390px, including retry, setup and fallback.');
} finally {
  if (browser) await browser.close();
  await new Promise(resolve => server.close(resolve));
}
