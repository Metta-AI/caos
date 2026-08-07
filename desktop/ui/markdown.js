import createDOMPurify from 'dompurify';
import { marked } from 'marked';

import { copyText } from './clipboard.js';
import { appendHighlightedCode } from './highlight.js';

const SVG_NAMESPACE = 'http://www.w3.org/2000/svg';
let purifier = null;

function codeCopyIcon(copied = false) {
  const svg = document.createElementNS(SVG_NAMESPACE, 'svg');
  svg.setAttribute('aria-hidden', 'true');
  svg.setAttribute('viewBox', '0 0 24 24');
  const paths = copied
    ? [['path', { d: 'm5 12 4 4L19 6' }]]
    : [
      ['rect', { x: '9', y: '9', width: '10', height: '10', rx: '2' }],
      ['path', { d: 'M15 6V5a2 2 0 0 0-2-2H5a2 2 0 0 0-2 2v8a2 2 0 0 0 2 2h1' }]
    ];
  for (const [name, attributes] of paths) {
    const child = document.createElementNS(SVG_NAMESPACE, name);
    for (const [attribute, value] of Object.entries(attributes)) {
      child.setAttribute(attribute, value);
    }
    svg.append(child);
  }
  return svg;
}

async function copyCode(text, button) {
  try {
    await copyText(text);
    button.replaceChildren(codeCopyIcon(true));
    button.setAttribute('aria-label', 'Copied code');
    button.title = 'Copied';
    window.setTimeout(() => {
      button.replaceChildren(codeCopyIcon());
      button.setAttribute('aria-label', 'Copy code');
      button.title = 'Copy code';
    }, 1200);
  } catch (_) {
    button.title = 'Could not copy code';
  }
}

function enhanceCodeBlocks(container) {
  for (const code of container.querySelectorAll('pre > code')) {
    const text = code.textContent.replace(/\n$/u, '');
    const language = [...code.classList]
      .find((name) => name.startsWith('language-'))
      ?.slice('language-'.length);
    code.replaceChildren();
    appendHighlightedCode(code, text, language);
    const pre = code.parentElement;
    pre.className = 'markdown-code-block';
    const wrapper = document.createElement('div');
    wrapper.className = 'markdown-code-block-wrap';
    pre.replaceWith(wrapper);
    const copy = document.createElement('button');
    copy.type = 'button';
    copy.className = 'markdown-code-copy';
    copy.setAttribute('aria-label', 'Copy code');
    copy.title = 'Copy code';
    copy.append(codeCopyIcon());
    copy.addEventListener('click', () => copyCode(text, copy));
    wrapper.append(pre, copy);
  }
}

function enhanceMarkdown(container) {
  for (const link of container.querySelectorAll('a')) {
    link.rel = 'noreferrer';
    link.target = '_blank';
  }
  for (const checkbox of container.querySelectorAll('li > input[type="checkbox"]')) {
    checkbox.closest('li').classList.add('markdown-task-item');
  }
  for (const table of container.querySelectorAll('table')) {
    table.classList.add('markdown-table');
    const wrapper = document.createElement('div');
    wrapper.className = 'markdown-table-wrap';
    table.replaceWith(wrapper);
    wrapper.append(table);
  }
  enhanceCodeBlocks(container);
}

function markdownHtml(source) {
  return marked.parse(String(source || ''), { gfm: true });
}

function renderMarkdown(container, source) {
  purifier ||= createDOMPurify(window);
  container.innerHTML = purifier.sanitize(markdownHtml(source), {
    FORBID_ATTR: ['style'],
    FORBID_TAGS: ['style']
  });
  enhanceMarkdown(container);
}

export { markdownHtml, renderMarkdown };
