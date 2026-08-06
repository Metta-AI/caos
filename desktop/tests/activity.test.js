const assert = require('node:assert/strict');
const {
  activityGroupComplete,
  activityGroupSummary,
  mergeTransientTurnEntries,
  scrollPositionIsNearBottom,
  toolDescription,
  transientTurnEntries
} = require('../ui/activity.js');

const calls = [
  { name: 'bash', summary: '$ cargo test' },
  { name: 'read', summary: 'read desktop/ui/app.js' },
  { name: 'bash', summary: '$ git diff --check' }
];
assert.equal(activityGroupSummary(calls), '2 commands, Read desktop/ui/app.js');
assert.equal(toolDescription(calls[0]), 'Ran cargo test');
assert.equal(toolDescription({ name: 'edit', summary: 'edit desktop/ui/app.css' }), 'Edited desktop/ui/app.css');
assert.equal(activityGroupComplete({ calls: [] }), false);
assert.equal(activityGroupComplete({ calls: [{ result: { isError: false } }] }), true);
assert.equal(activityGroupComplete({ calls: [{ result: { isError: false } }, {}] }), false);
assert.equal(scrollPositionIsNearBottom({ scrollHeight: 800, clientHeight: 300, scrollTop: 500 }), true);
assert.equal(scrollPositionIsNearBottom({ scrollHeight: 800, clientHeight: 300, scrollTop: 450 }), false);

const activity = { role: 'activity', calls: [{ name: 'read' }] };
const optimistic = [
  { role: 'agent', message: 'Earlier answer' },
  { role: 'human', message: 'New request' },
  { role: 'agent', message: 'I will inspect it.' },
  activity,
  { role: 'agent', message: 'Done.' }
];
const transient = transientTurnEntries(optimistic, 1);
assert.deepEqual(transient, [optimistic[2], activity]);

const durable = [
  { role: 'agent', message: 'Earlier answer', shortCommit: '1111111' },
  { role: 'human', message: 'New request', shortCommit: '2222222' },
  { role: 'agent', message: 'Done.', shortCommit: '3333333' }
];
const merged = mergeTransientTurnEntries(durable, transient);
assert.deepEqual(merged, [durable[0], durable[1], optimistic[2], activity, durable[2]]);

console.log('activity timeline tests passed');
