import test from 'node:test';
import assert from 'node:assert/strict';
import { projectEvent, projectHistory, quoteReply } from '../src/presentation.mjs';
import { Timeline } from '../src/store.mjs';
const frame = value => ({ sequence: '4', topic: 'message', value: { kind: 'event', source: { kind: 'thread', agent_id: 'agent_a' }, value } });
test('human and generated inputs retain provenance on the same side', () => {
  for (const origin of ['system_hook', 'agent_timer', 'worker_event', 'maintenance']) {
    const row = projectEvent(frame({ type: 'ssmcp.message', kind: 'input', role: 'tool', input_origin: origin, message_id: 'message_1', text: 'visible' }), 'generation');
    assert.equal(row.inputSide, true); assert.equal(row.inputOrigin, origin); assert.equal(row.messageId, 'message_1');
    assert.equal(row.source.agent_id, 'agent_a');
  }
  const human = projectEvent(frame({ type: 'ssmcp.user_input', message_id: 'message_human', text: 'question' }), 'generation');
  assert.equal(human.inputSide, true); assert.equal(human.inputOrigin, 'human');
});
test('history keeps input identity after bounded payload truncation', () => {
  const row = projectHistory({ sequence: '4', kind: 'input', text: 'retained excerpt', message_id: 'message_1', input_side: true,
    input_origin: 'agent_timer', value: { type: 'ssmcp.message', truncated: true } }, 'generation');
  assert.equal(row.messageId, 'message_1'); assert.equal(row.inputSide, true); assert.equal(row.inputOrigin, 'agent_timer');
  const spoof = projectEvent(frame({ type: 'ssmcp.tool_call', message_id: 'message_fake', kind: 'input', text: 'output' }), 'generation');
  assert.equal(spoof.messageId, null); assert.equal(spoof.inputSide, false);
});
test('editable reply retains the whole draft and bounds UTF-8 without splitting characters', () => {
  const draft = '已有回答 🦀';
  const value = quoteReply('message_original', '中文🦀\n'.repeat(900), draft, 4096);
  assert.ok(value.startsWith('> 回复消息 message_original\n> 中文🦀'));
  assert.ok(value.includes('引用已截断')); assert.ok(value.endsWith(draft));
  assert.ok(new TextEncoder().encode(value).length <= 4096);
  assert.equal(new TextDecoder().decode(new TextEncoder().encode(value)), value);
  assert.throws(() => quoteReply('bad\nid', 'text', ''));
  assert.throws(() => quoteReply('message_1', 'text', 'x'.repeat(4096), 4096));
});
test('queue and native input share a logical row without merging different recipients', () => {
  const timeline = new Timeline();
  timeline.put({ id: 'queue:g:m', generation: 'g', messageId: 'message_1', inputSide: true, source: { kind: 'local_queue' }, metadata: { target: 'agent_a' }, text: 'question' });
  timeline.put({ id: 'event:g:4', generation: 'g', sequence: '4', messageId: 'message_1', inputSide: true, source: { kind: 'local_human', recipient_agent_id: 'agent_a' }, text: 'question' });
  assert.equal(timeline.rows.length, 1); assert.equal(timeline.rows[0].sequence, '4');
  timeline.put({ id: 'queue:g:m', generation: 'g', messageId: 'message_1', inputSide: true, source: { kind: 'local_queue' }, metadata: { target: 'agent_a' }, status: 'delivered', text: 'question' });
  assert.equal(timeline.rows.length, 1); assert.equal(timeline.rows[0].source.kind, 'local_human');
  timeline.put({ id: 'event:g:5', generation: 'g', sequence: '5', messageId: 'message_1', inputSide: true, source: { kind: 'thread', agent_id: 'agent_b' }, text: 'other' });
  assert.equal(timeline.rows.length, 2);
});
