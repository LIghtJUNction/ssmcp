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
    const timer = setTimeout(() => stop(new Error('RTK helper fixture timed out')), 10000);
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
      tool_sources: officialSources(), type: 'run', code, clients: [{ id: 'one', type: 'shell', name: 'rtk-fixture' }],
    }) + '\n');
  });
}

const result = record => record.frames.find(frame => frame.type === 'result')?.value;
const error = record => record.frames.find(frame => frame.type === 'error')?.message;
const native = value => ({ ok: true, value: { isError: false, content: [], structuredContent: value } });
const succeeded = record => assert.equal(record.exitCode, 0, record.stderr || error(record));


test('explicit RTK argv preserves literal arguments, exits, recall hints and exec options', async () => {
  const options = {
    command:['rtk','git','log','--format=%s with spaces','中文','; touch injected','$(id)',''],
    cwd:'project with spaces', env:{CUSTOM:'a b'}, stdin:'null',
    timeout_ms:1000, wait_ms:1000, max_bytes:2048,
  };
  const snapshot = {job_id:'job',running:false,exit_code:7};
  const record = await run('return await exec.run("one",' + JSON.stringify(options) + ');', request => {
    assert.equal(request.op,'call');
    assert.equal(request.client_id,'one');
    if (request.name === 'ssmcp.exec.start') return native({job_id:'job',running:true});
    if (request.name === 'ssmcp.exec.wait') return native(snapshot);
    assert.equal(request.name,'ssmcp.exec.read');
    return native({snapshot,stdout:'FAILED\n[full output: rtk recall 1234abcd]\n',stderr:''});
  });
  succeeded(record);
  assert.deepEqual(record.requests[0].arguments,{
    command:options.command,cwd:options.cwd,env:options.env,stdin:'null',timeout_ms:1000,
  });
  assert.equal(record.requests[1].name,'ssmcp.exec.wait');
  assert.equal(record.requests[1].arguments.job_id,'job');
  assert(record.requests[1].arguments.wait_ms > 0 && record.requests[1].arguments.wait_ms <= 1000);
  assert.deepEqual(record.requests[2].arguments,{job_id:'job',max_bytes:2048});
  assert.equal(result(record).exit_code,7);
  assert.match(result(record).stdout,/rtk recall 1234abcd/);
});

test('RTK commands use regular exec jobs with stdin and cancellation', async () => {
  const record = await run(
    'const job=await exec.start("one",{command:["rtk","pipe","--filter","cargo-test"],stdin:"pipe"});'
    + 'await exec.write("one",job.job_id,{data:"test result: ok\\n",close_stdin:true});'
    + 'await exec.cancel("one",job.job_id);return {frozen:Object.isFrozen(exec),rtk:typeof rtk};',
    request => native({job_id:'job',name:request.name}),
  );
  succeeded(record);
  assert.deepEqual(result(record),{frozen:true,rtk:'undefined'});
  assert.deepEqual(record.requests.map(r => [r.name,r.arguments]),[
    ['ssmcp.exec.start',{command:['rtk','pipe','--filter','cargo-test'],stdin:'pipe'}],
    ['ssmcp.exec.write',{data:'test result: ok\n',close_stdin:true,job_id:'job'}],
    ['ssmcp.exec.cancel',{job_id:'job'}],
  ]);
});

test('explicit version probes return actual job state in the requested context', async () => {
  for (const state of [
    {running:false,exit_code:0,stdout:'rtk 1.2.0\n'},
    {running:false,exit_code:2,stdout:'rtk broken\n'},
    {running:true,exit_code:null,stdout:''},
  ]) {
    const snapshot={job_id:'probe',running:state.running,exit_code:state.exit_code};
    const record=await run('return await exec.run("one",{command:["rtk","--version"],cwd:".",env:{PATH:"/custom/bin"},timeout_ms:5000,wait_ms:1000,max_bytes:1024});',request => {
      return native(request.name==='ssmcp.exec.read' ? {snapshot,stdout:state.stdout,stderr:''}
        : request.name==='ssmcp.exec.start' ? {job_id:'probe',running:true} : snapshot);
    });
    succeeded(record);
    assert.deepEqual(record.requests[0].arguments,{command:['rtk','--version'],cwd:'.',env:{PATH:'/custom/bin'},timeout_ms:5000});
    assert(record.requests[1].arguments.wait_ms > 0 && record.requests[1].arguments.wait_ms <= 1000);
    assert.deepEqual(record.requests[2].arguments,{job_id:'probe',max_bytes:1024});
    assert.equal(result(record).job_id,'probe');
    assert.equal(result(record).running,state.running);
    assert.equal(result(record).exit_code,state.exit_code);
    assert.equal(result(record).stdout,state.stdout);
  }
});

test('version probes preserve spawn, permission and transport failures without fallback', async () => {
  for (const reason of [
    'could not start command: No such file or directory (os error 2)',
    'access denied for client','client disconnected','invalid execution cwd',
  ]) {
    const record=await run('return await exec.run("one",{command:["rtk","--version"]});',
      () => ({ok:true,value:{isError:true,content:[{type:'text',text:reason}]}}));
    assert.equal(record.exitCode,1);
    assert.equal(error(record),reason);
    assert.equal(record.requests.length,1);
  }
});

test('invalid local execution options fail before native execution', async () => {
  const invalid=[null,'rtk git status',[],{command:['rtk'],wait_ms:55001},
    {command:['rtk'],wait_ms:-1},{command:['rtk'],max_bytes:0}];
  const record=await run(
    'const failures=[];for(const args of '+JSON.stringify(invalid)+'){'
    +'try{await exec.run("one",args);failures.push(null)}catch(e){failures.push(e.message)}}return failures;',
    () => assert.fail('invalid options reached native exec'),
  );
  succeeded(record);
  assert.equal(result(record).length,invalid.length);
  assert(result(record).every(message => typeof message==='string' && message.startsWith('exec.run')));
  assert.deepEqual(record.requests,[]);
});

test('missing RTK never installs or reruns the original command', async () => {
  for(const method of ['run','start']) {
    const record=await run('return await exec.'+method+'("one",{command:["rtk","git","commit","-m","once"]});',
      () => ({ok:true,value:{isError:true,content:[{type:'text',text:'could not start command: RTK executable missing'}]}}));
    assert.equal(record.exitCode,1);
    assert.match(error(record),/RTK executable missing/);
    assert.equal(record.requests.length,1);
    assert.deepEqual(record.requests[0].arguments.command,['rtk','git','commit','-m','once']);
  }
});

test('failed observation preserves the RTK job ID without launching the command twice', async () => {
  const record=await run(
    'try{await exec.run("one",{command:["rtk","cargo","test"]})}catch(e){return {message:e.message,job_id:e.job_id}}',
    request => request.name==='ssmcp.exec.start'
      ? native({job_id:'already-started',running:true})
      : {ok:false,error:'connection interrupted'},
  );
  succeeded(record);
  assert.equal(result(record).job_id,'already-started');
  assert.match(result(record).message,/Inspect exec.status\/exec.read/);
  assert.equal(record.requests.filter(r => r.name==='ssmcp.exec.start').length,1);
});

test('signals and artifact capture completeness survive explicit RTK execution', async () => {
  const artifact={id:'output-id',complete:false,bytes:50000,observed_bytes:60000};
  const snapshot={job_id:'job',running:false,exit_code:null,exit_signal:15,cancelled:true,
    timed_out:false,stdout:{artifact,eof:true,truncated:true}};
  const record=await run('return await exec.run("one",{command:["rtk","cargo","test"]});',request => {
    if(request.name==='ssmcp.exec.start') return native({job_id:'job',running:true});
    return native(request.name==='ssmcp.exec.read' ? {snapshot,stdout:'condensed output',stderr:''} : snapshot);
  });
  succeeded(record);
  assert.equal(result(record).exit_signal,15);
  assert.equal(result(record).cancelled,true);
  assert.deepEqual(result(record).snapshot.stdout.artifact,artifact);
});

for (const method of ['start', 'run']) {
  test('explicit RTK ' + method + ' preserves native argv validation without retrying', async () => {
    const record = await run(
      'return await exec.' + method + '("one",{command:["rtk",0]}).catch(error=>error.message);',
      request => {
        assert.deepEqual(request.arguments.command,['rtk',0]);
        return {ok:true,value:{isError:true,content:[{type:'text',text:'command arguments must be strings'}]}};
      },
    );
    succeeded(record);
    assert.equal(result(record),'command arguments must be strings');
    assert.equal(record.requests.length,1);
  });

  test('explicit RTK ' + method + ' allSettled retains a launched job beside invalid options', async () => {
    const options = {command:['rtk','git','status'], ...(method === 'run' ? {wait_ms:0} : {})};
    const snapshot = {job_id:'started-once',running:false,exit_code:0};
    const record = await run(
      'const results=await Promise.allSettled(['
      + 'exec.' + method + '("one",' + JSON.stringify(options) + '),'
      + 'exec.' + method + '("one",null)]);'
      + 'return results.map(entry=>entry.status==="fulfilled"?entry:'
      + '{status:entry.status,reason:entry.reason.message});',
      request => {
        if (request.name === 'ssmcp.exec.start') {
          assert.deepEqual(request.arguments, {command:['rtk','git','status']});
          return native({job_id:'started-once',running:true});
        }
        assert.equal(method, 'run');
        assert.equal(request.name, 'ssmcp.exec.read');
        assert.equal(request.arguments.job_id, 'started-once');
        return native({snapshot,stdout:'success',stderr:''});
      },
    );
    succeeded(record);
    const entries = result(record);
    assert.deepEqual(entries.map(entry => entry.status), ['fulfilled','rejected']);
    assert.equal(entries[0].value.job_id, 'started-once');
    assert.match(entries[1].reason, /object/);
    if (method === 'run') {
      assert.equal(entries[0].value.exit_code, 0);
      assert.equal(entries[0].value.stdout, 'success');
    }
    assert.equal(record.requests.filter(request => request.name === 'ssmcp.exec.start').length, 1);
    assert.equal(record.requests.length, method === 'run' ? 2 : 1);
  });

  test('explicit RTK ' + method + ' settles native launch failures without retrying', async () => {
    for (const response of [
      {ok:false,error:'launch transport interrupted'},
      {ok:true,value:{isError:true,content:[{type:'text',text:'could not start command: RTK executable missing'}]}},
    ]) {
      const record = await run(
        'const results=await Promise.allSettled(['
        + 'exec.' + method + '("one",{command:["rtk","git","commit","-m","once"]})]);'
        + 'return results.map(entry=>({status:entry.status,reason:entry.reason?.message}));',
        request => {
          assert.equal(request.name, 'ssmcp.exec.start');
          assert.deepEqual(request.arguments.command, ['rtk','git','commit','-m','once']);
          return response;
        },
      );
      succeeded(record);
      const message = response.error ?? response.value.content[0].text;
      assert.deepEqual(result(record), [{status:'rejected',reason:message}]);
      assert.equal(record.requests.length, 1, 'a failed launch must never be retried');
    }
  });
}
