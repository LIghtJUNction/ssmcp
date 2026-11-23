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
    const timer = setTimeout(() => stop(new Error('execution helper fixture timed out')), 10000);
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
      tool_sources: officialSources(), type: 'run', code, clients: [{ id: 'one', type: 'shell', name: 'execution-fixture' }],
    }) + '\n');
  });
}

const result = record => record.frames.find(frame => frame.type === 'result')?.value;
const error = record => record.frames.find(frame => frame.type === 'error')?.message;
const native = value => ({ ok: true, value: { isError: false, content: [], structuredContent: value } });
const succeeded = record => assert.equal(record.exitCode, 0, record.stderr || error(record));
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));

test('execution titles pass through start, run and parallel without changing argv or job IDs', async () => {
  const command = ['python3', '-c', "print('literal fixture')"];
  const titles = ['Inspect direct start', '  Inspect run 中文  ', 'Inspect parallel one', 'Inspect parallel two'];
  const jobs = new Map();
  const record = await run(`
    const options = {command:${JSON.stringify(command)},cwd:'fixture',env:{UNCHANGED:'value'},timeout_ms:5000};
    const titles = ${JSON.stringify(titles)};
    return [
      await exec.start('one', {...options,title:titles[0]}),
      await exec.run('one', {...options,title:titles[1],wait_ms:0,max_bytes:64}),
      ...await exec.parallel('one', titles.slice(2).map(title => ({...options,title,wait_ms:0})), {concurrency:2}),
    ];
  `, request => {
    const args = request.arguments;
    if (request.name === 'ssmcp.exec.start') {
      const index = titles.indexOf(args.title);
      assert(index >= 0);
      assert.deepEqual(args, {command,cwd:'fixture',env:{UNCHANGED:'value'},timeout_ms:5000,title:titles[index]});
      const state = {job_id:`title-job-${index}`,title:args.title,command:args.command,running:false,exit_code:0};
      assert(!jobs.has(state.job_id), 'each titled command is submitted once');
      jobs.set(state.job_id, state);
      return native(state);
    }
    assert.equal(request.name, 'ssmcp.exec.read');
    return native({snapshot:jobs.get(args.job_id),stdout:'fixture output',stderr:''});
  });
  succeeded(record);
  assert.equal(jobs.size, titles.length);
  const values = result(record);
  const states = [values[0], values[1], ...values.slice(2).map(item => {
    assert.equal(item.status, 'fulfilled');
    return item.value;
  })];
  assert.deepEqual(states.map(state => state.title), titles);
  assert.deepEqual(states.map(state => state.job_id), titles.map((_, index) => `title-job-${index}`));
  assert(states.every(state => JSON.stringify(state.command) === JSON.stringify(command)));
});

test('parallel limits actual running jobs after zero and short waits, preserving input order', async () => {
  const jobs = new Map();
  const completed = [];
  let active = 0;
  let maximum = 0;
  const snapshot = id => ({ job_id: id, running: jobs.get(id), exit_code: jobs.get(id) ? null : id === 'c' ? 7 : 0 });
  const record = await run(`
    return await exec.parallel('one', [
      {command:['a'], wait_ms:0}, {command:['b'], wait_ms:1},
      {command:['c'], wait_ms:5}, {command:['d'], wait_ms:0},
    ], {concurrency:2});
  `, async request => {
    assert.equal(request.op, 'call');
    assert.equal(request.client_id, 'one');
    const args = request.arguments;
    if (request.name === 'ssmcp.exec.start') {
      const id = args.command[0];
      assert(active < 2, 'a running job must keep its pool slot after a poll deadline');
      assert(!jobs.has(id), 'each command must start exactly once');
      jobs.set(id, true);
      maximum = Math.max(maximum, ++active);
      return native(snapshot(id));
    }
    if (request.name === 'ssmcp.exec.wait') {
      const id = args.job_id;
      if (args.wait_ms === 60000) {
        await delay(id === 'a' ? 150 : 1);
        assert.equal(jobs.get(id), true);
        jobs.set(id, false);
        active--;
        completed.push(id);
      } else {
        assert([1, 5].includes(args.wait_ms));
      }
      return native({ ...snapshot(id), wait_timed_out: jobs.get(id) });
    }
    if (request.name === 'ssmcp.exec.read') {
      assert.equal(args.max_bytes, 2048);
      return native({
        snapshot: snapshot(args.job_id), stdout: jobs.get(args.job_id) ? 'pending' : args.job_id,
        stderr: '', stdout_cursor: 1, stderr_cursor: 0,
      });
    }
    assert.fail('unexpected native operation: ' + request.name);
  });
  succeeded(record);
  assert.equal(maximum, 2);
  assert.equal(active, 0);
  assert.equal(completed[0], 'b', 'fixture must complete out of input order');
  assert.deepEqual(result(record).map(entry => entry.status), ['fulfilled', 'fulfilled', 'fulfilled', 'fulfilled']);
  assert.deepEqual(result(record).map(entry => entry.value.job_id), ['a', 'b', 'c', 'd']);
  assert.deepEqual(result(record).map(entry => entry.value.stdout), ['a', 'b', 'c', 'd']);
  assert.deepEqual(result(record).map(entry => entry.value.running), [false, false, false, false]);
  assert.equal(result(record)[2].value.exit_code, 7, 'a nonzero command exit must remain observable');
  assert.equal(record.requests.filter(request => request.name === 'ssmcp.exec.wait' && request.arguments.wait_ms === 60000).length, 4);
});

test('observation failures retain job IDs and reserve workers for possibly running commands', async () => {
  const started = [];
  const record = await run(`
    return await exec.parallel('one', [
      {command:['first'],wait_ms:0}, {command:['second'],wait_ms:0},
      {command:['must-not-start'],wait_ms:0}, {command:['also-not-started'],wait_ms:0},
    ], {concurrency:2});
  `, request => {
    const args = request.arguments;
    if (request.name === 'ssmcp.exec.start') {
      started.push(args.command[0]);
      assert(started.length <= 2, 'unknown running commands still own their pool slots');
      return native({ job_id: args.command[0], running: true });
    }
    if (request.name === 'ssmcp.exec.read') {
      if (args.job_id === 'first') return { ok: false, error: 'read transport interrupted' };
      return native({ snapshot: { job_id: 'second', running: true }, stdout: '', stderr: '' });
    }
    if (request.name === 'ssmcp.exec.wait') {
      assert.deepEqual(args, { job_id: 'second', wait_ms: 60000 });
      return { ok: true, value: { isError: true, content: [{ type: 'text', text: 'native status temporarily unavailable' }] } };
    }
    assert.fail('unexpected native operation: ' + request.name);
  });
  succeeded(record);
  assert.deepEqual(started, ['first', 'second']);
  const entries = result(record);
  assert.equal(entries[0].job_id, 'first');
  assert.match(entries[0].reason, /Command started as first;.*read transport interrupted/);
  assert.equal(entries[1].job_id, 'second');
  assert.match(entries[1].reason, /native status temporarily unavailable/);
  assert(entries.every(entry => entry.status === 'rejected'));
  assert.deepEqual(entries.slice(2).map(entry => entry.not_started), [true, true]);
  assert(entries.slice(2).every(entry => !Object.hasOwn(entry, 'job_id')));
  assert(!record.requests.some(request => request.name === 'ssmcp.exec.cancel'), 'unknown work must not be implicitly cancelled');
});

test('missing completion state never permits another process to take the same slot', async () => {
  const record = await run(`
    return await exec.parallel('one', [{command:['unknown'],wait_ms:0}, {command:['next']}], {concurrency:1});
  `, request => {
    if (request.name === 'ssmcp.exec.start') return native({ job_id: 'unknown', running: true });
    if (request.name === 'ssmcp.exec.read') return native({ snapshot: { job_id: 'unknown', running: true } });
    if (request.name === 'ssmcp.exec.wait') return native({ job_id: 'unknown' });
    assert.fail('unexpected native operation: ' + request.name);
  });
  succeeded(record);
  assert.equal(record.requests.filter(request => request.name === 'ssmcp.exec.start').length, 1);
  assert.equal(result(record)[0].job_id, 'unknown');
  assert.match(result(record)[0].reason, /state is unavailable/);
  assert.equal(result(record)[1].not_started, true);
});

test('exhausting the native call budget leaves running jobs identifiable without oversubscription', async () => {
  const started = [];
  const record = await run(`
    return await exec.parallel('one', Array.from({length:4}, (_,index) => ({command:['job'+index],wait_ms:0})), {concurrency:2});
  `, request => {
    const id = request.name === 'ssmcp.exec.start' ? request.arguments.command[0] : request.arguments.job_id;
    if (request.name === 'ssmcp.exec.start') started.push(id);
    const state = { job_id: id, running: true, exit_code: null };
    return native(request.name === 'ssmcp.exec.read' ? { snapshot: state } : state);
  });
  succeeded(record);
  assert.deepEqual(started, ['job0', 'job1']);
  assert.equal(record.requests.length, 128);
  result(record).slice(0, 2).forEach((entry, index) => {
    assert.equal(entry.job_id, 'job' + index);
    assert.match(entry.reason, /128 native calls/);
  });
  assert(result(record).slice(2).every(entry => entry.not_started === true));
});

test('piped input and multiple terminal helpers preserve native arguments and structured results', async () => {
  const terminals = [
    ['open', { name: 'build', cwd: '.', rows: 25, cols: 100 }],
    ['list', {}],
    ['select', { terminal_id: 'pty-2' }],
    ['write', { terminal_id: 'pty-2', data: '中文\n' }],
    ['read', { terminal_id: 'pty-2', cursor: 23, max_bytes: 64, wait_ms: 0 }],
    ['status', { terminal_id: 'pty-2' }],
    ['resize', { terminal_id: 'pty-2', rows: 31, cols: 110 }],
    ['close', { terminal_id: 'pty-2' }],
  ];
  const record = await run(`
    const replies = [];
    replies.push(await exec.start('one', {command:['cat'],stdin:'pipe'}));
    replies.push(await exec.write('one', 'job', {data:${JSON.stringify('中文\n')},wait_ms:20,job_id:'cannot-override'}));
    replies.push(await exec.write('one', 'job', {data_base64:'AP8=',wait_ms:0}));
    replies.push(await exec.write('one', 'job', {close_stdin:true}));
    for (const [name,args] of ${JSON.stringify(terminals)}) replies.push(await terminal[name]('one',args));
    return {replies, frozen:Object.isFrozen(exec) && Object.isFrozen(terminal)};
  `, request => native({ name: request.name, arguments: request.arguments, nativeResult: true }));
  succeeded(record);
  assert.equal(result(record).frozen, true);
  const expected = [
    ['ssmcp.exec.start', { command: ['cat'], stdin: 'pipe' }],
    ['ssmcp.exec.write', { data: '中文\n', wait_ms: 20, job_id: 'job' }],
    ['ssmcp.exec.write', { data_base64: 'AP8=', wait_ms: 0, job_id: 'job' }],
    ['ssmcp.exec.write', { close_stdin: true, job_id: 'job' }],
    ...terminals.map(([name, args]) => ['terminal.' + name, args]),
  ];
  assert.deepEqual(record.requests.map(request => [request.name, request.arguments]), expected);
  record.requests.forEach(request => {
    assert.equal(request.op, 'call');
    assert.equal(request.client_id, 'one');
  });
  assert.deepEqual(result(record).replies, expected.map(([name, args]) => ({ name, arguments: args, nativeResult: true })));
});


test('skill views declare effective defaults while preserving explicit scope and roots', async () => {
  const record = await run(`
    const options = {cursor:'next',limit:8};
    const replies = [
      await skills.view('one'),
      await skills.view('one', {}),
      await skills.view('one', options),
      await skills.view('one', {root:'workspace'}),
      await skills.view('one', {root:'context-1',cursor:'next'}),
      await skills.view('one', {scope:'root'}),
      await skills.view('one', {scope:'effective'}),
    ];
    return {replies,options};
  `, request => {
    assert.equal(request.op, 'call');
    assert.equal(request.client_id, 'one');
    assert.equal(request.name, 'ssmcp.skills.view');
    return native(request.arguments);
  });
  succeeded(record);
  const expected = [
    {scope:'effective'},
    {scope:'effective'},
    {cursor:'next',limit:8,scope:'effective'},
    {root:'workspace'},
    {root:'context-1',cursor:'next'},
    {scope:'root'},
    {scope:'effective'},
  ];
  assert.deepEqual(record.requests.map(request => request.arguments), expected);
  assert.deepEqual(result(record).replies, expected);
  assert.deepEqual(result(record).options, {cursor:'next',limit:8}, 'caller options are not mutated');
});

test('skill views preserve invalid argument errors and never retry a rejected effective request', async () => {
  const invalid = await run(`
    const failures = [];
    for (const args of [null,[],1,'invalid',true]) {
      try { await skills.view('one',args); failures.push('unexpected success'); }
      catch (failure) { failures.push(failure.message); }
    }
    return failures;
  `, () => assert.fail('invalid argument types must not reach the native provider'));
  succeeded(invalid);
  assert.deepEqual(result(invalid), Array(5).fill('tool arguments must be a JSON object'));
  assert.equal(invalid.requests.length, 0);

  for (const response of [
    {ok:false,error:'invalid context arguments: unknown field scope'},
    {ok:true,value:{isError:true,content:[{type:'text',text:'invalid context arguments: unknown field scope'}]}},
  ]) {
    const record = await run(`return await skills.view('one');`, request => {
      assert.equal(request.name, 'ssmcp.skills.view');
      assert.deepEqual(request.arguments, {scope:'effective'});
      return response;
    });
    assert.match(error(record), /unknown field scope/);
    assert.equal(result(record), undefined);
    assert.equal(record.requests.length, 1, 'an older client must fail without a single-root fallback');
  }
});

test('local validation errors leave parallel workers available for later valid commands', async () => {
  for (const concurrency of [1, 2]) {
    const record = await run(`
      return await exec.parallel('one', [
        null, ['invalid'],
        {command:['invalid-wait'], wait_ms:60001},
        {command:['first'], wait_ms:0},
        {command:['invalid-output'], max_bytes:0},
        {command:['last'], wait_ms:0, max_bytes:17},
      ], {concurrency:${concurrency}});
    `, request => {
      const id = request.name === 'ssmcp.exec.start'
        ? request.arguments.command[0] : request.arguments.job_id;
      assert(['first', 'last'].includes(id), 'invalid options must never reach native execution');
      const state = {job_id:id, running:false, exit_code:0};
      if (request.name === 'ssmcp.exec.start') return native(state);
      assert.equal(request.name, 'ssmcp.exec.read');
      assert.equal(request.arguments.max_bytes, id === 'last' ? 17 : 2048);
      return native({snapshot:state, stdout:id, stderr:''});
    });
    succeeded(record);
    const entries = result(record);
    assert.deepEqual(entries.map(entry => entry.status),
      ['rejected', 'rejected', 'rejected', 'fulfilled', 'rejected', 'fulfilled']);
    for (const index of [0, 1, 2, 4]) {
      assert.equal(entries[index].not_started, true);
      assert(!Object.hasOwn(entries[index], 'job_id'));
    }
    assert.equal(entries[3].value.stdout, 'first');
    assert.equal(entries[5].value.stdout, 'last');
    assert.equal(record.requests.filter(request => request.name === 'ssmcp.exec.start').length, 2);
  }
});

test('a lost start response reserves its parallel worker even without a job ID', async () => {
  const record = await run(`
    return await exec.parallel('one', [
      {command:['uncertain']}, {command:['must-not-start']}
    ], {concurrency:1});
  `, request => {
    assert.equal(request.name, 'ssmcp.exec.start');
    assert.deepEqual(request.arguments.command, ['uncertain']);
    return {ok:false, error:'start transport interrupted'};
  });
  succeeded(record);
  assert.equal(record.requests.length, 1);
  assert.match(result(record)[0].reason, /start transport interrupted/);
  assert(!Object.hasOwn(result(record)[0], 'not_started'));
  assert.equal(result(record)[1].not_started, true);
});

test('job helpers accept native option objects and legacy IDs with identical routing', async () => {
  const calls = [
    ['status', {}],
    ['wait', {wait_ms:0}],
    ['read', {stdout_cursor:41, stderr_cursor:7, max_bytes:64, wait_ms:1}],
    ['write', {data:'中文', close_stdin:true, wait_ms:10}],
    ['cancel', {}],
    ['forget', {}],
  ];
  const record = await run(`
    const calls = ${JSON.stringify(calls)};
    const replies = [];
    for (const [name,options] of calls) {
      const object = Object.freeze({...options,job_id:'job'});
      replies.push(await exec[name]('one',object));
      replies.push(await exec[name]('one','job',Object.freeze({...options,job_id:'cannot-override'})));
      if (JSON.stringify(object) !== JSON.stringify({...options,job_id:'job'})) throw new Error('mutated options');
    }
    return replies;
  `, request => native({name:request.name, arguments:request.arguments}));
  succeeded(record);
  const expected = calls.flatMap(([name, options]) => Array(2).fill({
    name:'ssmcp.exec.' + name, arguments:{...options,job_id:'job'}
  }));
  assert.deepEqual(result(record), expected);
  assert.deepEqual(record.requests.map(({name,arguments:args}) => ({name,arguments:args})), expected);
  assert(record.requests.every(request => request.client_id === 'one' && request.op === 'call'));
});

test('invalid and ambiguous job arguments fail before any native request', async () => {
  const record = await run(`
    const failures = [];
    for (const name of ['status','wait','read','write','cancel','forget']) {
      const invalid = [
        [], [null], [[]], [1], [true], [''], [{}],
        [{job_id:''}], [{job_id:2}], [{job_id:{}}],
        [{job_id:'job'}, {}], [{job_id:'job'}, null],
        ['job',null], ['job',[]], ['job',1], ['job',true], ['job','text'],
      ];
      for (const args of invalid) {
        await exec[name]('one',...args).then(
          () => failures.push('unexpected success'),
          failure => failures.push(failure.message)
        );
      }
    }
    return failures;
  `, () => assert.fail('invalid helper arguments must not reach a native tool'));
  succeeded(record);
  assert.equal(record.requests.length, 0);
  assert.equal(result(record).length, 102);
  assert(result(record).every(message => /^exec\.(status|wait|read|write|cancel|forget) /.test(message)));
  assert(result(record).every(message => message !== 'unexpected success'));
});

test('job object calls preserve native failures without retrying side effects', async () => {
  for (const response of [
    {ok:false,error:'transport interrupted'},
    {ok:true,value:{isError:true,content:[{type:'text',text:'native approval required'}]}},
  ]) {
    const record = await run(`
      const failures = [];
      for (const name of ['status','wait','read','write','cancel','forget']) {
        try { await exec[name]('one',{job_id:'job'}); }
        catch (failure) { failures.push(failure.message); }
      }
      return failures;
    `, () => response);
    succeeded(record);
    const message = response.error ?? response.value.content[0].text;
    assert.deepEqual(result(record), Array(6).fill(message));
    assert.equal(record.requests.length, 6);
    assert.deepEqual(record.requests.map(request => request.name),
      ['status','wait','read','write','cancel','forget'].map(name => 'ssmcp.exec.'+name));
  }
});


test('UI helpers expose registered workflows and preserve native arguments and failures', async () => {
  const names = ['get','set','action','filter','queue','message','present','ask','questions','harness'];
  const record = await run(
    'const names=' + JSON.stringify(names) + ';'
    + 'const values=await Promise.all(names.map(name=>ui[name]("one",{fixture:name})));'
    + 'return {values,frozen:Object.isFrozen(ui),unsupported:typeof ui.messages,retiredHistory:typeof ui.history};',
    request => {
      assert(names.some(name => request.name === 'ssmcp.ui.' + name));
      assert.equal(request.client_id, 'one');
      assert.equal(request.arguments.fixture, request.name.slice('ssmcp.ui.'.length));
      return native({name:request.name});
    },
  );
  succeeded(record);
  assert.equal(result(record).frozen, true);
  assert.equal(result(record).unsupported, 'undefined');
  assert.equal(result(record).retiredHistory, 'undefined');
  assert.deepEqual(result(record).values.map(value=>value.name), names.map(name=>'ssmcp.ui.'+name));
  const rejected = await run(
    'try{await ui.questions("one",{id:"question-id"});return "unexpected";}catch(error){return error.message;}',
    request => {
      assert.deepEqual(request, {
        op:'call',client_id:'one',name:'ssmcp.ui.questions',arguments:{id:'question-id'},
      });
      return {ok:true,value:{isError:true,content:[{type:'text',text:'questions unavailable'}]}};
    },
  );
  succeeded(rejected);
  assert.equal(result(rejected), 'questions unavailable');
  assert.equal(rejected.requests.length, 1);
});

test('live UI events preserve selectors and failures without retrying native reads', async () => {
  const options = {agent_id:'agent_'+'a'.repeat(32),cursor:7,generation:'live-generation',limit:16};
  const expectedRequests = [
    {op:'tools',client_id:'one',cursor:null},
    {op:'call',client_id:'one',name:'ssmcp.ui.events',arguments:{...options,action:'thread',client_id:'one'}},
  ];
  for (const response of [
    native({fixture:'live-events'}),
    {ok:false,error:'events transport interrupted'},
    {ok:true,value:{isError:true,content:[{type:'text',text:'events unavailable'}]}},
  ]) {
    const record = await run(`
      const options = Object.freeze(${JSON.stringify(options)});
      let value, error;
      try { value = await ui.events('one', options); }
      catch (failure) { error = failure.message; }
      return {value, error, options};
    `, request => {
      if (request.op === 'tools') {
        assert.deepEqual(request, expectedRequests[0]);
        return {ok:true,value:{tools:[{
          name:'ssmcp.ui.events',inputSchema:{type:'object',properties:{action:{const:'thread'}}},
        }]}};
      }
      assert.deepEqual(request, expectedRequests[1]);
      return response;
    });
    succeeded(record);
    assert.deepEqual(record.requests, expectedRequests);
    const message = response.error ?? response.value.content?.[0]?.text;
    assert.deepEqual(result(record), message
      ? {error:message,options}
      : {value:response.value.structuredContent,options});
  }
});

test('exec.run passes the artifact budget through start and returns the scoped read notice', async () => {
  const artifacts = {
    directory: '.ssmcp/task-artifacts/0123456789abcdef',
    logical_bytes: 12,
    soft_limit_bytes: 10,
    incomplete: false,
    notice: {
      type: 'ssmcp.exec.artifact_budget_exceeded',
      logical_bytes: 12,
      soft_limit_bytes: 10,
    },
  };
  const record = await run(`
    return await exec.run('one', {command:['true'],track_artifacts:true,artifact_budget_bytes:10});
  `, ({name, arguments: args}) => {
    const snapshot = {job_id:'job',running:false,exit_code:0};
    if (name === 'ssmcp.exec.start') {
      assert.deepEqual(args, {command:['true'],track_artifacts:true,artifact_budget_bytes:10});
      return native({job_id:'job',running:true});
    }
    assert.equal(args.job_id, 'job');
    if (name === 'ssmcp.exec.wait') return native(snapshot);
    assert.equal(name, 'ssmcp.exec.read');
    return native({snapshot:{...snapshot,artifacts},stdout:'',stderr:''});
  });
  succeeded(record);
  assert.deepEqual(result(record).artifacts, artifacts);
  assert.deepEqual(result(record).snapshot.artifacts, artifacts);
});
