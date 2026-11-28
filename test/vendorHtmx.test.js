const test = require('node:test');
const assert = require('node:assert/strict');
const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');
const { contentSecurityPolicy } = require('../dist/security');

const root = path.join(__dirname, '..');
const vendorDir = path.join(root, 'public', 'vendor');

function cspHeader() {
  const headers = {};
  const res = { setHeader(name, value) { headers[name.toLowerCase()] = value; } };
  let nexted = false;
  contentSecurityPolicy({}, res, () => { nexted = true; });
  assert.equal(nexted, true);
  return headers['content-security-policy'];
}

// Rows in VERSIONS.md look like: | `htmx.min.js` | htmx.org | 2.0.4 | `sha384-...` |
function recordedFiles() {
  const md = fs.readFileSync(path.join(vendorDir, 'VERSIONS.md'), 'utf8');
  return [...md.matchAll(/^\|\s*`([^`]+\.js)`\s*\|\s*([^|]+?)\s*\|\s*([^|]+?)\s*\|\s*`(sha384-[A-Za-z0-9+/=]+)`\s*\|/gm)]
    .map(([, file, pkg, version, integrity]) => ({ file, pkg, version, integrity }));
}

test('CSP serves scripts from self only, with no unpkg origin', () => {
  const csp = cspHeader();
  const directives = Object.fromEntries(csp.split(';').map((d) => d.trim().split(/\s+/)).map(([k, ...v]) => [k, v]));
  assert.deepEqual(directives['script-src'], ["'self'"]);
  assert.doesNotMatch(csp, /unpkg/);
});

test('vendored htmx files match the versions and hashes recorded in VERSIONS.md', () => {
  const rows = recordedFiles();
  assert.deepEqual(rows.map((r) => [r.file, r.pkg]), [['htmx.min.js', 'htmx.org'], ['sse.min.js', 'htmx-ext-sse']]);
  assert.equal(rows[0].version, '2.0.4');
  assert.match(rows[1].version, /^2\.\d+\.\d+$/);
  for (const { file, integrity } of rows) {
    const body = fs.readFileSync(path.join(vendorDir, file));
    const actual = 'sha384-' + crypto.createHash('sha384').update(body).digest('base64');
    assert.equal(actual, integrity, `${file} does not match its recorded hash`);
  }
});

test('header partial loads the vendored files with matching SRI and no CDN', () => {
  const header = fs.readFileSync(path.join(root, 'views', 'partials', 'header.ejs'), 'utf8');
  assert.doesNotMatch(header, /unpkg|cdn\./);
  for (const { file, integrity } of recordedFiles()) {
    const tag = header.match(new RegExp(`<script[^>]*src="/vendor/${file.replace('.', '\\.')}"[^>]*>`));
    assert.ok(tag, `header.ejs does not load /vendor/${file}`);
    assert.match(tag[0], new RegExp(`integrity="${integrity.replace(/[+/]/g, '\\$&')}"`));
  }
});
