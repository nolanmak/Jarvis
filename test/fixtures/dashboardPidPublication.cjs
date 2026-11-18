// Diagnostic scheduler: preserve open/write/close semantics, pause the fixture
// after creating its PID file until the test has observed that filesystem state.
const fs = require('node:fs');
const open = fs.openSync, write = fs.writeFileSync, read = fs.readFileSync, exists = fs.existsSync;
const gate = process.env.L5_PID_GATE;
const trace = process.env.L5_PID_TRACE;
function log(event, fields = {}) { fs.appendFileSync(trace, JSON.stringify({ event, pid: process.pid, ...fields }) + '\n'); }
let released = false;
function release() { if (!released) { released = true; write(gate, 'continue'); } }
fs.writeFileSync = function(file, data, options) {
  const pid = process.env.SYNTHETIC_PID_FILE;
  if (pid && (file === pid || file === pid + '.tmp')) {
    const fd = open(file, 'w', 0o666);
    write(gate + '.opened', 'opened');
    log('pid-file-created', { file });
    read(gate); // FIFO handshake, not a timing delay.
    try { return write(fd, data, options); } finally { fs.closeSync(fd); }
  }
  return write.apply(this, arguments);
};
fs.existsSync = function(file) {
  const present = exists(file);
  if (file === process.env.SYNTHETIC_PID_FILE && !present && exists(gate + '.opened')) {
    log('pid-not-published'); release();
  }
  return present;
};
fs.readFileSync = function(file, options) {
  const value = read.apply(this, arguments);
  if (file === process.env.SYNTHETIC_PID_FILE) {
    log('pid-read', { value: String(value), parsed: Number(value) });
    release();
  }
  return value;
};
const cp = require('node:child_process'), spawn = cp.spawn;
cp.spawn = function(...args) {
  const child = spawn.apply(this, args);
  if (String(args[0]).includes('fake-augmentagent')) {
    log('spawn', { child: child.pid });
    child.on('exit', (code, signal) => log('child-exit', { child: child.pid, code, signal }));
  }
  return child;
};
