// The account portal belongs to the relay submitted for this particular launch.
// Use a validated relay origin, never a response's recharge URL or a session display name.
function trafficPage(relay) {
  if (typeof relay !== 'string' || !relay || relay.length > 512 || /[\s\\@?#%]/u.test(relay)) return null;
  const origin = relay.includes('://') ? relay : 'https://' + relay;
  if (!/^https?:\/\/[^/]+\/?$/i.test(origin)) return null;
  try {
    const url = new URL(origin);
    const loopback = url.hostname === '[::1]' || /^127(?:\.\d{1,3}){3}$/.test(url.hostname);
    if (url.protocol !== 'https:' && !(loopback && url.protocol === 'http:')) return null;
    if (url.username || url.password || url.port === '0') return null;
    return new URL('/account/traffic', url.origin).href;
  } catch { return null; }
}

export function relayRestriction(error) {
  if (!['none', 'may_have_occurred'].includes(error?.effect)) return null;
  if (error.status === 402 && error.code === 'traffic_quota_exceeded') return error.code;
  if (error.status === 409 && error.code === 'online_session_limit') return error.code;
  return null;
}

export function gatewayFeedback(error, { relay, creating = false, uncertain, unresolved = false } = {}) {
  const original = error?.message ?? String(error);
  const restriction = relayRestriction(error);
  if (restriction) {
    const quota = restriction === 'traffic_quota_exceeded';
    const cause = quota ? '中继连接被拒绝：本月可用流量不足。'
      : '中继连接被拒绝：此账户已有 1 个在线会话，同时在线已达上限。';
    const outcome = creating && error.effect === 'none' && !unresolved ? '本次会话未创建。'
      : creating ? '上次创建结果仍需用原请求核对。'
      : '请核对原会话及尚未收到回执的操作。';
    const next = quota ? '可以查看本月用量，或等下月额度恢复后再连接。'
      : '结束先前在线的会话后即可重新连接。如果服务开放购买，首次充值到账后可同时使用多个会话。';
    const submitted = trafficPage(relay);
    const confirmed = /^https?:\/\//i.test(error.relayOrigin ?? '') ? trafficPage(error.relayOrigin) : null;
    // When both parties know the relay, the normalized origins must agree.
    const mismatch = relay !== undefined && error.relayOrigin != null && submitted !== confirmed;
    return { message: cause + outcome + next, trafficUrl: mismatch ? null : submitted ?? confirmed };
  }
  if (error?.effect === 'may_have_occurred') return { message: uncertain ?? original };
  if (unresolved) {
    const { message } = gatewayFeedback(error);
    return { message: message + ' 上次创建的结果仍未确定，请保留原请求并核对。' };
  }
  if (error?.status === 401 || error?.code === 'authentication_required')
    return { message: '本机网关需要重新配对。请从原来的应用、Magisk 模块或 ssmcp gui open 重新打开工作台。' };
  if (error?.status === 403 || error?.code === 'denied') {
    if (original === 'gateway origin does not match its configured listener or proxy')
      return { message: '网关拒绝了当前页面地址。请从原来的工作台入口重新打开。' };
    if (original === 'attach lease expired' || original === 'invalid attach lease')
      return { message: '当前会话的控制授权已失效。请重新连接，再申请控制。' };
    if (error.status === 403)
      return { message: '没有权限执行此操作。' + (error.code === 'http_error' ? '请核对会话和控制权限。' : original) };
  }
  if (error?.status === 429)
    return { message: '网关当前请求过多，请稍后再试。' };
  if (error?.status === 410 || error?.code === 'closed')
    return { message: '这条前端连接已关闭。请刷新会话列表，再重新连接。' };
  if (error?.code === 'network_error')
    return { message: '无法连接工作台网关。请检查网络和网关地址，并确认工作台服务正在运行。' };
  return { message: original };
}

export function renderGatewayFeedback(container, error, options) {
  const { message, trafficUrl } = gatewayFeedback(error, options);
  if (container.firstChild?.nodeValue === message
      && (container.querySelector('a')?.href ?? null) === (trafficUrl ?? null)) return;
  container.textContent = message;
  if (!trafficUrl) return;
  const link = document.createElement('a');
  link.href = trafficUrl;
  link.target = '_blank';
  link.rel = 'noopener noreferrer';
  link.textContent = '查看本月流量与订单';
  const copy = document.createElement('button');
  copy.type = 'button';
  copy.className = 'text-button';
  copy.textContent = '复制账户地址';
  copy.addEventListener('click', async () => {
    try {
      await navigator.clipboard.writeText(trafficUrl);
      copy.textContent = '地址已复制';
    } catch {
      // Embedded browsers can disable both external navigation and clipboard access.
      const field = document.createElement('input');
      field.readOnly = true;
      field.value = trafficUrl;
      field.setAttribute('aria-label', '复制此地址，在浏览器打开流量与订单');
      copy.replaceWith(field);
      field.focus();
      field.select();
    }
  });
  const actions = document.createElement('span');
  actions.className = 'traffic-actions';
  actions.append(link, copy);
  container.append(actions);
}
