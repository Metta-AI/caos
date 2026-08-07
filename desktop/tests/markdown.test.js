import assert from 'node:assert/strict';

import { markdownHtml } from '../ui/markdown.js';

const emphasis = markdownHtml('plain **bold _and italic_**');
assert.match(emphasis, /<strong>bold <em>and italic<\/em><\/strong>/u);

const literal = markdownHtml('`**not bold** _not italic_` and **bold**');
assert.match(literal, /<code>\*\*not bold\*\* _not italic_<\/code>/u);
assert.match(literal, /<strong>bold<\/strong>/u);

const table = markdownHtml(`| Name | Role |
| :--- | ---: |
| Ann | **dev** |
| A\\|B | lead |`);
assert.match(table, /<table>/u);
assert.match(table, /align="right"/u);
assert.match(table, /A\|B/u);

const blocks = markdownHtml([
  '# Review',
  '',
  '1. First item',
  '   - Nested item with `code`',
  '2. Second item',
  '',
  '> Quoted **text**',
  '',
  '```rust',
  'fn main() {}',
  '```'
].join('\n'));
assert.match(blocks, /<h1>Review<\/h1>/u);
assert.match(blocks, /<ol>/u);
assert.match(blocks, /<ul>/u);
assert.match(blocks, /<blockquote>/u);
assert.match(blocks, /class="language-rust"/u);

console.log('markdown rendering tests passed');
