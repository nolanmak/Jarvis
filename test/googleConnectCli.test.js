const test = require('node:test');
const assert = require('node:assert/strict');

function fixture() {
  const config = new Map([['auth_config_googledrive', 'ac_drive'], ['auth_config_gmail', 'ac_gmail']]);
  const saved = [];
  return { config, saved, store: {
    getConfig: k => config.get(k), setConfig: (k, v) => config.set(k, v), deleteConfig: k => config.delete(k),
    getActiveDriveAccounts: () => [], getActiveGmailAccounts: () => [],
    addDriveAccount: (...args) => saved.push(['drive', ...args]), addGmailAccount: (...args) => saved.push(['gmail', ...args]),
  } };
}
test('CLI starts preserve distinct pending connections for four Drive accounts', async () => {
  const { startConnection } = await import('../scripts/connect-google.mjs');
  const { store, config } = fixture();
  let number = 0;
  const client = { link: { create: async () => ({ connected_account_id: `ca_${++number}`, redirect_url: 'https://connect.composio.dev/test' }) } };
  const attempts = [];
  for (let i = 0; i < 4; i++) attempts.push(await startConnection(client, store, 'googledrive'));
  assert.equal(new Set(attempts.map(a => a.userId)).size, 4);
  assert.equal([...config.keys()].filter(k => k.startsWith('google_cli_pending:')).length, 4);
});
test('CLI verifies identity, rejects a different email, and saves both toolkits correctly', async () => {
  const { finishConnection } = await import('../scripts/connect-google.mjs');
  for (const toolkit of ['gmail', 'googledrive']) {
    const { store, saved } = fixture();
    const pending = { id: 'ca_test', userId: 'u_test', toolkit, expectedEmail: 'owner@example.com' };
    let status = 'INITIATED';
    let email = 'wrong@example.com';
    const client = {
      connectedAccounts: { retrieve: async () => ({ status, user_id: 'u_test', toolkit: { slug: toolkit } }) },
      tools: { execute: async (_slug, args) => {
        assert.equal(args.connected_account_id, pending.id);
        return { successful: true, data: toolkit === 'gmail' ? { emailAddress: email } : { user: { emailAddress: email } } };
      } },
    };
    assert.equal((await finishConnection(client, store, pending)).status, 'INITIATED');
    assert.equal(saved.length, 0);
    status = 'ACTIVE';
    await assert.rejects(finishConnection(client, store, pending), /expected owner@example.com/);
    assert.equal(saved.length, 0);
    email = 'owner@example.com';
    assert.equal((await finishConnection(client, store, pending)).status, 'CONNECTED');
    assert.equal(saved[0][0], toolkit === 'gmail' ? 'gmail' : 'drive');
    assert.equal(saved[0][3], email);
    client.connectedAccounts.retrieve = async () => ({ status, user_id: 'wrong_user', toolkit: { slug: toolkit } });
    await assert.rejects(finishConnection(client, store, pending), /pending user and toolkit/);
  }
});
