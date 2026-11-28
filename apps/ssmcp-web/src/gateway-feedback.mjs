import { t } from './locale.mjs';
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
    const cause = quota ? t('trafficQuotaReached')
      : t('onlineLimitReached');
    const outcome = creating && error.effect === 'none' && !unresolved ? t('sessionNotCreated')
      : creating ? t('verifyPreviousCreation')
      : t('verifySessionAndOperations');
    const next = quota ? t('trafficQuotaNext')
      : t('onlineLimitNext');
    const submitted = trafficPage(relay);
    const confirmed = /^https?:\/\//i.test(error.relayOrigin ?? '') ? trafficPage(error.relayOrigin) : null;
    // When both parties know the relay, the normalized origins must agree.
    const mismatch = relay !== undefined && error.relayOrigin != null && submitted !== confirmed;
    return { message: cause + outcome + next, trafficUrl: mismatch ? null : submitted ?? confirmed };
  }
  if (error?.effect === 'may_have_occurred') return { message: uncertain ?? original };
  if (unresolved) {
    const { message } = gatewayFeedback(error);
    return { message: message + t('verifyUnresolvedCreation') };
  }
  if (error?.status === 401 || error?.code === 'authentication_required')
    return { message: t('pairAgain') };
  if (error?.status === 403 || error?.code === 'denied') {
    if (original === 'gateway origin does not match its configured listener or proxy')
      return { message: t('refusedPageOrigin') };
    if (original === 'attach lease expired' || original === 'invalid attach lease')
      return { message: t('expiredControlLease') };
    if (error.status === 403)
      return { message: t('deniedOperation') + (error.code === 'http_error' ? t('verifyControlPermission') : original) };
  }
  if (error?.status === 429)
    return { message: t('gatewayBusy') };
  if (error?.status === 410 || error?.code === 'closed')
    return { message: t('frontendClosed') };
  if (error?.code === 'network_error')
    return { message: t('gatewayUnreachable') };
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
  link.textContent = t('trafficAndOrders');
  const copy = document.createElement('button');
  copy.type = 'button';
  copy.className = 'text-button';
  copy.textContent = t('copyAccountUrl');
  copy.addEventListener('click', async () => {
    try {
      await navigator.clipboard.writeText(trafficUrl);
      copy.textContent = t('accountUrlCopied');
    } catch {
      // Embedded browsers can disable both external navigation and clipboard access.
      const field = document.createElement('input');
      field.readOnly = true;
      field.value = trafficUrl;
      field.setAttribute('aria-label', t('accountUrlCopyFallback'));
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
