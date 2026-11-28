const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const root = path.join(__dirname, '..');
const publicDir = path.join(root, 'public');
const manifest = JSON.parse(fs.readFileSync(path.join(publicDir, 'manifest.json'), 'utf8'));

// Width and height from a PNG's IHDR chunk (bytes 16-23).
function pngSize(file) {
  const buf = fs.readFileSync(file);
  assert.equal(buf.subarray(0, 8).toString('hex'), '89504e470d0a1a0a', `${file} is not a PNG`);
  return `${buf.readUInt32BE(16)}x${buf.readUInt32BE(20)}`;
}

test('manifest names the Jarvis Dashboard and starts at the app root', () => {
  assert.equal(manifest.name, 'Jarvis Dashboard');
  assert.equal(manifest.start_url, '/');
  assert.equal(manifest.scope, '/');
  assert.equal(manifest.display, 'standalone');
});

test('manifest icons exist at 192 and 512 and match their declared sizes', () => {
  const bySize = Object.fromEntries(manifest.icons.map((i) => [i.sizes, i]));
  assert.deepEqual(Object.keys(bySize).sort(), ['192x192', '512x512']);
  for (const [size, icon] of Object.entries(bySize)) {
    assert.equal(icon.src, `/icon-${size.split('x')[0]}.png`);
    assert.equal(icon.type, 'image/png');
    assert.equal(pngSize(path.join(publicDir, icon.src)), size);
  }
});

test('every manifest link is credentialed so it loads behind Access', () => {
  const views = path.join(root, 'views');
  const files = fs.readdirSync(views, { recursive: true }).filter((f) => f.endsWith('.ejs'));
  const links = files.flatMap((f) =>
    [...fs.readFileSync(path.join(views, f), 'utf8').matchAll(/<link[^>]*rel="manifest"[^>]*>/g)].map((m) => [f, m[0]]));
  assert.ok(links.some(([f]) => f === path.join('partials', 'header.ejs')), 'header partial links the manifest');
  for (const [file, tag] of links) {
    assert.match(tag, /href="\/manifest\.json"/, file);
    assert.match(tag, /crossorigin="use-credentials"/, file);
  }
});
