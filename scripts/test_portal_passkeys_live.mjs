// Launched by cargo test --lib chromium_passkey_signs_back_into_the_same_github_account -- --ignored.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { once } from 'node:events';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import http from 'node:http';
import https from 'node:https';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const { chromium } = createRequire(import.meta.url)(process.env.PLAYWRIGHT_MODULE || 'playwright');
const loopback = new URL(process.env.PASSKEY_TEST_LOOPBACK);
assert.equal(loopback.hostname, '127.0.0.1');
assert.equal(loopback.protocol, 'http:');
const origin = process.env.PASSKEY_TEST_ORIGIN;
assert.equal(new URL(origin).hostname, 'localhost');
assert.equal(new URL(origin).protocol, 'https:');
const cookieName = '__Host-ssmcp-portal';
const initial = process.env.PASSKEY_TEST_COOKIE;
assert.ok(initial);
const directory = await mkdtemp(join(tmpdir(), 'ssmcp-passkey-tls-'));
const key = join(directory, 'key.pem'), cert = join(directory, 'cert.pem');
const calls = [];
let proxy, browser;
try {
  execFileSync('openssl', ['req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-days', '1',
    '-subj', '/CN=localhost', '-keyout', key, '-out', cert], { stdio: 'ignore' });
  // The proxy terminates fixture TLS. All account HTML, cookies, redirects,
  // CSP and WebAuthn API responses come from the unmodified local Axum router.
  proxy = https.createServer({ key: await readFile(key), cert: await readFile(cert) }, (req, res) => {
    const upstream = http.request(new URL(req.url, loopback), { method: req.method, headers: req.headers }, response => {
      if (req.url.startsWith('/account/passkeys/')) calls.push([req.url, response.statusCode]);
      res.writeHead(response.statusCode, response.headers);
      response.pipe(res);
    });
    upstream.on('error', () => { res.writeHead(502); res.end(); });
    req.pipe(upstream);
  });
  // Rust reserved this loopback port before configuring the relying party.
  proxy.listen({ fd: 0 });
  await once(proxy, 'listening');
  browser = await chromium.launch({ executablePath: process.env.CHROMIUM_PATH, headless: true });
  const context = await browser.newContext({ ignoreHTTPSErrors: true });
  if (process.env.PASSKEY_TEST_FALLBACK === '1') {
    await context.addInitScript(() => {
      for (const name of ['parseCreationOptionsFromJSON', 'parseRequestOptionsFromJSON']) {
        Object.defineProperty(PublicKeyCredential, name, { configurable: true, value: undefined });
      }
      Object.defineProperty(PublicKeyCredential.prototype, 'toJSON', { configurable: true, value: undefined });
    });
  }
  await context.addCookies([{ name: cookieName, value: initial, url: origin, httpOnly: true, secure: true, sameSite: 'Lax' }]);
  const page = await context.newPage();
  page.setDefaultTimeout(8000);
  const cdp = await context.newCDPSession(page);
  await cdp.send('WebAuthn.enable');
  await cdp.send('WebAuthn.addVirtualAuthenticator', { options: {
    protocol: 'ctap2', transport: 'internal', hasResidentKey: true, hasUserVerification: true,
    isUserVerified: true, automaticPresenceSimulation: true,
  } });
  await page.goto(origin);
  const address = await page.locator('#account .identity-facts dd').innerText();
  assert.ok(address.endsWith('@' + new URL(origin).host));
  const bind = page.getByRole('button', { name: '绑定通行密钥' });
  const login = page.getByRole('button', { name: '使用通行密钥登录' });
  assert.equal(await login.count(), 0);
  async function waitStatus(text) {
    try { await page.getByRole('status').filter({ hasText: text }).waitFor(); }
    catch (error) {
      throw new Error('Expected ' + text + ', got ' + (await page.locator('#passkey-status').textContent()) +
        '; HTTP ' + JSON.stringify(calls), { cause: error });
    }
  }
  await bind.click();
  await waitStatus('下次可用它登录此账户');
  await page.getByRole('button', { name: '退出登录' }).click();
  await login.waitFor();
  assert.equal(await bind.count(), 0);
  await context.clearCookies();
  await page.goto(origin);
  const portalCookie = async () => (await context.cookies(origin)).find(item => item.name === cookieName);
  const guest = await portalCookie();
  assert.ok(guest && guest.value !== initial);
  assert.ok(await page.getByRole('button', { name: '使用 GitHub 注册或登录' }).isVisible());
  await login.click();
  await waitStatus('已使用通行密钥登录');
  assert.equal(await page.locator('#account .identity-facts dd').innerText(), address);
  const signed = await portalCookie();
  assert.ok(signed && signed.secure && signed.httpOnly && signed.sameSite === 'Lax');
  assert.ok(signed.value !== initial && signed.value !== guest.value);
  for (const [i, suffix] of ['register/start', 'register/finish', 'login/start', 'login/finish'].entries()) {
    assert.equal(calls[i]?.[0], '/account/passkeys/' + suffix);
    assert.ok(calls[i][1] >= 200 && calls[i][1] < 300);
  }
  assert.equal(calls.length, 4);
  console.log('PASS real portal renderer, bound virtual device, logout, fresh anonymous session, signed assertion, original GitHub account and rotated cookie');
} finally {
  if (browser) await browser.close();
  if (proxy) { proxy.closeAllConnections(); proxy.close(); }
  await rm(directory, { recursive: true, force: true });
}
