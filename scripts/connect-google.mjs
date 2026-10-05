#!/usr/bin/env node
// CLI OAuth for multiple Google accounts; no dashboard callback is required.
import { randomUUID } from 'node:crypto';
import { parseArgs } from 'node:util';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import { createRequire } from 'node:module';
import dotenv from 'dotenv';
import Composio from '@composio/client';

const PREFIX = 'google_cli_pending:';
export async function startConnection(client, store, toolkit, expectedEmail) {
  if (!['googledrive', 'gmail'].includes(toolkit)) throw new Error('toolkit must be googledrive or gmail');
  const cacheKey = `auth_config_${toolkit}`;
  let authConfig = store.getConfig(cacheKey);
  if (!authConfig) {
    const configs = await client.authConfigs.list({ toolkit_slug: toolkit, is_composio_managed: true });
    authConfig = configs.items[0]?.id;
    if (!authConfig) {
      const created = await client.authConfigs.create({ toolkit: { slug: toolkit },
        auth_config: { type: 'use_composio_managed_auth', name: `augmentagent-${toolkit}`, credentials: {} } });
      authConfig = created.auth_config?.id || created.id;
    }
    if (!authConfig) throw new Error('Composio returned no auth configuration');
    store.setConfig(cacheKey, authConfig);
  }
  const userId = `augmentagent-cli-${randomUUID()}`;
  const link = await client.link.create({ user_id: userId, auth_config_id: authConfig });
  if (!link.connected_account_id || !link.redirect_url) throw new Error('Composio returned an incomplete connection link');
  const pending = { id: link.connected_account_id, userId, toolkit, expectedEmail: expectedEmail || null };
  store.setConfig(PREFIX + pending.id, JSON.stringify(pending));
  return { ...pending, url: link.redirect_url };
}

export async function finishConnection(client, store, pending) {
  const account = await client.connectedAccounts.retrieve(pending.id);
  if (account.user_id !== pending.userId || account.toolkit?.slug !== pending.toolkit) {
    throw new Error('Connection does not match its pending user and toolkit');
  }
  if (account.status !== 'ACTIVE') return { id: pending.id, toolkit: pending.toolkit, status: account.status };
  const drive = pending.toolkit === 'googledrive';
  const profile = await client.tools.execute(drive ? 'GOOGLEDRIVE_GET_ABOUT' : 'GMAIL_GET_PROFILE', {
    connected_account_id: pending.id, user_id: pending.userId,
    version: drive ? '20261001_00' : '20260915_00',
    arguments: drive ? { fields: 'user(emailAddress)' } : { user_id: 'me' },
  });
  if (profile.successful !== true) throw new Error('Could not verify the Google account profile');
  const data = profile.data?.response_data || profile.data;
  const email = drive ? data?.user?.emailAddress : data?.emailAddress;
  if (typeof email !== 'string' || !email.includes('@')) throw new Error('Google profile returned no email address');
  if (pending.expectedEmail && email.toLowerCase() !== pending.expectedEmail.toLowerCase()) {
    throw new Error(`Signed in as ${email}; expected ${pending.expectedEmail}. Account was not added.`);
  }
  // Avoid replacing an already connected mailbox or introducing duplicate polling.
  const existing = (drive ? store.getActiveDriveAccounts() : store.getActiveGmailAccounts())
    .find(a => a.email?.toLowerCase() === email.toLowerCase());
  if (!existing) {
    (drive ? store.addDriveAccount : store.addGmailAccount)(pending.id, pending.userId, email, email);
  }
  store.deleteConfig(PREFIX + pending.id);
  return { id: pending.id, toolkit: pending.toolkit, email, status: existing ? 'ALREADY_CONNECTED' : 'CONNECTED' };
}

async function main() {
  const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
  dotenv.config({ path: path.join(repo, '.env'), quiet: true });
  const { values, positionals } = parseArgs({ allowPositionals: true, options: {
    toolkit: { type: 'string' }, email: { type: 'string' }, count: { type: 'string', default: '1' },
    help: { type: 'boolean', default: false },
  } });
  const command = positionals[0];
  if (values.help || !command) {
    console.log('Usage: node scripts/connect-google.mjs start --toolkit googledrive|gmail [--email address] [--count 4]\n       node scripts/connect-google.mjs finish\n       node scripts/connect-google.mjs status\nOpen the printed links, choose the intended Google accounts, then run finish to verify and save them.');
    return;
  }
  if (!['start', 'finish', 'status'].includes(command)) throw new Error('Unknown command');
  const require = createRequire(import.meta.url);
  const store = require('../dist/db.js');
  const database = store.initDb(process.env.AUGMENTAGENT_DB || path.join(repo, 'data.db'));
  try {
    if (command === 'status') {
      console.log(JSON.stringify({ drive: store.getActiveDriveAccounts().map(a => ({ email: a.email, id: a.connection_id })),
        gmail: store.getActiveGmailAccounts().map(a => ({ email: a.email, id: a.connectionId })),
        pending: database.prepare('SELECT value FROM config WHERE key LIKE ?').all(PREFIX + '%').map(r => JSON.parse(r.value)),
      }, null, 2));
      return;
    }
    const apiKey = store.getConfig('composio_api_key') || process.env.COMPOSIO_API_KEY;
    if (!apiKey) throw new Error('COMPOSIO_API_KEY is not configured');
    const client = new Composio({ apiKey });
    if (command === 'start') {
      const count = Number(values.count);
      if (!Number.isInteger(count) || count < 1 || count > 10 || (values.email && count !== 1)) {
        throw new Error('count must be 1–10; use --email only with a single connection');
      }
      for (let i = 0; i < count; i++) {
        console.log(JSON.stringify(await startConnection(client, store, values.toolkit, values.email)));
      }
    } else {
      const rows = database.prepare('SELECT value FROM config WHERE key LIKE ?').all(PREFIX + '%');
      for (const row of rows) {
        const pending = JSON.parse(row.value);
        try { console.log(JSON.stringify(await finishConnection(client, store, pending))); }
        catch (error) { console.error(`${pending.id}: ${error.message}`); process.exitCode = 1; }
      }
      if (!rows.length) console.log('No pending CLI connections.');
    }
  } finally { database.close(); }
}
if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch(error => { console.error(error.message); process.exitCode = 1; });
}
