const assert = require('node:assert/strict');
const { filePatchesFromPatch, lineCountsFromPatch } = require('../ui/changes.js');

const patch = [
  'diff --git a/desktop/ui/app.js b/desktop/ui/app.js',
  'index 1111111..2222222 100644',
  '--- a/desktop/ui/app.js',
  '+++ b/desktop/ui/app.js',
  '@@ -1 +1 @@',
  '-old app',
  '+new app',
  'diff --git a/desktop/ui/app.css b/desktop/ui/app.css',
  'index 3333333..4444444 100644',
  '--- a/desktop/ui/app.css',
  '+++ b/desktop/ui/app.css',
  '@@ -1 +1 @@',
  '-old css',
  '+new css',
  ''
].join('\n');

const files = filePatchesFromPatch(patch);
assert.deepEqual(files.map((file) => file.path), ['desktop/ui/app.js', 'desktop/ui/app.css']);
assert.match(files[0].patch, /\+new app/u);
assert.doesNotMatch(files[0].patch, /new css/u);
assert.match(files[1].patch, /\+new css/u);
assert.deepEqual(filePatchesFromPatch(''), []);
assert.deepEqual(lineCountsFromPatch(patch), { additions: 2, deletions: 2 });
assert.deepEqual(lineCountsFromPatch(''), { additions: 0, deletions: 0 });

console.log('change viewer tests passed');
