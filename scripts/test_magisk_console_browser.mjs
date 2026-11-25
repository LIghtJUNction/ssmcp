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
    if (!['index.html', 'console.js', 'console.css', 'device-api.js', 'module-entry.js', 'terminal-frame.js'].includes(name)) { res.writeHead(404).end(); return; }
    res.setHeader('Content-Type', name.endsWith('.js') ? 'text/javascript' : name.endsWith('.css') ? 'text/css' : 'text/html');
    res.end(await readFile(resolve(root, name)));
  } catch { res.writeHead(500).end(); }
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
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
          if (body.action === 'screen') {
            const first = window.expanded ? 'expanded' : 'collapsed';
            const second = window.scrolled ? 'older rows' : 'latest rows';
            const cursor = { visible: true, row: 2, col: 3 };
            if (transport === 'loopback') return { screen: `${first}\n${second}`, cursor };
            const revision = 1 + Number(window.expanded) + Number(window.scrolled);
            if (body.since === revision) return { unchanged: true, revision, cursor };
            return { rows: [[[first.length, first, { flags: 0 }]], [[second.length, second, { flags: 0 }]],
              [[1, 'A', { fg: 42, flags: 0 }], [2, '中', { fg: 0x01010203, flags: 0 }]]], revision, cursor };
          }
          return {};
        };
        window.replyToTerminal = async body => {
          const json = window.handleTerminal(body);
          if (body.action === 'input' && window.blockInput) {
            window.blockInput = false;
            const status = await new Promise(resolve => { window.releaseInput = resolve; });
            return { status, json: status === 200 ? json : { error: 'No receipt' } };
          }
          return { status: 200, json };
        };
        if (transport === 'manager') window.ksu = { exec(command, options, callback) {
          const bytes = Uint8Array.from(atob(command.split(' ').at(-1)), char => char.charCodeAt(0));
          const body = JSON.parse(new TextDecoder().decode(bytes));
          window.replyToTerminal(body).then(({ status, json }) => {
            setTimeout(() => window[callback](0, status + '\n' + JSON.stringify(json)), 0);
          });
        } };
      }, { transport });
      const page = await context.newPage();
      if (transport === 'loopback') await page.route('**/terminal', async route => {
        const body = route.request().postDataJSON();
        const reply = await page.evaluate(body => window.replyToTerminal(body), body);
        await route.fulfill(reply);
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
      if (transport === 'manager') {
        await page.waitForFunction(() => document.querySelectorAll('.terminal-row').length === 3);
        const reading = await page.evaluate(() => {
          const glyphs = document.querySelectorAll('.terminal-row:last-child span');
          return [getComputedStyle(glyphs[0]).color, getComputedStyle(glyphs[1]).color,
            Math.abs(glyphs[1].getBoundingClientRect().right - document.getElementById('terminal-cursor').getBoundingClientRect().left)];
        });
        assert.deepEqual(reading.slice(0, 2), ['rgb(0, 215, 135)', 'rgb(1, 2, 3)']);
        assert.ok(reading[2] <= 1, 'cursor starts immediately after the two-column glyph');
      }
      await page.touchscreen.tap(cell.x, cell.y);
      await page.waitForFunction(() => window.calls.some(call => call.text === '\x1b[<0;5;4M\x1b[<0;5;4m'));
      await page.waitForFunction(() => document.getElementById('screen').textContent.includes('expanded'));
      assert.notEqual(await page.evaluate(() => document.activeElement.id), 'direct-input', 'card tap keeps keyboard closed');
      await page.locator('#open-keyboard').tap();
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
      await page.waitForFunction(() => window.calls.some(call => call.text?.includes('\x1b[<65;5;4M')));
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
      await page.locator('#open-keyboard').tap();
      await page.evaluate(() => {
        window.calls.length = 0;
        const input = document.getElementById('direct-input');
        input.dispatchEvent(new CompositionEvent('compositionstart'));
        input.value = '\u200b中';
        input.dispatchEvent(new InputEvent('input', { isComposing: true }));
        input.dispatchEvent(new CompositionEvent('compositionend', { data: '中' }));
        input.dispatchEvent(new InputEvent('input', { data: '中' }));
      });
      await page.waitForFunction(() => window.calls.some(call => call.text === '中'));
      assert.equal(await page.evaluate(() => window.calls.filter(call => call.action === 'input').length), 1);
      await page.evaluate(() => {
        const input = document.getElementById('direct-input');
        window.blockInput = true;
        input.value = '\u200b' + 'a'.repeat(4095) + '🟠尾';
        input.dispatchEvent(new InputEvent('input', { inputType: 'insertFromPaste' }));
        input.value = '\u200b追加';
        input.dispatchEvent(new InputEvent('input'));
      });
      await page.waitForFunction(() => typeof window.releaseInput === 'function');
      await page.locator('[data-tab="0"]').tap();
      await page.evaluate(() => window.releaseInput(503));
      await page.locator('[data-tab="1"]').tap();
      await page.locator('#input-recovery').waitFor();
      assert.equal(await page.locator('#uncertain-input').inputValue(), 'a'.repeat(4095));
      assert.equal(await page.locator('#unsent-input').inputValue(), '🟠尾追加');
      assert.equal(await page.evaluate(() => window.calls.filter(call => call.text?.startsWith('a'.repeat(100))).length), 1);
      await page.locator('#dismiss-draft').tap();
      assert.equal(await page.locator('#direct-input').evaluate(node => node.readOnly), false);
      console.log(`PASS ${transport} ${width}px: tap coordinates, expansion fixture, native touch swipe, pinch, tab switching`);
      await context.close();
    }
  }
} finally { await browser.close(); await new Promise(resolve => server.close(resolve)); }
