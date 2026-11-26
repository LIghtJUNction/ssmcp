import assert from 'node:assert/strict';
import test from 'node:test';
import { gatewayFeedback } from '../src/gateway-feedback.mjs';
import { Gateway } from '../src/gateway.mjs';

const denied = {status:402, code:'traffic_quota_exceeded', effect:'none',
  message:'https://elsewhere.example/pay', recharge_url:'https://untrusted.example/'};
test('only a confirmed native quota rejection offers the submitted relay account page', () => {
  for (const [relay, trafficUrl] of [
    ['SSMCP.LMM.BEST', 'https://ssmcp.lmm.best/account/traffic'],
    ['https://relay.example:445/', 'https://relay.example:445/account/traffic'],
    ['http://127.0.0.1:8765', 'http://127.0.0.1:8765/account/traffic'],
    ['http://[::1]:8765', 'http://[::1]:8765/account/traffic'],
  ]) assert.equal(gatewayFeedback(denied, {relay}).trafficUrl, trafficUrl);
  for (const relay of [undefined, '', 'http://relay.example', 'relay.example@other.example',
    'https://user:pass@other.example', 'https://relay.example/path', 'relay.example/#',
    'relay.example?token=secret', 'https://relay.example\\@other.example', 'javascript:alert(1)',
    '//other.example', 'https://relay.example:0', 'https://relay.example/%2e%2e']) {
    assert.equal(gatewayFeedback(denied, {relay}).trafficUrl, null);
  }
  for (const patch of [{status:429}, {status:503}, {status:0}, {code:'http_error'},
    {effect:'other'}, {effect:undefined}]) {
    assert.equal(gatewayFeedback({...denied,...patch}, {relay:'relay.example'}).trafficUrl, undefined);
  }
  const unresolved = gatewayFeedback(denied, {relay:'relay.example', creating:true, unresolved:true});
  assert.match(unresolved.message, /结果仍需用原请求核对/);
  assert.doesNotMatch(unresolved.message, /会话未创建/);
  assert.match(gatewayFeedback({message:'gateway pairing is required', status:401, effect:'none'}, {unresolved:true}).message,
    /重新配对.*原请求/);
  assert.equal(gatewayFeedback({status:503, code:'unknown', effect:'may_have_occurred'}, {uncertain:'核对原请求'}).message, '核对原请求');
});

test('a published session retains the uncertainty of prior work after an authoritative relay denial', async () => {
  for (const [status, code, reason] of [[402,'traffic_quota_exceeded','本月可用流量不足'],
    [409,'online_session_limit','结束先前在线的会话']]) {
    const wire = {code,effect:'may_have_occurred',message:'native reason',relay_origin:'https://relay.example'};
    const transport = new Gateway({fetchImpl:async () => Response.json({error:wire}, {status})});
    transport.setToken('example');
    await assert.rejects(transport.request('/connections/owned/events'), error => {
      const feedback = gatewayFeedback(error);
      assert.ok(feedback.message.includes(reason));
      assert.match(feedback.message, /原会话及尚未收到回执的操作/);
      assert.doesNotMatch(feedback.message, /会话未创建/);
      assert.equal(feedback.trafficUrl, 'https://relay.example/account/traffic');
      assert.equal(gatewayFeedback(error, {relay:'other.example'}).trafficUrl, null);
      assert.equal(gatewayFeedback({...error,relayOrigin:'suspicious.example'}).trafficUrl, null);
      return true;
    });
    const before = gatewayFeedback({...wire,status,effect:'none'}, {creating:true,relay:'relay.example'});
    assert.match(before.message, /本次会话未创建/);
    if (code === 'online_session_limit') {
      assert.match(before.message, /已有 1 个在线会话/);
      assert.match(before.message, /首次充值到账后/);
      assert.doesNotMatch(before.message, /流量不足|无限流量/);
    }
  }
});

test('HTTP and transport errors retain their actual cause without a purchase action', async () => {
  const gateway = new Gateway({fetchImpl:async () => new Response('<html>proxy error</html>', {status:429})});
  gateway.setToken('a'.repeat(64));
  await assert.rejects(gateway.request('/sessions'), e =>
    e.code === 'http_error' && e.status === 429 && e.effect === 'none' && gatewayFeedback(e).message.includes('请求过多'));
  await assert.rejects(gateway.request('/owned-sessions', {method:'POST', mutation:true}), e =>
    e.status === 429 && e.effect === 'may_have_occurred' && gatewayFeedback(e, {uncertain:'核对原请求'}).message === '核对原请求');
  for (const [error, cause] of [
    [{status:401,code:'denied'}, '重新配对'],
    [{status:403,code:'denied',message:'attach lease expired'}, '控制授权已失效'],
    [{status:403,code:'denied',message:'gateway origin does not match its configured listener or proxy'}, '当前页面地址'],
    [{status:403,code:'denied',message:'some future reason'}, 'some future reason'],
    [{status:410,code:'closed'}, '重新连接'],
    [{code:'network_error'}, '网关地址'],
    [{status:503,code:'unavailable',message:'原生服务说明'}, '原生服务说明'],
  ]) {
    const feedback = gatewayFeedback({...error,effect:'none'}, {relay:'ssmcp.lmm.best'});
    assert.ok(feedback.message.includes(cause));
    assert.equal(feedback.trafficUrl, undefined);
  }
});
