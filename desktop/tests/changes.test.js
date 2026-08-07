import assert from 'node:assert/strict';
import {
  filePatchesFromPatch,
  filePresentation,
  highlightedHunkLines,
  lineCounts,
  unchangedLinesBefore
} from '../ui/changes.js';
import { initializeHighlighting } from '../ui/highlight.js';

const patch = [
  'diff --git a/desktop/ui/app.js b/desktop/ui/app.js',
  'index 1111111..2222222 100644',
  '--- a/desktop/ui/app.js',
  '+++ b/desktop/ui/app.js',
  '@@ -10,3 +10,4 @@ function run() {',
  ' const before = true;',
  '-old app',
  '+const next = "new app";',
  '+return next;',
  ' }',
  '@@ -30 +31 @@ function finish() {',
  '-old finish',
  '+new finish',
  'diff --git a/desktop/ui/new.css b/desktop/ui/new.css',
  'new file mode 100644',
  'index 0000000..4444444',
  '--- /dev/null',
  '+++ b/desktop/ui/new.css',
  '@@ -0,0 +1 @@',
  '+.new { color: green; }',
  ''
].join('\n');

const files = filePatchesFromPatch(patch);
assert.deepEqual(files.map((file) => file.path), ['desktop/ui/app.js', 'desktop/ui/new.css']);
assert.deepEqual(files[0].stats, { additions: 3, deletions: 2 });
assert.equal(files[0].status, 'modified');
assert.equal(files[1].status, 'added');

const [firstHunk, secondHunk] = files[0].hunks;
assert.deepEqual(firstHunk.lines.slice(0, 4), [
  { kind: 'context', oldLine: 10, newLine: 10, text: 'const before = true;' },
  { kind: 'delete', oldLine: 11, newLine: null, text: 'old app' },
  { kind: 'add', oldLine: null, newLine: 11, text: 'const next = "new app";' },
  { kind: 'add', oldLine: null, newLine: 12, text: 'return next;' }
]);
assert.equal(unchangedLinesBefore(firstHunk), 9);
assert.equal(unchangedLinesBefore(secondHunk, firstHunk), 17);

assert.deepEqual(filePresentation('desktop/ui/app.js'), {
  badge: 'JS', directory: 'desktop/ui', extension: 'js', name: 'app.js'
});
assert.equal(filePresentation('flake.lock').badge, 'LOCK');

await initializeHighlighting();
const highlighted = highlightedHunkLines(firstHunk, 'desktop/ui/app.js');
assert.equal(
  highlighted.map((line) => line.tokens.map((token) => token.content).join('')).join('\n'),
  firstHunk.lines.map((line) => line.text).join('\n')
);
assert.ok(highlighted.flatMap((line) => line.tokens).some((token) => token.color));

assert.deepEqual(filePatchesFromPatch(''), []);
assert.deepEqual(lineCounts(files), { additions: 4, deletions: 2 });
assert.deepEqual(lineCounts([]), { additions: 0, deletions: 0 });

console.log('change viewer tests passed');
