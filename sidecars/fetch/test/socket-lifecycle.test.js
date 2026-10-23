import assert from 'node:assert/strict';
import fs from 'node:fs';
import net from 'node:net';
import path from 'node:path';
import { test } from 'node:test';
import { FetchSocketServer } from '../dist/server.js';


test('a second fetch server cannot replace an active private socket', async () => {
  const root = fs.mkdtempSync(path.join('/tmp', 'jarvis-fetch-socket-'));
  fs.chmodSync(root, 0o700);
  const previous = process.env.FETCH_SOCKET;
  process.env.FETCH_SOCKET = path.join(root, 'fetch.sock');
  const first = new FetchSocketServer({
    fetch: async ({ url }) => ({ url, markdown: '# Synthetic page',
                               layer_used: 'http', attempts: [], elapsed_ms: 1 }),
  });
  const second = new FetchSocketServer({});
  try {
    await first.listen();
    const inode = fs.statSync(process.env.FETCH_SOCKET).ino;
    assert.equal(fs.statSync(process.env.FETCH_SOCKET).mode & 0o777, 0o600);
    await assert.rejects(() => second.listen(), { code: 'EADDRINUSE' });
    await second.close();
    assert.equal(fs.statSync(process.env.FETCH_SOCKET).ino, inode);
    const response = await new Promise((resolve, reject) => {
      const connection = net.createConnection(process.env.FETCH_SOCKET);
      connection.setEncoding('utf8');
      connection.once('error', reject);
      connection.once('data', (data) => { connection.destroy(); resolve(JSON.parse(data)); });
      connection.once('connect', () => connection.write(JSON.stringify({ request_id: 'test', op: 'ping' }) + '\n'));
    });
    assert.equal(response.result.pong, true);
    const page = await new Promise((resolve, reject) => {
      const connection = net.createConnection(process.env.FETCH_SOCKET);
      connection.setEncoding('utf8');
      connection.once('error', reject);
      connection.once('data', (data) => { connection.destroy(); resolve(JSON.parse(data)); });
      connection.once('connect', () => connection.write(JSON.stringify({
        request_id: 'page', op: 'fetch', params: { url: 'https://example.invalid/page' },
      }) + '\n'));
    });
    assert.equal(page.result.markdown, '# Synthetic page');
    assert.equal(page.result.url, 'https://example.invalid/page');
  } finally {
    await first.close();
    if (previous === undefined) delete process.env.FETCH_SOCKET;
    else process.env.FETCH_SOCKET = previous;
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test('fetch server refuses a public socket directory', async () => {
  const root = fs.mkdtempSync(path.join('/tmp', 'jarvis-fetch-public-'));
  fs.chmodSync(root, 0o755);
  const previous = process.env.FETCH_SOCKET;
  process.env.FETCH_SOCKET = path.join(root, 'fetch.sock');
  try {
    await assert.rejects(() => new FetchSocketServer({}).listen(), /owner-private/);
    assert.equal(fs.existsSync(process.env.FETCH_SOCKET), false);
  } finally {
    if (previous === undefined) delete process.env.FETCH_SOCKET;
    else process.env.FETCH_SOCKET = previous;
    fs.rmSync(root, { recursive: true, force: true });
  }
});
