const assert = require('node:assert/strict');
const { test } = require('node:test');
const { tarit } = require('../dist/index.js');
const config = { baseUrl: 'https://tarit.test', apiKey: 'tenant-key', shareDomain: 'shares.test' };
const share = (extra = {}) => ({ slug: 'existing', vm_id: 'vm', guest_port: 8080, visibility: 'public', revoked_at: null, ...extra });
const json = (body, status = 200) => new Response(JSON.stringify(body), { status });
async function vm(options = {}) { return tarit({ ...config, ...options }).instances.getById('vm'); }

test('getUrl reuses active public shares and checks revocation on later calls', async (t) => {
  let current = [share()]; let creates = 0; let lists = 0;
  t.mock.method(globalThis, 'fetch', async (url, init) => {
    if (url.endsWith('/v1/vms/vm')) return json({ id: 'vm', status: 'running' });
    assert.ok(url.endsWith('/v1/shares'));
    if (init.method === 'GET') { lists++; return json(current); }
    creates++; current.push(share({ slug: 'replacement' })); return json(current.at(-1));
  });
  const instance = await vm();
  assert.equal(await instance.getUrl({ port: 8080 }), 'https://existing.shares.test');
  current[0].revoked_at = '2026-11-10T00:00:00Z';
  assert.equal(await instance.getUrl({ port: 8080, protocol: 'http' }), 'http://replacement.shares.test');
  assert.equal(await instance.getUrl({ port: 8080 }), 'https://replacement.shares.test');
  assert.equal(creates, 1); assert.equal(lists, 3);
});

test('concurrent same-identity providers coalesce lookup and creation; no private/revoked reuse', async (t) => {
  let lists = 0; let creates = 0;
  const current = [share({ visibility: 'private' }), share({ revoked_at: 'now' }), share({ vm_id: 'other' }), share({ guest_port: 80 })];
  t.mock.method(globalThis, 'fetch', async (url, init) => {
    if (url.endsWith('/v1/vms/vm')) return json({ id: 'vm', status: 'running' });
    if (init.method === 'GET') { lists++; await new Promise(resolve => setImmediate(resolve)); return json(current); }
    assert.deepEqual(JSON.parse(init.body), { vm_id: 'vm', guest_port: 8080, visibility: 'public' });
    creates++; const created = share({ slug: 'created' }); current.push(created); return json(created);
  });
  const instances = await Promise.all([vm(), vm()]);
  const urls = await Promise.all(Array.from({ length: 50 }, (_, i) => instances[i % 2].getUrl({ port: 8080 })));
  assert.ok(urls.every(url => url === 'https://created.shares.test'));
  assert.equal(lists, 1); assert.equal(creates, 1);
  await instances[0].getUrl({ port: 8080 });
  assert.equal(lists, 2); assert.equal(creates, 1);
});

test('lookup failures do not create; rejected operations are cleared and ambiguous creates recovered by lookup', async (t) => {
  let phase = 'lookup-error'; let creates = 0; const current = [];
  t.mock.method(globalThis, 'fetch', async (url, init) => {
    if (url.endsWith('/v1/vms/vm')) return json({ id: 'vm', status: 'running' });
    if (init.method === 'GET') return phase === 'lookup-error' ? json({}, 503) : json(current);
    creates++; current.push(share({ slug: 'ambiguous' })); throw new Error('response lost');
  });
  const instance = await vm();
  await assert.rejects(instance.getUrl({ port: 8080 }), /503/); assert.equal(creates, 0);
  phase = 'create';
  await assert.rejects(instance.getUrl({ port: 8080 }), /response lost/);
  assert.equal(await instance.getUrl({ port: 8080 }), 'https://ambiguous.shares.test');
  assert.equal(creates, 1);
});

test('different API keys, URLs and ports never share outstanding lookups', async (t) => {
  const lookups = [];
  t.mock.method(globalThis, 'fetch', async (url, init) => {
    if (url.endsWith('/v1/vms/vm')) return json({ id: 'vm', status: 'running' });
    assert.equal(init.method, 'GET'); lookups.push([url, init.headers['X-API-Key']]);
    await new Promise(resolve => setImmediate(resolve));
    return json([share(), share({ guest_port: 8081 })]);
  });
  const [a, b, c] = await Promise.all([vm(), vm({ apiKey: 'other-key' }), vm({ baseUrl: 'https://other.test' })]);
  await Promise.all([a.getUrl({ port: 8080 }), a.getUrl({ port: 8081 }), b.getUrl({ port: 8080 }), c.getUrl({ port: 8080 })]);
  assert.equal(lookups.length, 4);
});
