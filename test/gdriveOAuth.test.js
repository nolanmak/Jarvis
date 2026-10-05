const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');

// Execute the compiled route with isolated Composio and store dependencies.
const source = fs.readFileSync(require.resolve('../dist/dashboard'), 'utf8');
const start = source.indexOf('router.get("/oauth/googledrive/callback"');
const end = source.indexOf('router.get("/api/oauth/googledrive/status"', start);
assert(start >= 0 && end > start);
async function callback({ status = 'ACTIVE', user = 'u1', pending = true, successful = true } = {}) {
  let handler;
  let redirect;
  const saved = [];
  const config = pending ? { gdrive_pending_connection_id: 'ca_1', gdrive_pending_entity_id: 'u1' } : {};
  const context = {
    router: { get(_path, fn) { handler = fn; } },
    console: { log() {}, error() {} },
    setTimeout(fn) { fn(); },
    db_1: {
      getConfig: key => config[key],
      deleteConfig: key => delete config[key],
      addDriveAccount: (...args) => saved.push(args),
    },
    getComposioClient: () => ({
      connectedAccounts: { retrieve: async () => ({ status, user_id: user, toolkit: { slug: 'googledrive' } }) },
      tools: { execute: async (slug, args) => {
        assert.equal(slug, 'GOOGLEDRIVE_GET_ABOUT');
        assert.equal(args.connected_account_id, 'ca_1');
        return { successful, data: { user: { emailAddress: 'owner@example.com' } } };
      } },
    }),
  };
  vm.runInNewContext(source.slice(start, end), context);
  await handler({ query: {} }, { redirect(url) { redirect = url; } });
  return { redirect, saved, config };
}

test('Drive callback saves only a verified active connection and its profile email', async () => {
  const result = await callback();
  assert.equal(result.redirect, '/settings?googledrive=connected');
  assert.deepEqual(result.saved[0], ['ca_1', 'u1', 'owner@example.com', 'owner@example.com']);
  assert.equal(result.config.gdrive_pending_connection_id, undefined);
});
test('Drive callback refuses inactive, mismatched, missing, and failed-profile connections', async () => {
  for (const scenario of [{ status: 'FAILED' }, { user: 'another-user' }, { pending: false }, { successful: false }]) {
    const result = await callback(scenario);
    assert.equal(result.redirect, '/settings?googledrive=error');
    assert.equal(result.saved.length, 0);
  }
});
