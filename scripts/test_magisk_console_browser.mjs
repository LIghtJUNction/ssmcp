// Real module WebUI entry + isolated transport fixture; no Android device required.
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
const { chromium } = await import(process.env.PLAYWRIGHT_MODULE || 'playwright');
const root = resolve('packaging/magisk/src/ssmcp/webroot');
const server = createServer(async (req, res) => {
  try {
    const name = new URL(req.url, 'http://localhost').pathname.slice(1) || 'index.html';
    if (!['index.html', 'console.js', 'console.css', 'device-api.js', 'module-entry.js'].includes(name)) { res.writeHead(404).end(); return; }
    res.setHeader('Content-Type', name.endsWith('.js') ? 'text/javascript' : name.endsWith('.css') ? 'text/css' : 'text/html');
    res.end(await readFile(resolve(root, name)));
  } catch { res.writeHead(500).end(); }
});
await new Promise(resolve => server.listen(0, '0.0.0.0', resolve));
const browser = await chromium.launch({ executablePath: process.env.CHROMIUM_PATH || '/usr/bin/chromium', headless: true, args: ['--no-sandbox'] });
try {
  for (const transport of ['manager', 'loopback']) {
    for (const width of [320, 390]) {
      const context = await browser.newContext({ viewport: { width, height: 780 }, isMobile: true, hasTouch: true });
      context.setDefaultTimeout(8000);
      await context.addInitScript(({ transport }) => {
        window.calls = [];
        window.expanded = false;
        window.scrolled = false;
        window.handleTerminal = body => {
          window.calls.push(body);
          if (body.action === 'list') return { target: 'fixture@localhost', tabs: [0, 1] };
          if (body.action === 'input') {
            if (body.text.startsWith('\x1b[<0;')) window.expanded = true;
            if (body.text.startsWith('\x1b[<65;')) window.scrolled = true;
          }
          if (body.action === 'screen') return { screen: `${window.expanded ? 'expanded' : 'collapsed'}\n${window.scrolled ? 'older rows' : 'latest rows'}`, cursor: { visible: true, row: 2, col: 1 } };
          return {};
        };
        if (transport === 'manager') window.ksu = { exec(command, options, callback) {
          const body = JSON.parse(atob(command.split(' ').at(-1)));
          setTimeout(() => window[callback](0, '200\n' + JSON.stringify(window.handleTerminal(body))), 0);
        } };
      }, { transport });
      const page = await context.newPage();
      if (transport === 'loopback') await page.route('**/terminal', async route => {
        const body = route.request().postDataJSON();
        const reply = await page.evaluate(body => window.handleTerminal(body), body);
        await route.fulfill({ json: reply });
      });
      const hostname = transport === 'manager' ? 'localhost' : '127.0.0.1';
      await page.goto(`http://${hostname}:${server.address().port}/index.html${transport === 'loopback' ? '#' + 'a'.repeat(64) : ''}`);
      await page.waitForFunction(() => window.calls.some(call => call.action === 'resize'));
      const screen = page.locator('#screen');
      const cell = await page.evaluate(() => {
        const node = document.getElementById('screen');
        const rect = node.getBoundingClientRect(), style = getComputedStyle(node), parent = getComputedStyle(node.parentElement);
        return { x: rect.left + parseFloat(style.paddingLeft) + 4.5 * parseFloat(parent.getPropertyValue('--cell-width')),
          y: rect.top + parseFloat(style.paddingTop) + 3.5 * parseFloat(parent.getPropertyValue('--cell-height')) };
      });
      await page.touchscreen.tap(cell.x, cell.y);
      await page.waitForFunction(() => window.calls.some(call => call.text === '\x1b[<0;5;4m'));
      await page.waitForFunction(() => document.getElementById('screen').textContent.includes('expanded'));
      await page.keyboard.type('a');
      await page.waitForFunction(() => window.calls.some(call => call.text === 'a'));
      await page.locator('#direct-input').evaluate(node => node.blur());
      await page.evaluate(() => { window.calls.length = 0; });
      const cdp = await context.newCDPSession(page);
      await cdp.send('Input.dispatchTouchEvent', { type: 'touchStart', touchPoints: [{ x: cell.x, y: cell.y + 120 }] });
      for (const offset of [100, 80, 60, 40, 20, 0]) {
        await cdp.send('Input.dispatchTouchEvent', { type: 'touchMove', touchPoints: [{ x: cell.x, y: cell.y + offset }] });
      }
      await cdp.send('Input.dispatchTouchEvent', { type: 'touchEnd', touchPoints: [] });
      await page.waitForFunction(() => window.calls.some(call => call.text?.startsWith('\x1b[<65;')));
      await page.waitForFunction(() => document.getElementById('screen').textContent.includes('older rows'));
      assert.equal(await page.evaluate(() => document.activeElement.id), '', 'swipe must not open keyboard');
      assert.equal(await page.evaluate(() => window.calls.some(call => call.text?.startsWith('\x1b[<0;'))), false, 'swipe must not click a card');
      await page.evaluate(() => { window.calls.length = 0; });
      await cdp.send('Input.dispatchTouchEvent', { type: 'touchStart', touchPoints: [{ x: cell.x, y: cell.y }] });
      await cdp.send('Input.dispatchTouchEvent', { type: 'touchMove', touchPoints: [{ x: cell.x, y: cell.y + 80 }] });
      await cdp.send('Input.dispatchTouchEvent', { type: 'touchEnd', touchPoints: [] });
      await page.waitForFunction(() => window.calls.some(call => call.text?.startsWith('\x1b[<64;')));
      await page.mouse.move(cell.x, cell.y);
      await page.mouse.wheel(0, 80);
      await page.waitForFunction(() => window.calls.some(call => call.text === '\x1b[<65;5;4M'));
      // Pinch continues to adjust the font without sending clicks or wheel input.
      await page.evaluate(() => { window.calls.length = 0; });
      const before = await screen.evaluate(node => parseFloat(getComputedStyle(node).fontSize));
      const touches = distance => [{ x: width / 2 - distance / 2, y: 250 }, { x: width / 2 + distance / 2, y: 250 }];
      await cdp.send('Input.dispatchTouchEvent', { type: 'touchStart', touchPoints: touches(100) });
      await cdp.send('Input.dispatchTouchEvent', { type: 'touchMove', touchPoints: touches(70) });
      await cdp.send('Input.dispatchTouchEvent', { type: 'touchEnd', touchPoints: [] });
      await page.waitForFunction(before => parseFloat(getComputedStyle(document.getElementById('screen')).fontSize) < before, before);
      assert.equal(await page.evaluate(() => window.calls.some(call => call.action === 'input')), false);
      await page.locator('[data-tab="1"]').tap();
      await page.waitForFunction(() => window.calls.some(call => call.action === 'screen' && call.tab === 1));
      console.log(`PASS ${transport} ${width}px: tap coordinates, expansion fixture, native touch swipe, pinch, tab switching`);
      await context.close();
    }
  }
} finally { await browser.close(); await new Promise(resolve => server.close(resolve)); }
