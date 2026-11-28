// #2705 — D8: the dashboard refuses to bind a non-loopback host unless
// AUGMENTAGENT_ALLOW_PUBLIC_BIND=1 is set.
const test = require('node:test');
const assert = require('node:assert/strict');
const { getBindHost } = require('../dist/security');

function withEnv(env, fn) {
  const keys = ['AUGMENTAGENT_BIND_HOST', 'AUGMENTAGENT_ALLOW_PUBLIC_BIND'];
  const saved = Object.fromEntries(keys.map((k) => [k, process.env[k]]));
  for (const k of keys) {
    if (env[k] === undefined) delete process.env[k];
    else process.env[k] = env[k];
  }
  try { return fn(); } finally {
    for (const k of keys) {
      if (saved[k] === undefined) delete process.env[k];
      else process.env[k] = saved[k];
    }
  }
}

test('unset bind host defaults to 127.0.0.1', () => {
  assert.equal(withEnv({}, getBindHost), '127.0.0.1');
  assert.equal(withEnv({ AUGMENTAGENT_BIND_HOST: '' }, getBindHost), '127.0.0.1');
});

test('loopback hosts are accepted', () => {
  for (const host of ['127.0.0.1', '127.0.0.2', '127.255.255.254', '::1', 'localhost', 'LOCALHOST']) {
    assert.equal(withEnv({ AUGMENTAGENT_BIND_HOST: host }, getBindHost), host);
  }
});

test('non-loopback hosts are refused', () => {
  for (const host of ['0.0.0.0', '::', '192.168.1.10', '10.0.0.1', '128.0.0.1', '127.0.0.256', '127.1', 'localhost.example', 'jarvis.example']) {
    assert.throws(() => withEnv({ AUGMENTAGENT_BIND_HOST: host }, getBindHost), /AUGMENTAGENT_ALLOW_PUBLIC_BIND/, host);
  }
  assert.throws(() => withEnv({ AUGMENTAGENT_BIND_HOST: '0.0.0.0', AUGMENTAGENT_ALLOW_PUBLIC_BIND: 'true' }, getBindHost));
});

test('AUGMENTAGENT_ALLOW_PUBLIC_BIND=1 overrides the guard', () => {
  assert.equal(withEnv({ AUGMENTAGENT_BIND_HOST: '0.0.0.0', AUGMENTAGENT_ALLOW_PUBLIC_BIND: '1' }, getBindHost), '0.0.0.0');
});
