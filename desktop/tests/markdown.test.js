const assert = require('node:assert/strict');

class FakeNode {
  constructor(tagName, text = null) {
    this.tagName = tagName;
    this.text = text;
    this.children = [];
    this.className = '';
    this.style = {};
    this.attributes = {};
    this.listeners = {};
  }

  append(...children) {
    this.children.push(...children);
  }

  replaceChildren(...children) {
    this.children = children;
  }

  setAttribute(name, value) {
    this.attributes[name] = String(value);
  }

  addEventListener(name, listener) {
    this.listeners[name] = listener;
  }

  get textContent() {
    return this.text ?? this.children.map((child) => child.textContent).join('');
  }

  set textContent(value) {
    this.text = String(value);
    this.children = [];
  }
}

global.document = {
  createElement: (tagName) => new FakeNode(tagName.toUpperCase()),
  createElementNS: (_namespace, tagName) => new FakeNode(tagName.toUpperCase()),
  createTextNode: (text) => new FakeNode('#TEXT', text)
};

const { renderMarkdown } = require('../ui/markdown.js');

function render(source) {
  const container = new FakeNode('ROOT');
  renderMarkdown(container, source);
  return container;
}

const emphasis = render('plain **bold _and italic_**');
assert.equal(emphasis.textContent, 'plain bold and italic');
assert.equal(emphasis.children[0].children[1].tagName, 'STRONG');
assert.equal(emphasis.children[0].children[1].children[1].tagName, 'EM');

const literal = render('`**not bold** _not italic_` and **bold**');
assert.equal(literal.textContent, '**not bold** _not italic_ and bold');
assert.equal(literal.children[0].children[0].tagName, 'CODE');
assert.equal(literal.children[0].children.filter((child) => child.tagName === 'STRONG').length, 1);

const unmatched = render('**open _still open snake_case __literal__');
assert.equal(unmatched.textContent, '**open _still open snake_case __literal__');
assert.equal(unmatched.children[0].children.length, 1);

const table = render(`| Name | Role |
| :--- | ---: |
| Ann | **dev** |
| A\\|B | lead |`);
const tableElement = table.children[0].children[0];
assert.equal(tableElement.tagName, 'TABLE');
assert.equal(tableElement.children[0].children[0].children[0].style.textAlign, 'left');
assert.equal(tableElement.children[0].children[0].children[1].style.textAlign, 'right');
assert.equal(tableElement.children[1].children[0].children[1].children[0].tagName, 'STRONG');
assert.equal(tableElement.children[1].children[1].children[0].textContent, 'A|B');

const blocks = render([
  '# Review',
  '',
  '1. First item',
  '   - Nested item with `code`',
  '2. Second item',
  '',
  '> Quoted **detail**',
  '',
  '```rust',
  'fn main() {}',
  '```',
  '',
  '[Open CAOS](https://example.com)'
].join('\n'));
assert.equal(blocks.children[0].tagName, 'H1');
assert.equal(blocks.children[1].tagName, 'OL');
assert.equal(blocks.children[1].children[0].children[1].tagName, 'UL');
assert.equal(blocks.children[2].tagName, 'BLOCKQUOTE');
assert.equal(blocks.children[3].className, 'markdown-code-block-wrap');
assert.equal(blocks.children[3].children[0].tagName, 'PRE');
assert.equal(blocks.children[3].children[0].children[0].className, 'language-rust');
assert.equal(blocks.children[3].children[1].className, 'markdown-code-copy');
assert.equal(blocks.children[3].children[1].attributes['aria-label'], 'Copy code');
assert.equal(blocks.children[4].children[0].tagName, 'A');
assert.equal(blocks.children[4].children[0].href, 'https://example.com');
assert.equal(blocks.children[4].children[0].target, '_blank');
assert.equal(blocks.children[4].children[0].rel, 'noreferrer');

console.log('markdown rendering tests passed');
