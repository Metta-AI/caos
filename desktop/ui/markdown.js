(function exposeMarkdown(global) {
  const SVG_NAMESPACE = 'http://www.w3.org/2000/svg';

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

  function fallbackCopy(text) {
    const field = document.createElement('textarea');
    field.value = text;
    field.style.position = 'fixed';
    field.style.opacity = '0';
    document.body.append(field);
    field.select();
    const copied = document.execCommand('copy');
    field.remove();
    if (!copied) throw new Error('copy command was rejected');
  }

  async function copyCode(text, button) {
    try {
      if (navigator.clipboard?.writeText) {
        try {
          await navigator.clipboard.writeText(text);
        } catch (_) {
          fallbackCopy(text);
        }
      } else {
        fallbackCopy(text);
      }
      button.replaceChildren(codeCopyIcon(true));
      button.setAttribute('aria-label', 'Copied code');
      button.title = 'Copied';
      global.setTimeout(() => {
        button.replaceChildren(codeCopyIcon());
        button.setAttribute('aria-label', 'Copy code');
        button.title = 'Copy code';
      }, 1200);
    } catch (_) {
      button.title = 'Could not copy code';
    }
  }

  function codeBlockElement(text, language = '') {
    const wrapper = document.createElement('div');
    wrapper.className = 'markdown-code-block-wrap';
    const pre = document.createElement('pre');
    pre.className = 'markdown-code-block';
    const code = document.createElement('code');
    if (language) code.className = `language-${language}`;
    code.textContent = text;
    pre.append(code);
    const copy = document.createElement('button');
    copy.type = 'button';
    copy.className = 'markdown-code-copy';
    copy.setAttribute('aria-label', 'Copy code');
    copy.title = 'Copy code';
    copy.append(codeCopyIcon());
    copy.addEventListener('click', () => copyCode(text, copy));
    wrapper.append(pre, copy);
    return wrapper;
  }

  function isWhitespace(character) {
    return /\s/u.test(character);
  }

  function isAlphaNumeric(character) {
    return /[\p{Letter}\p{Number}]/u.test(character);
  }

  function markerAt(characters, index, marker) {
    return marker.every((character, offset) => characters[index + offset] === character);
  }

  function findClosingMarker(characters, start, marker, canClose) {
    let index = start;
    while (index < characters.length) {
      if (characters[index] === '`') {
        const end = characters.indexOf('`', index + 1);
        if (end !== -1) {
          index = end + 1;
          continue;
        }
      }
      if (index > start && markerAt(characters, index, marker)) {
        const before = characters[index - 1];
        const after = characters[index + marker.length];
        if (canClose(before, after)) return index;
      }
      index += 1;
    }
    return -1;
  }

  function underscoreCanOpen(characters, index) {
    const before = characters[index - 1];
    const after = characters[index + 1];
    return (before === undefined || (!isAlphaNumeric(before) && before !== '_'))
      && after !== undefined
      && !isWhitespace(after)
      && after !== '_';
  }

  function appendInlineMarkdown(parent, source) {
    const characters = Array.from(source);
    let plain = '';
    let index = 0;
    const flushPlain = () => {
      if (!plain) return;
      parent.append(document.createTextNode(plain));
      plain = '';
    };

    while (index < characters.length) {
      if (characters[index] === '\\' && characters[index + 1] !== undefined) {
        plain += characters[index + 1];
        index += 2;
        continue;
      }
      if (characters[index] === '`') {
        const end = characters.indexOf('`', index + 1);
        if (end !== -1) {
          flushPlain();
          const code = document.createElement('code');
          code.textContent = characters.slice(index + 1, end).join('');
          parent.append(code);
          index = end + 1;
          continue;
        }
      }
      const image = characters[index] === '!' && characters[index + 1] === '[';
      if (image || characters[index] === '[') {
        const labelStart = index + (image ? 2 : 1);
        const labelEnd = characters.indexOf(']', labelStart);
        if (labelEnd !== -1 && characters[labelEnd + 1] === '(') {
          const hrefEnd = characters.indexOf(')', labelEnd + 2);
          if (hrefEnd !== -1) {
            const label = characters.slice(labelStart, labelEnd).join('');
            const href = safeUrl(characters.slice(labelEnd + 2, hrefEnd).join(''), image);
            if (href) {
              flushPlain();
              if (image) {
                const element = document.createElement('img');
                element.alt = label;
                element.src = href;
                parent.append(element);
              } else {
                const element = document.createElement('a');
                element.href = href;
                element.rel = 'noreferrer';
                element.target = '_blank';
                appendInlineMarkdown(element, label);
                parent.append(element);
              }
              index = hrefEnd + 1;
              continue;
            }
          }
        }
      }
      if (markerAt(characters, index, ['~', '~'])
        && characters[index + 2] !== undefined
        && !isWhitespace(characters[index + 2])) {
        const end = findClosingMarker(
          characters,
          index + 2,
          ['~', '~'],
          (before) => !isWhitespace(before)
        );
        if (end !== -1) {
          flushPlain();
          const deleted = document.createElement('del');
          appendInlineMarkdown(deleted, characters.slice(index + 2, end).join(''));
          parent.append(deleted);
          index = end + 2;
          continue;
        }
      }
      if (markerAt(characters, index, ['*', '*'])
        && characters[index + 2] !== undefined
        && !isWhitespace(characters[index + 2])) {
        const end = findClosingMarker(
          characters,
          index + 2,
          ['*', '*'],
          (before) => !isWhitespace(before)
        );
        if (end !== -1) {
          flushPlain();
          const strong = document.createElement('strong');
          appendInlineMarkdown(strong, characters.slice(index + 2, end).join(''));
          parent.append(strong);
          index = end + 2;
          continue;
        }
      }
      if (characters[index] === '*'
        && characters[index + 1] !== undefined
        && characters[index + 1] !== '*'
        && !isWhitespace(characters[index + 1])) {
        const end = findClosingMarker(
          characters,
          index + 1,
          ['*'],
          (before, after) => !isWhitespace(before) && after !== '*'
        );
        if (end !== -1) {
          flushPlain();
          const emphasis = document.createElement('em');
          appendInlineMarkdown(emphasis, characters.slice(index + 1, end).join(''));
          parent.append(emphasis);
          index = end + 1;
          continue;
        }
      }
      if (characters[index] === '_' && underscoreCanOpen(characters, index)) {
        const end = findClosingMarker(
          characters,
          index + 1,
          ['_'],
          (before, after) => !isWhitespace(before)
            && before !== '_'
            && (after === undefined || (!isAlphaNumeric(after) && after !== '_'))
        );
        if (end !== -1) {
          flushPlain();
          const emphasis = document.createElement('em');
          appendInlineMarkdown(emphasis, characters.slice(index + 1, end).join(''));
          parent.append(emphasis);
          index = end + 1;
          continue;
        }
      }
      plain += characters[index];
      index += 1;
    }
    flushPlain();
  }

  function safeUrl(value, image) {
    const trimmed = value.trim();
    if (image && /^data:image\/(?:png|gif|jpeg|webp);base64,/i.test(trimmed)) return trimmed;
    if (/^https?:\/\//i.test(trimmed)) return trimmed;
    if (!image && /^mailto:/i.test(trimmed)) return trimmed;
    return null;
  }

  function splitTableCells(line) {
    let inner = line.trim();
    if (inner.startsWith('|')) inner = inner.slice(1);
    if (inner.endsWith('|')) inner = inner.slice(0, -1);
    const cells = [];
    let current = '';
    for (let index = 0; index < inner.length; index += 1) {
      const character = inner[index];
      if (character === '\\' && inner[index + 1] === '|') {
        current += '|';
        index += 1;
      } else if (character === '|') {
        cells.push(current.trim());
        current = '';
      } else {
        current += character;
      }
    }
    cells.push(current.trim());
    return cells;
  }

  function tableAlignments(line) {
    if (!line?.includes('|')) return null;
    const alignments = [];
    for (const cell of splitTableCells(line)) {
      const match = cell.match(/^(:)?-+(:)?$/);
      if (!match) return null;
      alignments.push(match[1] && match[2] ? 'center' : match[2] ? 'right' : 'left');
    }
    return alignments;
  }

  function tableAt(lines, index) {
    const header = lines[index];
    if (!header?.includes('|')) return null;
    const alignments = tableAlignments(lines[index + 1]);
    if (!alignments) return null;
    const rows = [splitTableCells(header)];
    let consumed = 2;
    while (lines[index + consumed]?.trim() && lines[index + consumed].includes('|')) {
      rows.push(splitTableCells(lines[index + consumed]));
      consumed += 1;
    }
    const columns = Math.max(alignments.length, ...rows.map((row) => row.length));
    for (const row of rows) {
      while (row.length < columns) row.push('');
    }
    while (alignments.length < columns) alignments.push('left');
    return { alignments, consumed, rows };
  }

  function markdownTableElement(payload) {
    const wrapper = document.createElement('div');
    wrapper.className = 'markdown-table-wrap';
    const table = document.createElement('table');
    table.className = 'markdown-table';
    const head = document.createElement('thead');
    const headRow = document.createElement('tr');
    payload.rows[0].forEach((value, index) => {
      const cell = document.createElement('th');
      cell.style.textAlign = payload.alignments[index];
      appendInlineMarkdown(cell, value);
      headRow.append(cell);
    });
    head.append(headRow);
    table.append(head);
    if (payload.rows.length > 1) {
      const body = document.createElement('tbody');
      for (const row of payload.rows.slice(1)) {
        const tableRow = document.createElement('tr');
        row.forEach((value, index) => {
          const cell = document.createElement('td');
          cell.style.textAlign = payload.alignments[index];
          appendInlineMarkdown(cell, value);
          tableRow.append(cell);
        });
        body.append(tableRow);
      }
      table.append(body);
    }
    wrapper.append(table);
    return wrapper;
  }

  function listMatch(line) {
    const match = line.match(/^(\s*)([-+*]|\d+[.)])\s+(.*)$/);
    if (!match) return null;
    return {
      indent: match[1].replaceAll('\t', '    ').length,
      ordered: /^\d/u.test(match[2]),
      text: match[3]
    };
  }

  function renderList(lines, start, baseIndent, ordered) {
    const list = document.createElement(ordered ? 'ol' : 'ul');
    let index = start;
    let lastItem = null;
    while (index < lines.length) {
      const match = listMatch(lines[index]);
      if (!match || match.indent < baseIndent) break;
      if (match.indent > baseIndent) {
        if (!lastItem) break;
        const nested = renderList(lines, index, match.indent, match.ordered);
        lastItem.append(nested.element);
        index = nested.next;
        continue;
      }
      if (match.ordered !== ordered) break;
      const item = document.createElement('li');
      const task = match.text.match(/^\[([ xX])\]\s+(.*)$/);
      if (task) {
        const checkbox = document.createElement('input');
        checkbox.type = 'checkbox';
        checkbox.checked = task[1].toLowerCase() === 'x';
        checkbox.disabled = true;
        item.className = 'markdown-task-item';
        item.append(checkbox);
        appendInlineMarkdown(item, task[2]);
      } else {
        appendInlineMarkdown(item, match.text);
      }
      list.append(item);
      lastItem = item;
      index += 1;
    }
    return { element: list, next: index };
  }

  function fenceAt(line) {
    const match = line.match(/^\s{0,3}(`{3,}|~{3,})\s*([^\s`]*)?.*$/);
    if (!match) return null;
    return { character: match[1][0], length: match[1].length, language: match[2] || '' };
  }

  function horizontalRule(line) {
    const compact = line.trim().replaceAll(' ', '');
    return compact.length >= 3
      && (compact.split('').every((character) => character === '-')
        || compact.split('').every((character) => character === '*')
        || compact.split('').every((character) => character === '_'));
  }

  function startsBlock(lines, index) {
    const line = lines[index];
    return !line.trim()
      || Boolean(fenceAt(line))
      || /^\s{0,3}#{1,6}\s+/u.test(line)
      || /^\s{0,3}>/u.test(line)
      || /^ {4}\S/u.test(line)
      || Boolean(listMatch(line))
      || horizontalRule(line)
      || Boolean(tableAt(lines, index));
  }

  function paragraphElement(lines) {
    const paragraph = document.createElement('p');
    lines.forEach((line, index) => {
      const hardBreak = / {2}$/u.test(line);
      appendInlineMarkdown(paragraph, line.trimEnd());
      if (index < lines.length - 1) {
        paragraph.append(hardBreak ? document.createElement('br') : document.createTextNode(' '));
      }
    });
    return paragraph;
  }

  function appendBlocks(container, lines) {
    for (let index = 0; index < lines.length;) {
      const line = lines[index];
      if (!line.trim()) {
        index += 1;
        continue;
      }

      const fence = fenceAt(line);
      if (fence) {
        const body = [];
        index += 1;
        while (index < lines.length) {
          const closing = lines[index].trim();
          if (closing.length >= fence.length
            && closing.split('').every((character) => character === fence.character)) {
            index += 1;
            break;
          }
          body.push(lines[index]);
          index += 1;
        }
        container.append(codeBlockElement(body.join('\n'), fence.language));
        continue;
      }

      const table = tableAt(lines, index);
      if (table) {
        container.append(markdownTableElement(table));
        index += table.consumed;
        continue;
      }

      const heading = line.match(/^\s{0,3}(#{1,6})\s+(.+?)\s*#*$/);
      if (heading) {
        const element = document.createElement(`h${heading[1].length}`);
        appendInlineMarkdown(element, heading[2]);
        container.append(element);
        index += 1;
        continue;
      }

      if (/^\s{0,3}>/u.test(line)) {
        const quoted = [];
        while (index < lines.length && /^\s{0,3}>/u.test(lines[index])) {
          quoted.push(lines[index].replace(/^\s{0,3}>\s?/u, ''));
          index += 1;
        }
        const blockquote = document.createElement('blockquote');
        appendBlocks(blockquote, quoted);
        container.append(blockquote);
        continue;
      }

      const list = listMatch(line);
      if (list) {
        const rendered = renderList(lines, index, list.indent, list.ordered);
        container.append(rendered.element);
        index = rendered.next;
        continue;
      }

      if (horizontalRule(line)) {
        container.append(document.createElement('hr'));
        index += 1;
        continue;
      }

      if (/^ {4}\S/u.test(line)) {
        const body = [];
        while (index < lines.length && (lines[index].startsWith('    ') || !lines[index].trim())) {
          body.push(lines[index].startsWith('    ') ? lines[index].slice(4) : '');
          index += 1;
        }
        container.append(codeBlockElement(body.join('\n')));
        continue;
      }

      const paragraph = [line];
      index += 1;
      while (index < lines.length && !startsBlock(lines, index)) {
        paragraph.push(lines[index]);
        index += 1;
      }
      container.append(paragraphElement(paragraph));
    }
  }

  function renderMarkdown(container, source) {
    container.replaceChildren();
    const lines = String(source).split(/\r?\n/);
    appendBlocks(container, lines);
  }

  global.CaosMarkdown = { renderMarkdown };
  if (typeof module !== 'undefined') module.exports = global.CaosMarkdown;
}(globalThis));
