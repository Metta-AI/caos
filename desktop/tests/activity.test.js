import assert from 'node:assert/strict';
import {
  activityGroupComplete,
  activityGroupExpandable,
  activityGroupSummary,
  mergeReplayedHistory,
  replayedTurnEntries,
  sameToolCall,
  scrollPositionIsNearBottom,
  toolDescription
} from '../ui/activity.js';

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
assert.equal(activityGroupExpandable({ calls: [{ name: 'bash' }] }), true);
assert.equal(activityGroupExpandable({ calls: [{ name: 'read' }] }), false);
assert.equal(activityGroupExpandable({ calls: [{ name: 'read' }, { name: 'edit' }] }), true);
assert.equal(scrollPositionIsNearBottom({ scrollHeight: 800, clientHeight: 300, scrollTop: 500 }), true);
assert.equal(scrollPositionIsNearBottom({ scrollHeight: 800, clientHeight: 300, scrollTop: 450 }), false);

const replayed = replayedTurnEntries([
  { kind: 'assistantText', text: 'I will inspect it.' },
  { kind: 'toolCall', request: 'req-1', round: 0, toolUseId: 'tool-1', stepCommit: 'aaaaaaa', name: 'read', summary: 'read README.md' },
  { kind: 'toolResult', request: 'req-1', round: 0, toolUseId: 'tool-1', stepCommit: 'bbbbbbb', isError: false, content: 'README contents' }
]);
assert.equal(replayed.length, 2);
assert.deepEqual(replayed[0], {
  role: 'agent',
  message: 'I will inspect it.',
  shortCommit: ''
});
assert.equal(replayed[1].role, 'activity');
assert.equal(replayed[1].running, false);
assert.equal(replayed[1].calls[0].result.content, 'README contents');
assert.equal(sameToolCall(
  { request: 'req-1', round: 1, toolUseId: 'tool-1' },
  { request: 'req-1', round: 1, toolUseId: 'tool-1' }
), true);
assert.equal(sameToolCall(
  { request: 'req-1', round: 1, toolUseId: 'tool-1' },
  { request: 'req-2', round: 1, toolUseId: 'tool-1' }
), false);
const orphanResult = replayedTurnEntries([
  { kind: 'toolResult', request: 'req-3', round: 4, toolUseId: 'tool-1', isError: false, content: 'ok' }
]);
assert.equal(orphanResult[0].calls[0].request, 'req-3');
assert.equal(orphanResult[0].calls[0].round, 4);

const turns = [
  { role: 'human', message: 'New request', commit: '1111111', timestampUnix: 100 },
  { role: 'agent', message: 'Done.', commit: '2222222', timestampUnix: 123 }
];
const history = mergeReplayedHistory(turns, [{ turnCommit: '2222222', events: [
  { kind: 'assistantText', text: 'I will inspect it.' },
  { kind: 'toolCall', request: 'req-1', round: 0, toolUseId: 'tool-1', stepCommit: 'aaaaaaa', name: 'read', summary: 'read README.md' },
  { kind: 'toolResult', request: 'req-1', round: 0, toolUseId: 'tool-1', stepCommit: 'bbbbbbb', isError: false, content: 'README contents' }
] }]);
assert.equal(history.length, 4);
assert.equal(history[0], turns[0]);
assert.equal(history[1].message, 'I will inspect it.');
assert.equal(history[2].role, 'activity');
assert.equal(history[3], turns[1]);

const aggregateHistory = mergeReplayedHistory(turns, [{ turnCommit: 'canonical-head', events: [
  { kind: 'toolCall', request: 'req-2', round: 2, toolUseId: 'tool-1', name: 'bash', summary: '$ cargo test' },
  { kind: 'toolResult', request: 'req-2', round: 2, toolUseId: 'tool-1', isError: false, content: 'ok' }
] }]);
assert.equal(aggregateHistory.at(-2).role, 'activity');
assert.equal(aggregateHistory.at(-1), turns[1]);

console.log('activity timeline tests passed');
