// Exercise the real code_mode worker through its JSON transport; no model or
// external service is invoked. Run: node --test scripts/test_client_capabilities.mjs
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

const worker = process.env.SSMCP_CODE_MODE_SCRIPT
  ?? fileURLToPath(new URL('../src/code_mode.js', import.meta.url));

function run(code, reply) {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [worker], { stdio: ['pipe', 'pipe', 'pipe'] });
    const frames = [], requests = [];
    let stderr = '', failure;
    const stop = error => { failure ??= error; child.kill(); };
    const timer = setTimeout(() => stop(new Error('client capabilities fixture timed out')), 10000);
    child.on('error', stop);
    child.stdin.on('error', stop);
    child.stderr.on('data', data => { stderr += data; });
    const lines = createInterface({input:child.stdout});
    lines.on('line', line => {
      try {
        const frame = JSON.parse(line);
        if (frame.type === 'call') {
          requests.push(frame.request);
          Promise.resolve().then(() => reply(frame.request)).then(response => {
            if (!failure && !child.stdin.destroyed) {
              child.stdin.write(JSON.stringify({type:'reply', id:frame.id, ...response}) + '\n');
            }
          }).catch(stop);
        } else frames.push(frame);
      } catch (error) { stop(error); }
    });
    child.on('close', (exitCode, signal) => {
      clearTimeout(timer);
      lines.close();
      if (failure) reject(failure);
      else resolve({frames, requests, exitCode, signal, stderr});
    });
    child.stdin.write(JSON.stringify({
      type:'run', code, clients:[{id:'one', type:'shell', name:'search-fixture'}],
    }) + '\n');
  });
}
const value = record => record.frames.find(frame => frame.type === 'result')?.value;
const error = record => record.frames.find(frame => frame.type === 'error')?.message;
const success = record => assert.equal(record.exitCode, 0, record.stderr || error(record));
const failure = (record, pattern) => {
  assert.equal(record.exitCode, 1, record.stderr);
  assert.match(error(record), pattern);
};

test('dynamic native API calls a future client primitive without relay provider changes', async () => {
  const record = await run(
    "const api = native('one'); const same = await Promise.resolve(api); return await same.future.version_two.capture({region:[1,2,3,4]});",
    request => {
      assert.equal(request.op,'call');
      assert.equal(request.client_id,'one');
      assert.equal(request.name,'future.version_two.capture');
      assert.deepEqual(request.arguments,{region:[1,2,3,4]});
      return {ok:true,value:{content:[],structuredContent:{future:true},isError:false,'x-native':{v:2}}};
    });
  success(record);
  assert.deepEqual(value(record),{content:[],structuredContent:{future:true},isError:false,'x-native':{v:2}});
  assert.equal(record.requests.length,1);
});

test('client capability manifests are fresh and never shared between client IDs', async () => {
  const record = await run(
    "const a=await capabilities('one'); const b=await capabilities('two'); const changed=await capabilities('one'); return [a,b,changed];",
    request => {
      assert.equal(request.name,'ssmcp.client.capabilities');
      const generation = request.client_id === 'two' ? 20 : ++count;
      return {ok:true,value:{structuredContent:{client_version:'future',generation}}};
    });
  success(record);
  assert.deepEqual(value(record).map(v=>v.structuredContent.generation),[1,20,2]);
  assert.equal(record.requests.length,3);
});
let count=0;

test('client resource helper preserves opaque cursors, templates and future content fields', async () => {
  const record = await run(
    "const r=resources.forClient('one'); return [await r.list({cursor:'opaque:next',limit:2}),await r.templates({cursor:'templates:two'}),await r.read('future://report?part=2'),Object.isFrozen(r)];",
    request => ({ok:true,value:{structuredContent:{name:request.name,args:request.arguments,unknown:{kept:true}}}}));
  success(record);
  assert.deepEqual(record.requests.map(r=>[r.op,r.client_id,r.name,r.arguments]),[
    ['call','one','ssmcp.resources.list',{cursor:'opaque:next',limit:2}],
    ['call','one','ssmcp.resources.templates',{cursor:'templates:two'}],
    ['call','one','ssmcp.resources.read',{uri:'future://report?part=2'}],
  ]);
  assert.equal(value(record)[0].unknown.kept,true);
  assert.equal(value(record)[3],true);
});

test('dynamic API keeps native failures and validation instead of pretending success', async () => {
  const nativeFailure = await run(
    "return await native('one').future.fail({});",
    () => ({ok:true,value:{isError:true,content:[{type:'text',text:'Unavailable'}]}}));
  success(nativeFailure);
  assert.equal(value(nativeFailure).isError,true);
  const unregistered = await run(
    "return await native('one').missing.tool({});",
    () => ({ok:false,error:'tool is unavailable or not registered'}));
  failure(unregistered,/unavailable/);
  for (const code of [
    "return native('one').future.test([]);",
    "return native('one').future.test({},{});",
    "return native('one').constructor;",
    "return native('');",
  ]) {
    const invalid = await run(code,()=>assert.fail('invalid API must not call host'));
    assert.equal(invalid.exitCode,1);
    assert.equal(invalid.requests.length,0);
  }
});

test('legacy call and relay resource helpers keep their existing wire requests', async () => {
  const record = await run(
    "return [await call('one','legacy.tool',{x:1}),await resources.list(),await resources.templates(),await resources.read('ssmcp://guide')];",
    request=>({ok:true,value:request}));
  success(record);
  assert.deepEqual(record.requests.map(r=>r.name),['legacy.tool','resources.list','resources.templates','resources.read']);
  assert.equal(record.requests[0].op,'call');
  assert(record.requests.slice(1).every(r=>r.op==='builtin'));
});

test('say preserves Markdown, kind and retry identity through the registered message tool', async () => {
  const body = '## 结果\n保留中文和空格。';
  const requests = [
    {text:body},
    {text:body,kind:'answer',idempotency_key:'reply-1'},
    {text:'Next step',kind:'progress'},
  ];
  let sequence = 0;
  const record = await run(
    "return [await say('one'," + JSON.stringify(body) + "),"
      + "await say('one'," + JSON.stringify(requests[1]) + "),"
      + "await ui.message('one'," + JSON.stringify(requests[2]) + ")];",
    request => {
      const index = sequence++;
      assert.equal(request.op,'call');
      assert.equal(request.client_id,'one');
      assert.equal(request.name,'ssmcp.ui.message');
      assert.deepEqual(request.arguments,requests[index]);
      return {ok:true,value:{isError:false,content:[],structuredContent:{
        shown:true,accepted:true,message_id:'message-'+index,
        kind:request.arguments.kind??'progress',duplicate:index===1,history_saved:true,
      }}};
    });
  success(record);
  assert.equal(record.requests.length,3);
  assert.deepEqual(value(record),requests.map((request,index)=>({
    shown:true,accepted:true,message_id:'message-'+index,
    kind:request.kind??'progress',duplicate:index===1,history_saved:true,
  })));
});

test('say surfaces native and transport failures without a fallback message channel', async () => {
  for (const [reply,pattern] of [
    [{ok:true,value:{isError:true,content:[{type:'text',text:'idempotency_key already identifies different text'}]}},/idempotency_key/],
    [{ok:false,error:'tool is unavailable or not registered'},/unavailable/],
    [{ok:true,value:{content:[]}},/returned no structured result/],
  ]) {
    const record = await run(
      "return await say('one',{text:'Try once',idempotency_key:'once'});",
      request => {
        assert.equal(request.name,'ssmcp.ui.message');
        return reply;
      });
    failure(record,pattern);
    assert.equal(record.requests.length,1);
  }
});
