const assert = require('node:assert/strict');
const { test } = require('node:test');
const { tarit } = require('../dist/index.js');

const snapshots = [
  { snapshot_id: 'other-first', source_vm_id: 'other-vm', created_at: '2026-09-01T00:00:00Z' },
  { snapshot_id: 'wanted-first', source_vm_id: 'wanted-vm', created_at: '2026-09-02T00:00:00Z' },
  { snapshot_id: 'wanted-second', source_vm_id: 'wanted-vm', created_at: '2026-09-03T00:00:00Z' },
];

test('snapshot list honors instance filters and applies limit after filtering', async (t) => {
  t.mock.method(globalThis, 'fetch', async (url, init) => {
    assert.equal(url, 'https://tarit.test/v1/snapshots');
    assert.equal(init.method, 'GET');
    assert.equal(init.headers['X-API-Key'], 'tenant-key');
    return new Response(JSON.stringify(snapshots), { status: 200 });
  });
  const provider = tarit({ baseUrl: 'https://tarit.test', apiKey: 'tenant-key' });
  const ids = async (options) => (await provider.snapshot.list(options)).map((snapshot) => snapshot.id);
  assert.deepEqual(await ids(), ['other-first', 'wanted-first', 'wanted-second']);
  assert.deepEqual(await ids({ instanceId: 'wanted-vm' }), ['wanted-first', 'wanted-second']);
  assert.deepEqual(await ids({ instanceId: 'wanted-vm', limit: 1 }), ['wanted-first']);
  assert.deepEqual(await ids({ sandboxId: 'wanted-vm', limit: 1 }), ['wanted-first']);
  assert.deepEqual(await ids({ instanceId: 'wanted-vm', sandboxId: 'other-vm' }), ['wanted-first', 'wanted-second']);
  assert.deepEqual(await ids({ instanceId: 'missing' }), []);
  assert.deepEqual(await ids({ limit: 0 }), []);
  assert.deepEqual(await ids({ limit: 1 }), ['other-first']);
  for (const limit of [-1, 0.5, NaN, Infinity]) {
    await assert.rejects(ids({ limit }), RangeError);
  }
});
