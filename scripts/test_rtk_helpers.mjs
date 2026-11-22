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


test('RTK uses literal argv and preserves exit codes, recall hints and exec options', async () => {
  const options = {
    command:['git','log','--format=%s with spaces','中文','; touch injected','$(id)',''],
    cwd:'project with spaces', env:{CUSTOM:'a b'}, stdin:'null',
    timeout_ms:1000, wait_ms:20, max_bytes:2048,
  };
  const snapshot = {job_id:'job',running:false,exit_code:7};
  const record = await run('return await rtk.run("one",' + JSON.stringify(options) + ');', request => {
    assert.equal(request.op,'call');
    assert.equal(request.client_id,'one');
    if (request.name === 'ssmcp.exec.start') return native({job_id:'job',running:true});
    if (request.name === 'ssmcp.exec.wait') return native(snapshot);
    assert.equal(request.name,'ssmcp.exec.read');
    return native({snapshot,stdout:'FAILED\n[full output: rtk recall 1234abcd]\n',stderr:''});
  });
  succeeded(record);
  assert.deepEqual(record.requests.map(r => [r.name,r.arguments]),[
    ['ssmcp.exec.start',{command:['rtk',...options.command],cwd:options.cwd,env:options.env,stdin:'null',timeout_ms:1000}],
    ['ssmcp.exec.wait',{job_id:'job',wait_ms:20}],
    ['ssmcp.exec.read',{job_id:'job',max_bytes:2048}],
  ]);
  assert.equal(result(record).exit_code,7);
  assert.match(result(record).stdout,/rtk recall 1234abcd/);
});

test('RTK start returns a regular exec job with stdin and cancellation', async () => {
  const record = await run(
    'const job=await rtk.start("one",{command:["pipe","--filter","cargo-test"],stdin:"pipe"});'
    + 'await exec.write("one",job.job_id,{data:"test result: ok\\n",close_stdin:true});'
    + 'await exec.cancel("one",job.job_id);return {frozen:Object.isFrozen(rtk),names:Object.keys(rtk)};',
    request => native({job_id:'job',name:request.name}),
  );
  succeeded(record);
  assert.equal(result(record).frozen,true);
  assert.deepEqual(result(record).names,['status','start','run']);
  assert.deepEqual(record.requests.map(r => [r.name,r.arguments]),[
    ['ssmcp.exec.start',{command:['rtk','pipe','--filter','cargo-test'],stdin:'pipe'}],
    ['ssmcp.exec.write',{data:'test result: ok\n',close_stdin:true,job_id:'job'}],
    ['ssmcp.exec.cancel',{job_id:'job'}],
  ]);
});

test('RTK status runs a bounded probe in the requested context', async () => {
  for (const state of [
    {running:false,exit_code:0,stdout:'rtk 1.2.0\n',available:true},
    {running:false,exit_code:2,stdout:'rtk broken\n',available:false},
    {running:true,exit_code:null,stdout:'',available:null},
  ]) {
    const snapshot={job_id:'probe',running:state.running,exit_code:state.exit_code};
    const record=await run('return await rtk.status("one",{cwd:".",env:{PATH:"/custom/bin"}});',request => {
      return native(request.name==='ssmcp.exec.read' ? {snapshot,stdout:state.stdout,stderr:''} : snapshot);
    });
    succeeded(record);
    assert.deepEqual(record.requests[0].arguments,{cwd:'.',env:{PATH:'/custom/bin'},command:['rtk','--version'],timeout_ms:5000});
    assert.deepEqual(record.requests[1].arguments,{job_id:'probe',wait_ms:5000});
    assert.deepEqual(record.requests[2].arguments,{job_id:'probe',max_bytes:1024});
    assert.equal(result(record).available,state.available);
    assert.equal(result(record).job_id,'probe');
    assert.equal(result(record).version,state.stdout.trim() || null);
  }
});

test('status distinguishes native spawn failure from permission and transport errors', async () => {
  const missing='could not start command: No such file or directory (os error 2)';
  const nativeError=message => ({ok:true,value:{isError:true,content:[{type:'text',text:message}]}});
  const absent=await run('return await rtk.status("one");',() => nativeError(missing));
  succeeded(absent);
  assert.deepEqual(result(absent),{available:false,version:null,error:missing});
  assert.equal(absent.requests.length,1);
  for (const reason of ['access denied for client','client disconnected','invalid execution cwd']) {
    const record=await run('return await rtk.status("one");',() => nativeError(reason));
    assert.equal(record.exitCode,1);
    assert.equal(error(record),reason);
    assert.equal(record.requests.length,1);
  }
});

test('invalid RTK options fail before native execution', async () => {
  const invalid=[
    ['run',null],['run','git status'],['run',[]],['run',{}],
    ['run',{command:[]}],['run',{command:['']}],['run',{command:['git',1]}],
    ['run',{command:['git','\0']}],['run',{command:Array(256).fill('x')}],
    ['run',{command:['git'],shell:true}],['run',{command:['git'],fallback:true}],
    ['start',{command:['git'],wait_ms:0}],['status',{command:['must-not-run']}],['status',[]],
  ];
  const record=await run(
    'const failures=[];for(const [name,args] of '+JSON.stringify(invalid)+'){'
    +'try{await rtk[name]("one",args);failures.push(null)}catch(e){failures.push(e.message)}}return failures;',
    () => assert.fail('invalid options reached native exec'),
  );
  succeeded(record);
  assert.equal(result(record).length,invalid.length);
  assert(result(record).every(message => typeof message==='string' && message.startsWith('rtk')));
  assert.deepEqual(record.requests,[]);
});

test('missing RTK never installs or reruns the original command', async () => {
  for(const method of ['run','start']) {
    const record=await run('return await rtk.'+method+'("one",{command:["git","commit","-m","once"]});',
      () => ({ok:true,value:{isError:true,content:[{type:'text',text:'could not start command: RTK executable missing'}]}}));
    assert.equal(record.exitCode,1);
    assert.match(error(record),/RTK executable missing/);
    assert.equal(record.requests.length,1);
    assert.deepEqual(record.requests[0].arguments.command,['rtk','git','commit','-m','once']);
  }
});

test('failed observation preserves job ID and never repeats a launched RTK command', async () => {
  const record=await run(
    'try{await rtk.run("one",{command:["cargo","test"]})}catch(e){return {message:e.message,job_id:e.job_id}}',
    request => request.name==='ssmcp.exec.start'
      ? native({job_id:'already-started',running:true})
      : {ok:false,error:'connection interrupted'},
  );
  succeeded(record);
  assert.equal(result(record).job_id,'already-started');
  assert.match(result(record).message,/Inspect exec.status\/exec.read/);
  assert.equal(record.requests.filter(r => r.name==='ssmcp.exec.start').length,1);
});

test('signals and artifact capture completeness survive the RTK wrapper', async () => {
  const artifact={id:'output-id',complete:false,bytes:50000,observed_bytes:60000};
  const snapshot={job_id:'job',running:false,exit_code:null,exit_signal:15,cancelled:true,
    timed_out:false,stdout:{artifact,eof:true,truncated:true}};
  const record=await run('return await rtk.run("one",{command:["cargo","test"]});',request => {
    if(request.name==='ssmcp.exec.start') return native({job_id:'job',running:true});
    return native(request.name==='ssmcp.exec.read' ? {snapshot,stdout:'condensed output',stderr:''} : snapshot);
  });
  succeeded(record);
  assert.equal(result(record).exit_signal,15);
  assert.equal(result(record).cancelled,true);
  assert.deepEqual(result(record).snapshot.stdout.artifact,artifact);
});

for (const method of ['start', 'run']) {
  test('RTK ' + method + ' rejects invalid options through catch without native calls', async () => {
    const invalid = [{command:[]}, {command:['git'],fallback:true}];
    const record = await run(
      'const failures=[];for(const options of ' + JSON.stringify(invalid) + '){'
      + 'failures.push(await rtk.' + method + '("one",options).catch(error=>error.message));}'
      + 'return failures;',
      () => assert.fail('invalid options reached native exec'),
    );
    succeeded(record);
    assert.equal(result(record).length, invalid.length);
    assert(result(record).every(message => typeof message === 'string' && message.startsWith('rtk')));
    assert.deepEqual(record.requests, []);
  });

  test('RTK ' + method + ' allSettled retains a launched job beside invalid options', async () => {
    const options = {command:['git','status'], ...(method === 'run' ? {wait_ms:0} : {})};
    const snapshot = {job_id:'started-once',running:false,exit_code:0};
    const record = await run(
      'const results=await Promise.allSettled(['
      + 'rtk.' + method + '("one",' + JSON.stringify(options) + '),'
      + 'rtk.' + method + '("one",{command:[]})]);'
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
    assert.match(entries[1].reason, /rtk command must contain/);
    if (method === 'run') {
      assert.equal(entries[0].value.exit_code, 0);
      assert.equal(entries[0].value.stdout, 'success');
    }
    assert.equal(record.requests.filter(request => request.name === 'ssmcp.exec.start').length, 1);
    assert.equal(record.requests.length, method === 'run' ? 2 : 1);
  });

  test('RTK ' + method + ' settles native launch failures without retrying', async () => {
    for (const response of [
      {ok:false,error:'launch transport interrupted'},
      {ok:true,value:{isError:true,content:[{type:'text',text:'could not start command: RTK executable missing'}]}},
    ]) {
      const record = await run(
        'const results=await Promise.allSettled(['
        + 'rtk.' + method + '("one",{command:["git","commit","-m","once"]})]);'
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
