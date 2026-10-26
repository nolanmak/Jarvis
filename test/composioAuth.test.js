const test = require('node:test');
const assert = require('node:assert/strict');
const express = require('express');
const {createComposioAuthRouter} = require('../dist/composioAuthRoutes');
process.env.AUGMENTAGENT_API_KEY = 'synthetic-auth-key';
const {requireAuth} = require('../dist/security');

async function fixture(t) {
  const config = new Map(), accounts = new Map(), links = [], saved = [];
  let gmail = [], failure = false;
  const client = {
    link: {create: async args => {
      links.push(args);
      const id = `ca_${links.length}`;
      accounts.set(id, {id, user_id: args.user_id, toolkit: {slug: args.auth_config_id}, status: 'INITIATED'});
      return {connected_account_id:id, redirect_url:`https://connect.composio.dev/link/${id}`, expires_at:new Date(Date.now()+600000).toISOString()};
    }},
    connectedAccounts: {retrieve: async id => accounts.get(id)},
  };
  const deps = {
    getClient:()=>client, getAuthConfig:async (_, toolkit)=>toolkit,
    getConfig:key=>config.get(key), setConfig:(key,value)=>config.set(key,value),
    getActiveGmailAccounts:()=>gmail,
    persist:async (...args)=> { await new Promise(r=>setTimeout(r,5)); if(failure) throw Error('secret-upstream-token'); saved.push(args); },
  };
  const app = express(); app.use(express.json()); app.use(requireAuth);
  app.use('/api/composio/auth', createComposioAuthRouter(deps));
  const server = app.listen(0,'127.0.0.1');
  await new Promise(r=>server.once('listening',r));
  t.after(()=>new Promise(r=>{server.closeAllConnections();server.close(r)}));
  const post = async (operation, body, authenticated=true)=> {
    const response=await fetch(`http://127.0.0.1:${server.address().port}/api/composio/auth/${operation}`, {
      method:'POST', headers:{'Content-Type':'application/json', ...(authenticated?{Authorization:'Bearer synthetic-auth-key'}:{})},body:JSON.stringify(body)});
    return {status:response.status, body:await response.text().then(text=>{try{return JSON.parse(text)}catch{return text}})};
  };
  return {post, config, accounts, links, saved, client, deps, gmail(value){gmail=value}, fail(value){failure=value}};
}

test('all four integrations require verified consent and explicit persistence, without localhost callbacks',async t=>{
  const f=await fixture(t); f.gmail([{entityId:'gmail-owner'}]);
  for(const provider of ['gmail','calendar','googledrive','slack']) {
    const start=await f.post('start',{provider}); assert.equal(start.status,201);
    const {attempt_id, connection_id}=start.body;
    assert.equal('callback_url' in f.links.at(-1),false);
    assert.equal((await f.post('status',{attempt_id})).body.connected,false);
    assert.equal((await f.post('complete',{attempt_id})).status,409);
    f.accounts.get(connection_id).status='ACTIVE';
    const ready=await f.post('status',{attempt_id}); assert.equal(ready.body.status,'ACTIVE'); assert.equal(ready.body.connected,false);
    const before=f.saved.length;
    const results=await Promise.all([f.post('complete',{attempt_id}),f.post('complete',{attempt_id})]);
    results.forEach(result=>assert.equal(result.body.connected,true));
    assert.equal(f.saved.length,before+1);
    assert.equal((await f.post('complete',{attempt_id})).body.connected,true);
    assert.equal(f.saved.length,before+1);
  }
});

test('overlapping attempts stay isolated and reject mismatched account identity',async t=>{
  const f=await fixture(t);
  const first=(await f.post('start',{provider:'gmail'})).body;
  const second=(await f.post('start',{provider:'gmail'})).body;
  assert.notEqual(first.entity_id,second.entity_id);
  const account=f.accounts.get(first.connection_id); account.status='ACTIVE';
  const original={...account};
  for(const mismatch of [{id:second.connection_id},{user_id:second.entity_id},{toolkit:{slug:'slack'}}]) {
    Object.assign(account,original,mismatch);
    assert.equal((await f.post('complete',{attempt_id:first.attempt_id})).status,409);
  }
  assert.equal(f.saved.length,0); Object.assign(account,original);
  assert.equal((await f.post('complete',{attempt_id:first.attempt_id})).body.connected,true);
  assert.equal((await f.post('status',{attempt_id:second.attempt_id})).body.connected,false);
});

test('authentication, strict validation and calendar account selection fail closed',async t=>{
  const f=await fixture(t);
  for(const op of ['start','status','complete']) assert.equal((await f.post(op,{},false)).status,401);
  for(const body of [{provider:'toString'},{provider:'__proto__'},{provider:'discord'},{provider:'gmail',entity_id:3},{provider:'gmail',entity_id:'other'}, {provider:'calendar',entity_id:null},{provider:'gmail',unknown:1},[],null]) {
    assert.equal((await f.post('start',body)).status,400);
  }
  assert.equal(f.links.length,0);
  assert.equal((await f.post('start',{provider:'calendar'})).status,400);
  f.gmail([{entityId:'first'},{entityId:'second'}]);
  const ambiguous=await f.post('start',{provider:'calendar'});
  assert.equal(ambiguous.status,400); assert.deepEqual(ambiguous.body.accounts,[{entity_id:'first'},{entity_id:'second'}]);
  assert.equal((await f.post('start',{provider:'calendar',entity_id:'unknown'})).status,400);
  assert.equal((await f.post('start',{provider:'calendar',entity_id:'second'})).body.entity_id,'second');
  for(const body of [{},{attempt_id:3},{attempt_id:'invalid'},{attempt_id:'00000000-0000-0000-0000-000000000000',extra:true}]) assert.equal((await f.post('status',body)).status,400);
  assert.equal((await f.post('status',{attempt_id:'00000000-0000-0000-0000-000000000000'})).status,404);
});

test('failed persistence is retryable and errors do not leak upstream secrets',async t=>{
  const f=await fixture(t); const start=(await f.post('start',{provider:'slack'})).body;
  f.accounts.get(start.connection_id).status='ACTIVE'; f.fail(true);
  const failed=await f.post('complete',{attempt_id:start.attempt_id});
  assert.equal(failed.status,502); assert(!JSON.stringify(failed).includes('secret-upstream-token'));
  assert.equal((await f.post('status',{attempt_id:start.attempt_id})).body.connected,false);
  f.fail(false); assert.equal((await f.post('complete',{attempt_id:start.attempt_id})).body.connected,true);
});

test('expired attempts and malformed provider links never become connected',async t=>{
  const f=await fixture(t); const start=(await f.post('start',{provider:'gmail'})).body;
  const key=`composio_auth_attempt_${start.attempt_id}`; const stored=JSON.parse(f.config.get(key)); stored.expiresAt=0; f.config.set(key,JSON.stringify(stored));
  assert.equal((await f.post('complete',{attempt_id:start.attempt_id})).status,410);
  assert.equal(f.saved.length,0);
  for(const link of [{}, {connected_account_id:'id',redirect_url:'http://unsafe.example',expires_at:new Date(Date.now()+10000).toISOString()}, {connected_account_id:'id',redirect_url:'https://connect.composio.dev/link/id',expires_at:'invalid'}]) {
    f.client.link.create=async()=>link;
    assert.equal((await f.post('start',{provider:'gmail'})).status,502);
  }
  assert.equal(f.config.size,1);
});

test('dashboard persistence adapters save the correct provider and propagate failures',async()=>{
  const fs=require('node:fs'), vm=require('node:vm');
  const source=fs.readFileSync(require.resolve('../dist/dashboard'),'utf8');
  const start=source.indexOf('router.use("/api/composio/auth"');
  const end=source.indexOf('router.get("/oauth/gmail/start"',start);
  assert(start>=0 && end>start);
  let deps, gmail=[], drive=[], slack=[], profileFails=false, slackFails=false;
  const context={
    router:{use(){}}, composioAuthRoutes_1:{createComposioAuthRouter(value){deps=value}},
    getComposioClient:()=>({tools:{execute:async(slug,args)=>{
      assert.equal(slug,'GOOGLEDRIVE_GET_ABOUT'); assert.equal(args.connected_account_id,'drive-id');
      return {successful:!profileFails,data:{user:{emailAddress:'drive@example.test'}}};
    }}}), getOrCreateAuthConfig:async()=>{}, calendarStatusCache:{stale:true}, process:{env:{}},
    db_1:{getConfig:()=> 'synthetic-key',setConfig(){},getActiveGmailAccounts:()=>[],
      getGmailAccounts:()=>gmail,addGmailAccount:(connectionId,entityId,email)=>gmail.push({connectionId,entityId,email}),
      addDriveAccount:(...args)=>drive.push(args)},
    slackApi_1:{persistSlackAuth:async args=>{slack.push(args);return {ok:!slackFails}}},
  };
  vm.runInNewContext(source.slice(start,end),context);
  await deps.persist('gmail','gmail-id','owner',{email:'owner@example.test'});
  await deps.persist('gmail','gmail-id','owner',{email:'owner@example.test'});
  assert.equal(gmail.length,1); assert.equal(gmail[0].entityId,'owner');
  profileFails=true; await assert.rejects(deps.persist('googledrive','drive-id','owner',{})); assert.equal(drive.length,0);
  profileFails=false; await deps.persist('googledrive','drive-id','owner',{});
  assert.deepEqual(drive[0],['drive-id','owner','drive@example.test','drive@example.test']);
  slackFails=true; await assert.rejects(deps.persist('slack','slack-id','owner',{}));
  slackFails=false; await deps.persist('slack','slack-id','owner',{});
  assert.equal(slack[1].connectionId,'slack-id'); assert.equal(slack[1].entityId,'owner');
  await deps.persist('calendar','calendar-id','owner',{}); assert.equal(context.calendarStatusCache,null);
});

test('gmail start goes to the operator\'s own Google client when configured and is saved by the callback', async t => {
  const f = await fixture(t);
  const attempts = new Map();
  let configured = true;
  f.deps.google = {
    configured: () => configured,
    start: async () => {
      const attempt_id = 'aaaaaaaa-0000-4000-8000-00000000000a';
      attempts.set(attempt_id, {attempt_id, status: 'pending'});
      return {attempt_id, auth_url: 'https://accounts.google.com/o/oauth2/v2/auth?state=' + attempt_id, expires_at: new Date(Date.now() + 600000).toISOString()};
    },
    status: id => attempts.get(id) ?? null,
  };
  const start = await f.post('start', {provider: 'gmail'});
  assert.equal(start.status, 201);
  assert.equal(start.body.transport, 'google');
  assert.match(start.body.auth_url, /^https:\/\/accounts\.google\.com\//);
  assert.equal(f.links.length, 0, 'no Composio link is created for a Google connect');
  const {attempt_id} = start.body;
  const pending = await f.post('status', {attempt_id});
  assert.equal(pending.body.status, 'PENDING'); assert.equal(pending.body.connected, false);
  assert.equal((await f.post('complete', {attempt_id})).status, 409);
  attempts.set(attempt_id, {attempt_id, status: 'connected', email: 'owner@example.com', entity_id: 'google:owner@example.com'});
  const done = await f.post('complete', {attempt_id});
  assert.equal(done.status, 200); assert.equal(done.body.connected, true); assert.equal(done.body.entity_id, 'google:owner@example.com');
  attempts.set(attempt_id, {attempt_id, status: 'failed', error: 'access_denied'});
  const failed = await f.post('status', {attempt_id});
  assert.equal(failed.body.status, 'FAILED'); assert.equal(failed.body.error, 'access_denied');
  // Other providers, and Gmail without a Google client, still use Composio.
  configured = false;
  const composio = await f.post('start', {provider: 'gmail'});
  assert.equal(composio.status, 201); assert.equal(composio.body.transport, undefined); assert.equal(f.links.length, 1);
});
