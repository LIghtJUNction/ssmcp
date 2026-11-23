import { officialSources } from '../src/tool_repository/fixture.mjs';
// Exercise the real code_mode worker over its JSON transport. No model is invoked.
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
    const frames = [];
    const requests = [];
    let stderr = '';
    let failure;
    const stop = error => {
      failure ??= error;
      child.kill();
    };
    const timer = setTimeout(() => stop(new Error('event helper fixture timed out')), 10000);
    child.on('error', stop);
    child.stdin.on('error', stop);
    child.stderr.on('data', data => { stderr += data; });
    const input = createInterface({ input: child.stdout });
    input.on('line', line => {
      try {
        const frame = JSON.parse(line);
        if (frame.type === 'call') {
          requests.push(frame.request);
          Promise.resolve().then(() => reply(frame.request)).then(response => {
            if (!failure && !child.stdin.destroyed) {
              child.stdin.write(JSON.stringify({ type: 'reply', id: frame.id, ...response }) + '\n');
            }
          }).catch(stop);
        } else {
          frames.push(frame);
        }
      } catch (error) { stop(error); }
    });
    child.on('close', (exitCode, signal) => {
      clearTimeout(timer);
      input.close();
      if (failure) reject(failure);
      else resolve({ frames, requests, exitCode, signal, stderr });
    });
    child.stdin.write(JSON.stringify({
      tool_sources: officialSources(), type: 'run', code, clients: [{ id: 'one', type: 'shell', name: 'event-fixture' }],
    }) + '\n');
  });
}

const result = record => record.frames.find(frame => frame.type === 'result')?.value;
const error = record => record.frames.find(frame => frame.type === 'error')?.message;
const native = value => ({ ok: true, value: { isError: false, content: [], structuredContent: value } });
const succeeded = record => assert.equal(record.exitCode, 0, record.stderr || error(record));


test('event helpers use the authenticated builtin envelope and preserve source credentials',async()=>{
  const args={request_id:'once',filter:{kinds:['worker.completed'],client_ids:['one','two']},
    source_agents:[{client_id:'two',agent_id:'actor-two',token:'fixture-two'}],
    agent_id:'actor-one',token:'fixture-one'};
  const response={subscription_id:'sub',next_cursor:5,has_more:false};
  const record=await run('return await events.subscribe("one",'+JSON.stringify(args)+');',request=>{
    assert.deepEqual(request,{op:'builtin',name:'events.subscribe',arguments:{...args,client_id:'one'}});
    return {ok:true,value:response};
  });
  succeeded(record);assert.deepEqual(result(record),response);assert.equal(record.requests.length,1);
});
test('event waits and waitFor resume use one short native operation without polling',async()=>{
  const record=await run(
    'const page=await events.wait("one",{subscription_id:"sub"});'
    +'const resumed=await waitFor("one",{wait_id:"wait",after:0,window_ms:1000});'
    +'return {page,resumed};',
    request=>({ok:true,value:{name:request.name,args:request.arguments}}),
  );
  succeeded(record);
  assert.deepEqual(record.requests,[
    {op:'builtin',name:'events.wait',arguments:{subscription_id:'sub',client_id:'one',wait_ms:1000}},
    {op:'builtin',name:'await',arguments:{wait_id:'wait',after:0,window_ms:1000,client_id:'one',timeout_ms:1000}},
  ]);
});
test('long JS event windows fail before host dispatch and native failures are not replayed',async()=>{
  const invalid=await run(
    'const errors=[];for(const f of [()=>events.wait("one",{subscription_id:"sub",wait_ms:55000}),'
    +'()=>waitFor("one",{targets:[{type:"event",subscription_id:"sub"}],timeout_ms:55000})])'
    +'{try{await f()}catch(e){errors.push(e.message)}}return errors;',
    ()=>assert.fail('long JS wait reached host'),
  );
  succeeded(invalid);assert.equal(result(invalid).length,2);assert.equal(invalid.requests.length,0);
  const failed=await run('return await events.publish("one",{request_id:"once",name:"custom.build",data:{ok:true}});',
    ()=>({ok:false,error:'source credential rejected'}));
  assert.equal(failed.exitCode,1);assert.equal(error(failed),'source credential rejected');
  assert.equal(failed.requests.length,1);
});
