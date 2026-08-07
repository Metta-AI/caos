import assert from 'node:assert/strict';
import {
  modelChoices,
  modelLabel,
  parseComposerCommand,
  slashCommandMatches
} from '../ui/commands.js';

assert.deepEqual(parseComposerCommand('/from abc123'), { kind: 'from', argument: 'abc123' });
assert.deepEqual(parseComposerCommand('/title A useful title'), { kind: 'rename', argument: 'A useful title' });
assert.deepEqual(parseComposerCommand('/update-tree include edits'), {
  kind: 'update-tree',
  argument: 'include edits'
});
assert.deepEqual(parseComposerCommand('/commands'), { kind: 'commands', argument: '' });
assert.equal(parseComposerCommand('/unknown value'), null);

assert.deepEqual(slashCommandMatches('/up').map((command) => command.name), ['/update-tree']);
assert.equal(slashCommandMatches('/from abc').length, 0);

const choices = modelChoices('custom-model');
assert.equal(choices.at(-1).value, 'custom-model');
assert.equal(modelLabel('claude-opus-4-8', choices), 'Opus 4.8');
assert.equal(modelLabel('custom-model', choices), 'custom-model');
