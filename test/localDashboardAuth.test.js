const test = require('node:test');
const assert = require('node:assert/strict');
process.env.MODE = 'local';
process.env.AUGMENTAGENT_API_KEY = 'synthetic-dashboard-key';
const { requireAuth, loginPageHandler } = require('../dist/security');
function request(overrides = {}) {
  const headers = {host:'localhost:3000', accept:'text/html', 'sec-fetch-site':'none', 'sec-fetch-mode':'navigate', 'sec-fetch-dest':'document', ...overrides.headers};
  return { method:'GET', socket:{remoteAddress:'127.0.0.1'}, ...overrides, headers, header(name) { return headers[name]; } };
}
function result(req, handler = requireAuth) {
  const out = {};
  const res = { status(n){out.status=n;return this}, type(){return this}, send(){return this}, json(){return this}, redirect(url){out.redirect=url} };
  handler(req,res,()=>out.allowed=true);
  return out;
}
test('local Chrome opens dashboard without a key and old login bookmarks redirect',()=>{
  assert.equal(result(request()).allowed,true);
  assert.equal(result(request(),loginPageHandler).redirect,'/');
  assert.equal(result(request({socket:{remoteAddress:'::ffff:127.0.0.1'}})).allowed,true);
  assert.equal(result(request({method:'POST',headers:{'sec-fetch-site':'same-origin',origin:'http://localhost:3000'}})).allowed,true);
});
test('remote, proxied, foreign-site and machine requests still need authentication',()=>{
  for(const req of [
    request({socket:{remoteAddress:'192.0.2.10'}}),
    request({headers:{host:'remote.example:3000'}}),
    request({headers:{forwarded:'for=192.0.2.10'}}),
    request({headers:{'x-forwarded-for':'127.0.0.1'}}),
    request({headers:{'sec-fetch-site':'cross-site'}}),
    request({headers:{'sec-fetch-site':'same-site'}}),
    request({headers:{'sec-fetch-site':undefined}}),
    request({method:'POST',headers:{'sec-fetch-site':'none'}}),
    request({method:'POST',headers:{'sec-fetch-site':'same-origin',origin:'https://foreign.example'}}),
  ]) assert.notEqual(result(req).allowed,true);
});
