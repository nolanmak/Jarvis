// Approval cards label the action's platform. Unknown ids must still read as
// a word, never a raw lowercase id (seen live: "whatsapp", "gcal").
const test = require('node:test');
const assert = require('node:assert/strict');
const { platformLabel } = require('../dist/web/fragments');

test('known platforms get their product names', () => {
  assert.equal(platformLabel('gmail'), 'Gmail');
  assert.equal(platformLabel('whatsapp'), 'WhatsApp');
  assert.equal(platformLabel('gcal'), 'Calendar');
  assert.equal(platformLabel('linkedin'), 'LinkedIn');
  assert.equal(platformLabel('imessage'), 'iMessage');
});

test('missing platform defaults to Gmail, unknown ids are capitalized', () => {
  assert.equal(platformLabel(undefined), 'Gmail');
  assert.equal(platformLabel('newthing'), 'Newthing');
});
