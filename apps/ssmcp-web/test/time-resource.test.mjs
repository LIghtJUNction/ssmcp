import test from 'node:test';
import assert from 'node:assert/strict';
import { capabilities, harness, response } from './fixtures.mjs';
import { waitFixture } from './wait-fixtures.mjs';

test('advertised time snapshot is refreshed without commands or acknowledgements', async () => {
  const { client, calls } = harness({ caps: capabilities({ resources: ['runtime', 'time'], commands: [], mode: 'observe' }),
    bridge(request) {
      if (request.operation.op === 'state' && request.operation.resource === 'time') {
        return response(request, { resource: 'time', schema_version: 1, value: waitFixture });
      }
    } });
  await client.refreshStates({ includeHistory: false });
  assert.deepEqual(client.states.get('time'), waitFixture);
  const requests = calls.filter(call => call[0] === 'bridge').map(call => call[1].operation);
  assert.equal(requests.filter(operation => operation.resource === 'time').length, 1);
  assert.ok(requests.every(operation => operation.op !== 'command'));
  assert.equal(client.ledger.records.length, 0);
});

test('old and unrecognized time capabilities remain unavailable instead of fabricating an empty snapshot', async () => {
  for (const version of [null, 2]) {
    const caps = capabilities({ resources: ['runtime'], commands: [] });
    if (version !== null) caps.resources.push({ id: 'time', schema_version: version, description: 'Unknown future schema' });
    const { client, calls } = harness({ caps });
    await client.refreshStates({ includeHistory: false });
    assert.equal(client.states.has('time'), false);
    assert.equal(client.hasResource('time'), false);
    assert.ok(calls.every(call => call[0] !== 'bridge' || call[1].operation.resource !== 'time'));
  }
});
