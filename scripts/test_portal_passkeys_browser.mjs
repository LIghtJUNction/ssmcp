// Run from the repository root. All browser keys and account data are synthetic.
import assert from 'node:assert/strict';
import { createHash, createPublicKey, randomBytes, verify } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import { createRequire } from 'node:module';

const { chromium } = createRequire(import.meta.url)(process.env.PLAYWRIGHT_MODULE || 'playwright');
const js = await readFile(new URL('../src/portal/portal.js', import.meta.url));
const origin = 'https://portal.test';
const rp = 'portal.test';
const csp = "default-src 'none'; script-src 'self'; connect-src 'self'; form-action 'self'";
const asset = '/account/assets/' + createHash('sha256').update(js).digest('hex') + '/portal.js';
const decode = value => Buffer.from(value, 'base64url');
const hash = value => createHash('sha256').update(value).digest();

function pageHtml(mode, csrf) {
  const action = mode === 'login' ? '使用通行密钥登录' : '绑定通行密钥';
  return '<!doctype html><html lang="zh"><meta charset="utf-8"><title>Test</title><main id="account">' +
    '<button type="button" disabled data-passkey="' + mode + '" data-csrf="' + csrf + '">' + action + '</button>' +
    (mode === 'login' ? '<form action="/account/login" method="post"><button>使用 GitHub 注册或登录</button></form>' : '') +
    '<p role="status" id="passkey-status"></p><script src="' + asset + '" defer></script></main></html>';
}

async function ceremony(browser, fallback) {
  const context = await browser.newContext();
  try {
    if (fallback) {
      await context.addInitScript(() => {
        for (const name of ['parseCreationOptionsFromJSON', 'parseRequestOptionsFromJSON']) {
          Object.defineProperty(PublicKeyCredential, name, { configurable: true, value: undefined });
        }
        Object.defineProperty(PublicKeyCredential.prototype, 'toJSON', { configurable: true, value: undefined });
      });
    }
    const page = await context.newPage();
    const devtools = await context.newCDPSession(page);
    await devtools.send('WebAuthn.enable');
    await devtools.send('WebAuthn.addVirtualAuthenticator', { options: { protocol: 'ctap2', transport: 'internal',
      hasResidentKey: true, hasUserVerification: true, isUserVerified: true, automaticPresenceSimulation: true } });
    const user = randomBytes(16).toString('base64url');
    const csrf = { register: randomBytes(16).toString('hex'), login: randomBytes(16).toString('hex') };
    const calls = [];
    const errors = [];
    let challenge, publicKey, credentialId;
    let response = 'normal';
    await context.route('**/*', async route => {
      const req = route.request();
      const url = new URL(req.url());
      try {
        assert.equal(url.origin, origin);
        const mode = (req.headers().cookie || '').includes('signed=1') ? 'register' : 'login';
        if (url.pathname === '/') {
          return await route.fulfill({ contentType: 'text/html; charset=utf-8',
            headers: { 'Content-Security-Policy': csp }, body: pageHtml(mode, csrf[mode]) });
        }
        if (url.pathname === asset) return await route.fulfill({ contentType: 'text/javascript', body: js });
        const match = /^\/account\/passkeys\/(register|login)\/(start|finish)$/.exec(url.pathname);
        assert.ok(match, 'unexpected fixture path ' + url.pathname);
        assert.equal(req.method(), 'POST');
        assert.equal(match[1], mode);
        assert.equal(req.headers()['content-type'], 'application/json');
        const body = req.postDataJSON();
        assert.equal(body.csrf, csrf[mode]);
        calls.push(mode + '/' + match[2]);
        if (match[2] === 'start') {
          challenge = randomBytes(32).toString('base64url');
          const options = mode === 'register' ? {
            challenge, rp: { id: rp, name: 'SSMCP fixture' }, user: { id: user, name: 'fixture', displayName: 'Fixture' },
            pubKeyCredParams: [{ type: 'public-key', alg: -7 }], attestation: 'none', excludeCredentials: [],
            authenticatorSelection: { residentKey: 'required', userVerification: 'required' },
          } : { challenge, rpId: rp, allowCredentials: [], userVerification: 'required' };
          return await route.fulfill({ json: { publicKey: options } });
        }
        const key = body.credential;
        assert.equal(key.type, 'public-key');
        assert.equal(key.rawId, key.id);
        assert.deepEqual(key.clientExtensionResults, {});
        const data = decode(key.response.clientDataJSON);
        const client = JSON.parse(data);
        assert.equal(client.origin, origin);
        assert.equal(client.challenge, challenge);
        assert.equal(client.type, mode === 'register' ? 'webauthn.create' : 'webauthn.get');
        const authData = decode(key.response.authenticatorData);
        assert.deepEqual(authData.subarray(0, 32), hash(rp));
        assert.equal(authData[32] & 0x05, 0x05, 'user presence and verification');
        if (mode === 'register') {
          assert.ok(decode(key.response.attestationObject).length);
          publicKey = createPublicKey({ key: decode(key.response.publicKey), type: 'spki', format: 'der' });
          credentialId = key.id;
        } else {
          assert.equal(key.id, credentialId);
          assert.equal(key.response.userHandle, user);
          assert.ok(verify('sha256', Buffer.concat([authData, hash(data)]), publicKey, decode(key.response.signature)));
        }
        if (response === 'lost') return await route.abort('failed');
        if (response === 'expired') return await route.fulfill({ status: 410,
          json: { error: { message: '这次通行密钥验证已结束，请重新开始。' } } });
        return await route.fulfill({ headers: { 'Set-Cookie': 'signed=1; Path=/; Secure; HttpOnly; SameSite=Strict' },
          json: { redirect: '/#account' } });
      } catch (error) {
        errors.push(error);
        await route.fulfill({ status: 400, json: {} });
      }
    });
    async function expectStatus(text) {
      try { await page.getByRole('status').filter({ hasText: text }).waitFor({ timeout: 7000 }); }
      catch (error) { throw errors[0] || new Error('Expected ' + text + ', got ' + await page.locator('#passkey-status').textContent(), { cause: error }); }
    }
    await context.addCookies([{ name: 'signed', value: '1', url: origin, httpOnly: true, secure: true }]);
    await page.goto(origin);
    if (!fallback) {
      const hasNative = await page.evaluate(() => [PublicKeyCredential.parseCreationOptionsFromJSON,
        PublicKeyCredential.parseRequestOptionsFromJSON, PublicKeyCredential.prototype.toJSON].every(fn => typeof fn === 'function'));
      console.log('Native WebAuthn JSON available:', hasNative);
    }
    const bind = page.getByRole('button', { name: '绑定通行密钥' });
    const login = page.getByRole('button', { name: '使用通行密钥登录' });
    await bind.click();
    await expectStatus('下次可用它登录此账户');
    await context.clearCookies();
    await page.goto(origin);
    await login.click();
    await expectStatus('已使用通行密钥登录');
    assert.ok(await bind.isVisible());
    assert.deepEqual(calls, ['register/start', 'register/finish', 'login/start', 'login/finish']);
    await context.clearCookies();
    await page.goto(origin);
    await page.evaluate(() => Object.defineProperty(navigator.credentials, 'get', { configurable: true,
      value: () => Promise.reject(new DOMException('Cancelled', 'NotAllowedError')) }));
    await login.click();
    await expectStatus('登录未完成');
    assert.equal(calls.at(-1), 'login/start');
    assert.ok(await login.isEnabled());
    await page.reload();
    response = 'expired';
    await login.click();
    await expectStatus('这次通行密钥验证已结束，请重新开始');
    assert.ok(await login.isEnabled());
    response = 'lost';
    await login.click();
    await expectStatus('暂未收到确认');
    assert.ok(await login.isDisabled());
    assert.ok(await page.getByRole('button', { name: '使用 GitHub 注册或登录' }).isEnabled());
    await page.addInitScript(() => Object.defineProperty(window, 'PublicKeyCredential', { value: undefined }));
    await page.getByRole('button', { name: '刷新账户页' }).click();
    await expectStatus('此浏览器暂不支持');
    assert.ok(await login.isDisabled());
    assert.ok(await page.getByRole('button', { name: '使用 GitHub 注册或登录' }).isEnabled());
    assert.deepEqual(errors, []);
    console.log('PASS', fallback ? 'base64url fallback' : 'browser defaults', 'registration, discoverable signed assertion, cancellation, expiry, lost response, unsupported browser');
  } finally {
    await context.close();
  }
}

const browser = await chromium.launch({ executablePath: process.env.CHROMIUM_PATH, headless: true });
try {
  await ceremony(browser, false);
  await ceremony(browser, true);
} finally {
  await browser.close();
}
