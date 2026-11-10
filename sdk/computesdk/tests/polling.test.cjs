const assert = require('node:assert/strict');
const { test } = require('node:test');
const { tarit, TaritDeadlineExceeded } = require('../dist/index.js');
const config = { baseUrl: 'https://tarit.test', apiKey: 'key', commandTimeoutMs: 180000 };
const record = { id: 'vm', status: 'running' };
const execution = status => ({ id: 'exec', status, exit_code: status === 'completed' ? 0 : null, stdout: 'done' });
const json = (body, status = 200) => new Response(JSON.stringify(body), { status });

test('command polling retries transient GET responses without resubmitting', async t => {
  const statuses = [429, 502, 503, 504, 200];
  const methods = [];
  t.mock.method(globalThis, 'fetch', async (url, init) => {
    methods.push(init.method);
    if (url.endsWith('/v1/vms/vm')) return json(record);
    if (init.method === 'POST') return json(execution('pending'), 202);
    assert.ok(url.endsWith('/v1/executions/exec'));
    const status = statuses.shift();
    return status === 200 ? json(execution('completed')) : json({ error: 'busy' }, status);
  });
  const vm = await tarit(config).sandbox.getById('vm');
  assert.equal((await vm.runCommand('once')).stdout, 'done');
  assert.equal(methods.filter(method => method === 'POST').length, 1);
  assert.equal(statuses.length, 0);
});

test('poll authorization errors stay terminal', async t => {
  let polls = 0;
  t.mock.method(globalThis, 'fetch', async (url, init) => {
    if (url.endsWith('/v1/vms/vm')) return json(record);
    if (init.method === 'POST') return json(execution('pending'), 202);
    polls++;
    return json({ error: 'forbidden' }, 403);
  });
  const vm = await tarit(config).sandbox.getById('vm');
  await assert.rejects(vm.runCommand('once'), error => error.status === 403);
  assert.equal(polls, 1);
});

test('poll overload respects the original deadline and retains execution ID', async t => {
  let now = 0;
  t.mock.method(performance, 'now', () => now);
  let posts = 0;
  t.mock.method(globalThis, 'fetch', async (url, init) => {
    if (url.endsWith('/v1/vms/vm')) return json(record);
    if (init.method === 'POST') { posts++; return json(execution('pending'), 202); }
    now = 185001;
    return json({ error: 'busy' }, 503);
  });
  const vm = await tarit(config).sandbox.getById('vm');
  await assert.rejects(vm.runCommand('once'), error => error instanceof TaritDeadlineExceeded && error.executionId === 'exec');
  assert.equal(posts, 1);
});

test('command deadline aborts a stalled request and does not replay submission', async t => {
  let posts = 0;
  let aborted = false;
  t.mock.method(globalThis, 'fetch', async (url, init) => {
    if (url.endsWith('/v1/vms/vm')) return json(record);
    posts++;
    return new Promise((_resolve, reject) => init.signal.addEventListener('abort', () => {
      aborted = true;
      reject(init.signal.reason);
    }, { once: true }));
  });
  const vm = await tarit(config).sandbox.getById('vm');
  await assert.rejects(vm.runCommand('once', { timeout: 1 }), error => error instanceof TaritDeadlineExceeded && error.executionId === undefined);
  assert.equal(posts, 1);
  assert.ok(aborted);
});
