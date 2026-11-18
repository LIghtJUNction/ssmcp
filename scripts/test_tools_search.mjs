// Exercise the real code_mode worker through its JSON transport; no model or
// external service is invoked. Run: node --test scripts/test_tools_search.mjs
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
    const timer = setTimeout(() => stop(new Error('tool search fixture timed out')), 10000);
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
const descriptor = (name, description = '') => ({
  name, description, inputSchema:{type:'object', properties:{}, additionalProperties:false},
});
const replyPages = pages => request => {
  assert.equal(request.op, 'tools', 'search must never invoke a native tool');
  assert.equal(request.client_id, 'one');
  assert(pages.has(request.cursor), 'unexpected cursor: ' + request.cursor);
  return {ok:true, value:pages.get(request.cursor)};
};

test('search keeps tools() compatible and returns concise routing namespaces across pages', async () => {
  const pages = new Map([
    [null, {tools:[descriptor('claude.Read', 'Read files'), descriptor('ssmcp.browser.tabs', 'Current browser tabs')], nextCursor:'v1:64'}],
    ['v1:64', {tools:[descriptor('playwright.browser_snapshot', 'Read the current page')]}],
  ]);
  const record = await run(`
    const original = await tools('one');
    const found = await tools.search('one',{query:'BROWSER'});
    return {original,found,frozen:Object.isFrozen(tools),hidden:typeof globalThis.__ssmcp_digest};
  `, replyPages(pages));
  success(record);
  assert.deepEqual(value(record).original, pages.get(null), 'existing tools() must retain the complete original page');
  const found = value(record).found;
  assert.deepEqual(found.tools.map(item => item.name), ['ssmcp.browser.tabs','playwright.browser_snapshot']);
  assert.deepEqual(found.tools.map(item => item.namespace), ['ssmcp.browser','playwright']);
  assert(found.tools.every(item => !Object.hasOwn(item,'inputSchema') && !Object.hasOwn(item,'provider')));
  assert.equal(found.namespace_source, 'registered_tool_name');
  assert.equal(found.include_schemas, false);
  assert.equal(found.complete, true);
  assert.equal(found.nextCursor, null);
  assert.deepEqual(found.stats, {pages_read:2,tools_scanned:3,total_tools_scanned:3});
  assert.equal(value(record).frozen, true);
  assert.equal(value(record).hidden, 'undefined');
  assert.deepEqual(record.requests.map(item => item.cursor), [null,null,'v1:64']);
});

test('include_schemas preserves the entire native descriptor exactly', async () => {
  const native = {
    ...descriptor('codex.mcp.native.read','Read a native resource'),
    inputSchema:{
      $schema:'https://json-schema.org/draft/2020-12/schema',
      $defs:{Resource:{type:'object',properties:{uri:{type:'string',format:'uri'}},required:['uri']}},
      allOf:[{$ref:'#/$defs/Resource'},{not:{required:['secret']}}],
      unevaluatedProperties:false,
    },
    outputSchema:{oneOf:[{type:'null'},{$ref:'https://example.invalid/schema.json'}]},
    annotations:{readOnlyHint:true, idempotentHint:true},
    'x-native-extension':{approval:'native', values:[null,false,7,'中文']},
  };
  const record = await run("return await tools.search('one',{query:'native',include_schemas:true});",
    replyPages(new Map([[null,{tools:[native]}]])));
  success(record);
  assert.deepEqual(value(record).tools,[native]);
  assert(!Object.hasOwn(value(record).tools[0],'namespace'),'full descriptors must not gain synthetic schema fields');
});

test('short descriptions preserve Unicode boundaries and disclose shortening', async () => {
  const description = '界'.repeat(239) + '🦀' + '、后续';
  const native = descriptor('ssmcp.browser.snapshot', description);
  const record = await run("return await tools.search('one');",replyPages(new Map([[null,{tools:[native]}]])));
  success(record);
  const item = value(record).tools[0];
  assert.equal(item.description,'界'.repeat(239)+'🦀…');
  assert.equal(item.description_truncated,true);
  assert.equal(Buffer.from(item.description,'utf8').toString('utf8'),item.description);
});

test('cursor resumes within a page in a new worker without skipping or duplicating matches', async () => {
  const pages = new Map([
    [null,{tools:['a','b','c'].map(n=>descriptor('playwright.'+n)),nextCursor:'generation:64'}],
    ['generation:64',{tools:[descriptor('playwright.d')]}],
  ]);
  const first = await run("return await tools.search('one',{query:'playwright',limit:2});",replyPages(pages));
  success(first);
  assert.deepEqual(value(first).tools.map(x=>x.name),['playwright.a','playwright.b']);
  const cursor=value(first).nextCursor;
  assert(cursor);
  const second=await run(`return await tools.search('one',{query:'playwright',limit:2,cursor:${JSON.stringify(cursor)}});`,replyPages(pages));
  success(second);
  assert.deepEqual(value(second).tools.map(x=>x.name),['playwright.c','playwright.d']);
  assert.deepEqual(second.requests.map(r=>r.cursor),[null,'generation:64']);
  assert.equal(value(second).complete,true);
  assert.equal(value(second).stats.total_tools_scanned,4);
});

test('continuations bind client, query, limit and schema mode before any host request',async()=>{
  const first=await run("return await tools.search('one',{query:'x',limit:1});",
    replyPages(new Map([[null,{tools:[descriptor('x.a'),descriptor('x.b')]}]])));
  success(first);
  const cursor=value(first).nextCursor;
  const variations=[
    ["two",{query:'x',limit:1}],
    ["one",{query:'y',limit:1}],
    ["one",{query:'x',limit:2}],
    ["one",{query:'x',limit:1,include_schemas:true}],
  ];
  for(const [id,opts] of variations){
    const record=await run(`return await tools.search(${JSON.stringify(id)},${JSON.stringify({...opts,cursor})});`,
      ()=>assert.fail('invalid cursor must not reach host'));
    failure(record,/cursor does not match/);
    assert.equal(record.requests.length,0);
  }
});

test('resuming rechecks authorization and rejects a changed page including changed schema',async()=>{
  const native=descriptor('browser.a');
  const pages=new Map([[null,{tools:[native,descriptor('browser.b')]}]]);
  const first=await run("return await tools.search('one',{limit:1});",replyPages(pages));
  success(first);
  const code=`return await tools.search('one',{limit:1,cursor:${JSON.stringify(value(first).nextCursor)}});`;
  const denied=await run(code,request=>{
    assert.equal(request.op,'tools');
    return {ok:false,error:'client is offline or no longer authorized'};
  });
  failure(denied,/no longer authorized/);
  assert.equal(denied.requests.length,1);
  const changed=structuredClone(pages.get(null));
  changed.tools[0].inputSchema.properties.newField={type:'string'};
  const stale=await run(code,replyPages(new Map([[null,changed]])));
  failure(stale,/catalog changed.*restart/);
});

test('a later page authorization error is propagated without a partial success',async()=>{
  const record=await run("return await tools.search('one',{query:'browser'});",request=>{
    if(request.cursor===null)return {ok:true,value:{tools:[descriptor('ssmcp.browser.tabs')],nextCursor:'next'}};
    return {ok:false,error:'access policy revoked'};
  });
  failure(record,/access policy revoked/);
  assert.equal(value(record),undefined);
  assert.equal(record.requests.length,2);
});

test('page budget yields a continuation and detects cursor loops across calls',async()=>{
  const pages=new Map([
    [null,{tools:[descriptor('a.first')],nextCursor:'next'}],
    ['next',{tools:[descriptor('b.second')],nextCursor:'third'}],
    ['third',{tools:[descriptor('c.third')]}],
  ]);
  const first=await run("return await tools.search('one',{query:'absent',max_pages:1});",replyPages(pages));
  success(first);
  assert.equal(value(first).stopped_reason,'page_budget');
  assert.equal(value(first).stats.pages_read,1);
  const continued=await run(`return await tools.search('one',{query:'absent',max_pages:1,cursor:${JSON.stringify(value(first).nextCursor)}});`,replyPages(pages));
  success(continued);
  assert.equal(continued.requests[0].cursor,'next');
  const loop=await run(`return await tools.search('one',{query:'absent',cursor:${JSON.stringify(value(continued).nextCursor)}});`,
    replyPages(new Map([['third',{tools:[],nextCursor:'next'}]])));
  failure(loop,/repeated native cursor/);
  assert.equal(loop.requests.length,1);
});

test('malformed pages, self loops and invalid options fail with bounded requests',async()=>{
  for(const page of [
    {tools:[],nextCursor:42},
    {tools:Array.from({length:65},()=>descriptor('native.a'))},
    {tools:[{name:'invalid tool name'}]},
  ]){
    const record=await run("return await tools.search('one');",()=>({ok:true,value:page}));
    failure(record,/invalid native/);
    assert.equal(record.requests.length,1);
  }
  const loop=await run("return await tools.search('one');",request=>({ok:true,value:{tools:[],nextCursor:request.cursor??'loop'}}));
  failure(loop,/repeated native cursor/);
  assert.equal(loop.requests.length,2);
  for(const opts of [
    {limit:0},{limit:51},{max_pages:0},{max_pages:33},{include_schemas:'yes'},
    {query:'🦀'.repeat(65)},{query:null},{cursor:'bad json'},{unknown:true},
  ]){
    const record=await run(`return await tools.search('one',${JSON.stringify(opts)});`,()=>assert.fail('invalid options reached host'));
    failure(record,/tools.search/);
    assert.equal(record.requests.length,0);
  }
});

test('output budget returns a precise continuation without dropping native schema fields',async()=>{
  const make=n=>({...descriptor('native.'+n),inputSchema:{type:'object',description:'s'.repeat(60000)}});
  const pages=new Map([[null,{tools:[make('a'),make('b')]}]]);
  const first=await run("return await tools.search('one',{include_schemas:true});",replyPages(pages));
  success(first);
  assert.equal(value(first).stopped_reason,'output_budget');
  assert.deepEqual(value(first).tools,[make('a')]);
  assert(Buffer.byteLength(JSON.stringify(value(first)))<=128*1024);
  const second=await run(`return await tools.search('one',{include_schemas:true,cursor:${JSON.stringify(value(first).nextCursor)}});`,replyPages(pages));
  success(second);
  assert.deepEqual(value(second).tools,[make('b')]);
  assert.equal(value(second).complete,true);
});

test('an oversized single descriptor gives an explicit original-tools fallback instead of truncating',async()=>{
  const native={...descriptor('native.large'),inputSchema:{type:'string',description:'x'.repeat(100000)}};
  const record=await run("return await tools.search('one',{include_schemas:true});",
    replyPages(new Map([[null,{tools:[native]}]])));
  failure(record,/96 KiB item budget.*unchanged descriptor with tools/);
  assert.equal(record.requests.length,1);
});

test('a cursor chain stops at 128 distinct pages even across helper invocations',async()=>{
  const record=await run(`
    let cursor=null;
    for(let i=0;i<5;i++){
      const found=await tools.search('one',{query:'absent',max_pages:32,cursor});
      cursor=found.nextCursor;
      if(found.complete)return found;
    }
  `,request=>{
    const index=request.cursor===null?0:Number(request.cursor.slice(1));
    return {ok:true,value:{tools:[],nextCursor:'p'+(index+1)}};
  });
  failure(record,/exceeds 128 catalog pages/);
  assert.equal(record.requests.length,128);
});

test('the existing total native call budget still applies to search',async()=>{
  const record=await run(`
    for(let i=0;i<129;i++) await tools.search('one');
    return true;
  `,replyPages(new Map([[null,{tools:[]}]])));
  failure(record,/exceeds 128 native calls/);
  assert.equal(record.requests.length,128);
});

test('name_prefix filters registered names before large unrelated descriptions', async () => {
  const unrelated = descriptor('claude.Agent', 'ssmcp.exec. command execution ' + 'x'.repeat(100000));
  const start = descriptor('ssmcp.exec.start', 'Start a command');
  const read = descriptor('ssmcp.exec.read', 'Read command output');
  const pages = new Map([[null, {tools:[unrelated, start, read]}]]);
  const record = await run("return await tools.search('one',{name_prefix:'SSMCP.EXEC.',include_schemas:true});", replyPages(pages));
  success(record);
  assert.deepEqual(value(record).tools, [start, read], 'excluded descriptions must not consume the item budget');
  assert.equal(value(record).name_prefix, 'ssmcp.exec.');
  assert.equal(value(record).complete, true);
  assert.equal(value(record).stats.tools_scanned, 3);
});

test('name_prefix intersects the existing name or description query', async () => {
  const pages = new Map([[null, {tools:[
    descriptor('ssmcp.exec.start', 'Start an OUTPUT-producing command'),
    descriptor('ssmcp.exec.output_status', 'Inspect a command'),
    descriptor('ssmcp.exec.wait', 'Wait for completion'),
    descriptor('ssmcp.fs.read', 'Read output'),
  ]}]]);
  const record = await run("return await tools.search('one',{name_prefix:'ssmcp.exec.',query:' Output '});", replyPages(pages));
  success(record);
  assert.deepEqual(value(record).tools.map(item => item.name), ['ssmcp.exec.start', 'ssmcp.exec.output_status']);
});

test('filtered pagination resumes within and across pages without losing matches', async () => {
  const pages = new Map([
    [null, {tools:[descriptor('claude.exec'), descriptor('ssmcp.exec.start'), descriptor('ssmcp.exec.read')], nextCursor:'next'}],
    ['next', {tools:[descriptor('ssmcp.fs.read'), descriptor('ssmcp.exec.wait')], nextCursor:'last'}],
    ['last', {tools:[descriptor('ssmcp.exec.status')]}],
  ]);
  let cursor = null;
  const found = [];
  for (const expectedPage of [null, null, 'next', 'last']) {
    const record = await run(
      `return await tools.search('one',${JSON.stringify({name_prefix:'ssmcp.exec.',limit:1,cursor})});`,
      replyPages(pages));
    success(record);
    assert.equal(record.requests[0].cursor, expectedPage);
    const result = value(record);
    found.push(...result.tools.map(item => item.name));
    cursor = result.nextCursor;
    if (cursor) {
      const state = JSON.parse(cursor);
      assert.equal(state.v, 2);
      assert.equal(state.name_prefix, 'ssmcp.exec.');
    } else {
      assert.equal(result.complete, true);
      assert.equal(result.stats.total_tools_scanned, 6);
    }
  }
  assert.equal(cursor, null);
  assert.deepEqual(found, ['ssmcp.exec.start', 'ssmcp.exec.read', 'ssmcp.exec.wait', 'ssmcp.exec.status']);
});

test('filtered continuations bind the prefix and reject mismatches before contacting the host', async () => {
  const pages = new Map([[null, {tools:[descriptor('ssmcp.exec.start'), descriptor('ssmcp.exec.wait')]}]]);
  const first = await run("return await tools.search('one',{name_prefix:'ssmcp.exec.',limit:1});", replyPages(pages));
  success(first);
  const cursor = value(first).nextCursor;
  for (const name_prefix of [undefined, '', 'ssmcp.fs.', 'ssmcp.exec']) {
    const record = await run(
      `return await tools.search('one',${JSON.stringify({name_prefix,limit:1,cursor})});`,
      () => assert.fail('a different prefix must not reach the host'));
    failure(record, /cursor does not match/);
    assert.equal(record.requests.length, 0);
  }
  const resumed = await run(
    `return await tools.search('one',${JSON.stringify({name_prefix:'SSMCP.EXEC.',limit:1,cursor})});`,
    replyPages(pages));
  success(resumed);
  assert.deepEqual(value(resumed).tools.map(item => item.name), ['ssmcp.exec.wait']);
});

test('omitted and empty prefixes retain the unfiltered result and v1 cursor contract', async () => {
  const pages = new Map([[null, {tools:[descriptor('claude.exec'), descriptor('ssmcp.exec.start')]}]]);
  const omitted = await run("return await tools.search('one',{query:'exec',limit:1});", replyPages(pages));
  const empty = await run("return await tools.search('one',{query:'exec',name_prefix:'',limit:1});", replyPages(pages));
  success(omitted);
  success(empty);
  assert.deepEqual(value(empty), value(omitted));
  const cursor = value(omitted).nextCursor;
  assert.equal(JSON.parse(cursor).v, 1);
  assert(!Object.hasOwn(JSON.parse(cursor), 'name_prefix'));
  const resumed = await run(
    `return await tools.search('one',${JSON.stringify({query:'exec',name_prefix:'',limit:1,cursor})});`,
    replyPages(pages));
  success(resumed);
  assert.deepEqual(value(resumed).tools.map(item => item.name), ['ssmcp.exec.start']);
  const filtered = await run(
    `return await tools.search('one',${JSON.stringify({query:'exec',name_prefix:'ssmcp.',limit:1,cursor})});`,
    () => assert.fail('a v1 cursor cannot be reused with a new filter'));
  failure(filtered, /cursor does not match/);
  assert.equal(filtered.requests.length, 0);
});

test('name_prefix validates type, length and registered-name characters before host requests', async () => {
  for (const name_prefix of [null, true, 1, [], {}, 'x'.repeat(129), '界', 'ssmcp.exec.*', 'ssmcp exec', ' ssmcp.']) {
    const record = await run(
      `return await tools.search('one',${JSON.stringify({name_prefix})});`,
      () => assert.fail('invalid prefix reached the host'));
    failure(record, /name_prefix/);
    assert.equal(record.requests.length, 0);
  }
  const name_prefix = 'x'.repeat(128);
  const record = await run(
    `return await tools.search('one',${JSON.stringify({name_prefix})});`,
    replyPages(new Map([[null, {tools:[descriptor(name_prefix)]}]])));
  success(record);
  assert.equal(value(record).tools[0].name, name_prefix);
});
