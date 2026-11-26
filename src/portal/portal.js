// The account page supplies exactly one action, scoped to its server-rendered session.
(() => {
  'use strict';
  const button = document.querySelector('[data-passkey]');
  const status = document.getElementById('passkey-status');
  if (!button || !status) return;

  const registration = button.dataset.passkey === 'register';
  const mode = registration ? 'register' : 'login';
  const completedKey = 'ssmcp-passkey-completed';
  function message(text) { status.textContent = text; }
  try {
    const completed = sessionStorage.getItem(completedKey);
    sessionStorage.removeItem(completedKey);
    if (registration && completed === 'register') message('通行密钥已绑定。下次可用它登录此账户。');
    if (registration && completed === 'login') message('已使用通行密钥登录。');
  } catch { /* The server-rendered account remains usable without browser storage. */ }

  if (!window.isSecureContext || !window.PublicKeyCredential || !navigator.credentials) {
    message(registration ? '此浏览器暂不支持通行密钥。请在支持的浏览器中打开账户页。'
      : '此浏览器暂不支持通行密钥。可以使用 GitHub 注册或登录。');
    return;
  }
  button.disabled = false;

  function decode(value) {
    const text = atob(value.replace(/-/g, '+').replace(/_/g, '/'));
    return Uint8Array.from(text, ch => ch.charCodeAt(0));
  }
  function encode(value) {
    const bytes = new Uint8Array(value);
    let raw = '';
    for (const byte of bytes) raw += String.fromCharCode(byte);
    return btoa(raw).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
  }
  function options(data) {
    if (registration && typeof PublicKeyCredential.parseCreationOptionsFromJSON === 'function') {
      return PublicKeyCredential.parseCreationOptionsFromJSON(data);
    }
    if (!registration && typeof PublicKeyCredential.parseRequestOptionsFromJSON === 'function') {
      return PublicKeyCredential.parseRequestOptionsFromJSON(data);
    }
    const parsed = { ...data, challenge: decode(data.challenge) };
    if (registration) parsed.user = { ...data.user, id: decode(data.user.id) };
    const credentialList = registration ? 'excludeCredentials' : 'allowCredentials';
    if (data[credentialList]) {
      parsed[credentialList] = data[credentialList].map(item => ({ ...item, id: decode(item.id) }));
    }
    return parsed;
  }
  function extensionJSON(value) {
    if (value instanceof ArrayBuffer) return encode(value);
    if (ArrayBuffer.isView(value)) return encode(new Uint8Array(value.buffer, value.byteOffset, value.byteLength));
    if (Array.isArray(value)) return value.map(extensionJSON);
    if (value && typeof value === 'object') {
      return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, extensionJSON(item)]));
    }
    return value;
  }
  function credentialJSON(credential) {
    if (typeof credential.toJSON === 'function') return credential.toJSON();
    const data = credential.response;
    const response = { clientDataJSON: encode(data.clientDataJSON) };
    if (registration) {
      response.attestationObject = encode(data.attestationObject);
      if (data.getTransports) response.transports = data.getTransports();
      if (data.getAuthenticatorData) response.authenticatorData = encode(data.getAuthenticatorData());
      if (data.getPublicKeyAlgorithm) response.publicKeyAlgorithm = data.getPublicKeyAlgorithm();
      const key = data.getPublicKey?.();
      if (key) response.publicKey = encode(key);
    } else {
      response.authenticatorData = encode(data.authenticatorData);
      response.signature = encode(data.signature);
      response.userHandle = data.userHandle ? encode(data.userHandle) : null;
    }
    return { id: credential.id, rawId: encode(credential.rawId), type: credential.type,
      authenticatorAttachment: credential.authenticatorAttachment ?? null, response,
      clientExtensionResults: extensionJSON(credential.getClientExtensionResults()) };
  }

  async function send(step, fields) {
    const result = await fetch('/account/passkeys/' + mode + '/' + step, {
      method: 'POST', mode: 'same-origin', credentials: 'same-origin', redirect: 'error',
      headers: { 'content-type': 'application/json', accept: 'application/json' },
      body: JSON.stringify({ csrf: button.dataset.csrf, ...fields }),
    });
    if (!result.ok) {
      const error = new Error('Passkey request failed');
      error.status = result.status;
      if (result.headers.get('content-type')?.includes('application/json')) {
        const problem = await result.json().catch(() => null);
        if (typeof problem?.error?.message === 'string') error.detail = problem.error.message.slice(0, 240);
      }
      throw error;
    }
    return result.json();
  }

  button.addEventListener('click', async () => {
    if (button.disabled) return;
    button.disabled = true;
    let phase = 'start';
    let retry = true;
    try {
      message('正在准备验证…');
      const start = await send('start', {});
      const publicKey = options(start.publicKey);
      phase = 'authenticator';
      message('请按设备提示完成验证。');
      const credential = registration ? await navigator.credentials.create({ publicKey })
        : await navigator.credentials.get({ publicKey });
      if (!credential) throw new DOMException('No credential', 'NotAllowedError');
      const serialized = credentialJSON(credential);
      phase = 'finish';
      message('正在核对验证结果…');
      const result = await send('finish', { credential: serialized });
      if (result.redirect !== '/#account') throw new Error('Unexpected account destination');
      retry = false;
      message(registration ? '通行密钥已绑定。' : '已使用通行密钥登录。');
      try { sessionStorage.setItem(completedKey, mode); } catch { /* Optional status after reload. */ }
      // A hash-only navigation would leave the anonymous HTML and old CSRF in place.
      history.replaceState(null, '', '/#account');
      location.reload();
    } catch (error) {
      if (phase === 'authenticator' && (error.name === 'NotAllowedError' || error.name === 'AbortError')) {
        message(registration ? '绑定未完成。操作可能已取消或超时。'
          : '登录未完成。操作可能已取消、超时，或设备未找到可用密钥。也可使用 GitHub 登录。');
      } else if (phase === 'authenticator' && error.name === 'InvalidStateError') {
        message('此通行密钥可能已绑定。可以使用现有密钥登录。');
      } else if (phase === 'finish' && (!error.status || error.status >= 500 || error.status === 408)) {
        retry = false;
        const outcome = registration ? '已提交绑定，暂未收到确认。请刷新账户页或尝试使用该密钥登录，再决定是否重新绑定。'
          : '已提交登录，暂未收到确认。请刷新此页查看登录状态。';
        message(error.detail ? error.detail + ' ' + outcome : outcome);
        const refresh = document.createElement('button');
        refresh.type = 'button';
        refresh.className = 'quiet';
        refresh.textContent = '刷新账户页';
        refresh.addEventListener('click', () => location.reload());
        status.append(' ', refresh);
      } else if (error.detail) {
        message(error.detail);
      } else if (error.status === 401 || error.status === 403) {
        message('验证未通过或已过期。请刷新此页后重新验证。');
      } else if (error.status === 429) {
        message('验证过于频繁，请稍后再试。');
      } else if (phase === 'start') {
        message('暂时无法开始通行密钥验证。请检查连接后重试。');
      } else {
        message(registration ? '通行密钥未能完成绑定。请重新开始验证。'
          : '通行密钥验证未通过。请重新验证，或使用 GitHub 登录。');
      }
    } finally {
      button.disabled = !retry;
    }
  });
})();
