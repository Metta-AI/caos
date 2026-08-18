import { createHighlighterCore } from '@shikijs/core';
import { createJavaScriptRegexEngine } from '@shikijs/engine-javascript';
import css from '@shikijs/langs/css';
import go from '@shikijs/langs/go';
import html from '@shikijs/langs/html';
import javascript from '@shikijs/langs/javascript';
import jsx from '@shikijs/langs/jsx';
import json from '@shikijs/langs/json';
import markdown from '@shikijs/langs/markdown';
import nix from '@shikijs/langs/nix';
import python from '@shikijs/langs/python';
import rust from '@shikijs/langs/rust';
import shellscript from '@shikijs/langs/shellscript';
import toml from '@shikijs/langs/toml';
import tsx from '@shikijs/langs/tsx';
import typescript from '@shikijs/langs/typescript';
import yaml from '@shikijs/langs/yaml';
import githubDarkDefault from '@shikijs/themes/github-dark-default';

const THEME = 'github-dark-default';
const LANGUAGE_BY_EXTENSION = new Map([
  ['css', 'css'], ['go', 'go'], ['htm', 'html'], ['html', 'html'], ['js', 'javascript'],
  ['jsx', 'jsx'], ['json', 'json'], ['md', 'markdown'], ['nix', 'nix'], ['py', 'python'],
  ['rs', 'rust'], ['sh', 'shellscript'], ['toml', 'toml'], ['ts', 'typescript'],
  ['tsx', 'tsx'], ['yaml', 'yaml'], ['yml', 'yaml']
]);
const LANGUAGE_ALIASES = new Map([
  ['bash', 'shellscript'], ['js', 'javascript'], ['md', 'markdown'], ['py', 'python'],
  ['rs', 'rust'], ['sh', 'shellscript'], ['shell', 'shellscript'], ['ts', 'typescript'],
  ['yml', 'yaml']
]);

let highlighter = null;

async function initializeHighlighting() {
  highlighter ||= await createHighlighterCore({
    engine: createJavaScriptRegexEngine(),
    langs: [
      css, go, html, javascript, jsx, json, markdown, nix, python, rust, shellscript,
      toml, tsx, typescript, yaml
    ],
    themes: [githubDarkDefault]
  });
}

function languageFor(value) {
  const normalized = String(value || '').toLowerCase();
  const hint = normalized.includes('.') ? normalized.split('.').at(-1) : normalized;
  return LANGUAGE_BY_EXTENSION.get(hint) || LANGUAGE_ALIASES.get(hint) || hint || 'text';
}

function codeTokens(source, languageOrPath) {
  const text = String(source || '');
  const language = languageFor(languageOrPath);
  if (!highlighter || !highlighter.getLoadedLanguages().includes(language)) {
    return text.split('\n').map((line) => [{ content: line }]);
  }
  return highlighter.codeToTokensBase(text, { lang: language, theme: THEME });
}

function appendTokens(container, tokens) {
  for (const token of tokens) {
    const span = document.createElement('span');
    span.textContent = token.content;
    if (token.htmlStyle) {
      for (const [property, value] of Object.entries(token.htmlStyle)) {
        span.style.setProperty(property, value);
      }
    } else {
      if (token.color) span.style.color = token.color;
      if (token.bgColor) span.style.backgroundColor = token.bgColor;
      if (token.fontStyle & 1) span.style.fontStyle = 'italic';
      if (token.fontStyle & 2) span.style.fontWeight = 'bold';
      if (token.fontStyle & 4) span.style.textDecoration = 'underline';
    }
    container.append(span);
  }
}

function appendHighlightedCode(container, source, languageOrPath) {
  codeTokens(source, languageOrPath).forEach((tokens, lineIndex) => {
    if (lineIndex > 0) container.append(document.createTextNode('\n'));
    appendTokens(container, tokens);
  });
}

export { appendHighlightedCode, appendTokens, codeTokens, initializeHighlighting, languageFor };
