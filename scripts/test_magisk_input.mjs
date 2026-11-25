// Exercise the same writer used by the Magisk WebUI without an Android manager.
import assert from 'node:assert/strict';

globalThis.location = { protocol: 'file:', hostname: '' };
const { TerminalWriter } = await import('../packaging/magisk/src/ssmcp/webroot/device-api.js');
const encoder = new TextEncoder();
function fixture() {
  let release, fail, idle = 0, uncertain = null;
  const parts = [];
  const writer = new TerminalWriter(text => {
    parts.push(text);
    return new Promise((resolve, reject) => { release = resolve; fail = reject; });
  }, (chunk, unsent) => { uncertain = { chunk, unsent }; }, () => idle++);
  return { writer, parts, release: () => release(), fail: () => fail(Error('no receipt')),
    get idle() { return idle; }, get uncertain() { return uncertain; } };
}
const tick = () => new Promise(resolve => setTimeout(resolve, 0));

{
  const f = fixture();
  const text = 'x'.repeat(4095) + '💡' + '你'.repeat(350000) + '\uFEFF';
  assert.ok(f.writer.append(text));
  assert.equal(f.parts.length, 1);
  assert.equal(f.parts[0].length, 4095);
  assert.ok(f.writer.append('a'));
  assert.ok(f.writer.append('\r'));
  assert.ok(f.writer.append('\x1b[A'));
  const done = f.writer.done;
  const other = fixture();
  assert.ok(other.writer.append('另一个会话'));
  other.release();
  await other.writer.done;
  assert.deepEqual(other.parts, ['另一个会话']);
  while (f.writer.done) { f.release(); await tick(); }
  await done;
  assert.equal(f.parts.join(''), text + 'a\r\x1b[A');
  assert.ok(f.parts.every(s => encoder.encode(s).length <= 4096));
  assert.equal(f.idle, 1);
  assert.equal(f.uncertain, null);
}

{
  const f = fixture();
  f.writer.append('A'.repeat(4096) + 'B'.repeat(4096) + '尚未发送');
  f.writer.append('追加入队');
  const done = f.writer.done;
  f.release(); await tick(); f.fail(); await done;
  assert.equal(f.parts.length, 2);
  assert.deepEqual(f.uncertain, { chunk: 'B'.repeat(4096), unsent: '尚未发送追加入队' });
  assert.equal(f.idle, 1);
}

{
  const f = fixture();
  f.writer.append('first');
  const done = f.writer.done;
  assert.ok(f.writer.append('x'.repeat(65536)));
  assert.equal(f.writer.append('拒绝整次输入'), false);
  assert.equal(f.writer.done, done);
  assert.equal(f.parts.length, 1);
  f.fail(); await done;
  assert.deepEqual(f.uncertain, { chunk: 'first', unsent: 'x'.repeat(65536) });
}
{
  const f = fixture();
  f.writer.append('key in flight');
  const done = f.writer.done;
  f.writer.append('before paste');
  const paste = 'a'.repeat(65537);
  assert.ok(f.writer.append(paste));
  assert.ok(f.writer.append('after paste'));
  assert.equal(f.writer.append(paste), false);
  while (f.writer.done) { f.release(); await tick(); }
  await done;
  assert.equal(f.parts.join(''), 'key in flightbefore paste' + paste + 'after paste');
}
console.log('PASS large paste, UTF-8 boundaries, independent tabs, shortcuts, backpressure and failure suffix');
