(function changeHelpers(globalScope) {
  const FILE_BADGES = new Map([
    ['css', 'CSS'], ['go', 'GO'], ['html', 'HTML'], ['js', 'JS'], ['jsx', 'JS'],
    ['json', 'JSON'], ['md', 'MD'], ['nix', 'NIX'], ['py', 'PY'], ['rs', 'RS'],
    ['sh', 'SH'], ['toml', 'TOML'], ['ts', 'TS'], ['tsx', 'TS'], ['yaml', 'YML'],
    ['yml', 'YML']
  ]);

  const KEYWORDS = new Set([
    'as', 'async', 'await', 'break', 'catch', 'class', 'const', 'continue', 'crate',
    'def', 'else', 'enum', 'export', 'extends', 'false', 'finally', 'fn', 'for',
    'from', 'function', 'if', 'impl', 'import', 'in', 'interface', 'let', 'match',
    'mod', 'move', 'mut', 'new', 'None', 'null', 'pub', 'return', 'self', 'Some',
    'static', 'struct', 'super', 'this', 'throw', 'trait', 'true', 'try', 'type',
    'use', 'var', 'while', 'yield'
  ]);

  function lineCountsFromPatch(patch) {
    let additions = 0;
    let deletions = 0;
    for (const line of String(patch || '').split('\n')) {
      if (line.startsWith('+') && !line.startsWith('+++')) additions += 1;
      if (line.startsWith('-') && !line.startsWith('---')) deletions += 1;
    }
    return { additions, deletions };
  }

  function filePresentation(path) {
    const normalized = String(path || '');
    const slash = normalized.lastIndexOf('/');
    const name = slash >= 0 ? normalized.slice(slash + 1) : normalized;
    const directory = slash >= 0 ? normalized.slice(0, slash) : '.';
    const extension = name.includes('.') ? name.split('.').at(-1).toLowerCase() : '';
    return {
      badge: FILE_BADGES.get(extension) || (extension ? extension.slice(0, 4).toUpperCase() : 'FILE'),
      directory,
      extension,
      name
    };
  }

  function parseHunks(lines) {
    const hunks = [];
    let current = null;
    let oldLine = 0;
    let newLine = 0;
    for (const line of lines) {
      const header = line.match(/^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@(?:\s?(.*))?$/u);
      if (header) {
        current = {
          oldStart: Number(header[1]),
          oldCount: Number(header[2] || 1),
          newStart: Number(header[3]),
          newCount: Number(header[4] || 1),
          context: header[5] || '',
          lines: []
        };
        oldLine = current.oldStart;
        newLine = current.newStart;
        hunks.push(current);
        continue;
      }
      if (!current) continue;
      if (line.startsWith('+') && !line.startsWith('+++')) {
        current.lines.push({ kind: 'add', oldLine: null, newLine, text: line.slice(1) });
        newLine += 1;
      } else if (line.startsWith('-') && !line.startsWith('---')) {
        current.lines.push({ kind: 'delete', oldLine, newLine: null, text: line.slice(1) });
        oldLine += 1;
      } else if (line.startsWith(' ')) {
        current.lines.push({ kind: 'context', oldLine, newLine, text: line.slice(1) });
        oldLine += 1;
        newLine += 1;
      } else if (line === '\\ No newline at end of file') {
        current.lines.push({ kind: 'notice', oldLine: null, newLine: null, text: line.slice(2) });
      }
    }
    return hunks;
  }

  function completedFile(file) {
    const patch = file.lines.join('\n');
    const stats = lineCountsFromPatch(patch);
    let status = 'modified';
    if (file.lines.some((line) => line.startsWith('new file mode '))) status = 'added';
    if (file.lines.some((line) => line.startsWith('deleted file mode '))) status = 'deleted';
    if (file.lines.some((line) => line.startsWith('rename from '))) status = 'renamed';
    const renamedTo = file.lines.find((line) => line.startsWith('rename to '));
    const path = renamedTo ? renamedTo.slice('rename to '.length) : file.path;
    return {
      hunks: parseHunks(file.lines),
      patch,
      path,
      presentation: filePresentation(path),
      stats,
      status
    };
  }

  function filePatchesFromPatch(patch) {
    const files = [];
    let current = null;
    for (const line of String(patch || '').split('\n')) {
      const match = line.match(/^diff --git a\/(.+) b\/(.+)$/u);
      if (match) {
        if (current) files.push(completedFile(current));
        current = { path: match[2], lines: [line] };
      } else if (current) {
        current.lines.push(line);
      }
    }
    if (current) files.push(completedFile(current));
    return files;
  }

  function unchangedLinesBefore(hunk, previousHunk = null) {
    if (!previousHunk) return Math.max(0, Math.max(hunk.oldStart, hunk.newStart) - 1);
    const previousOldEnd = previousHunk.oldStart + previousHunk.oldCount;
    const previousNewEnd = previousHunk.newStart + previousHunk.newCount;
    return Math.max(0, hunk.oldStart - previousOldEnd, hunk.newStart - previousNewEnd);
  }

  function commentPattern(extension) {
    if (['py', 'sh', 'nix', 'yaml', 'yml'].includes(extension)) return /#.*$/gu;
    if (['css'].includes(extension)) return /\/\*.*?(?:\*\/|$)/gu;
    if (['html'].includes(extension)) return /<!--.*?(?:-->|$)/gu;
    return /\/\/.*$/gu;
  }

  function syntaxTokens(text, path) {
    const source = String(text || '');
    const { extension } = filePresentation(path);
    const matches = [];
    const patterns = [
      ['comment', commentPattern(extension)],
      ['string', /"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|`(?:\\.|[^`\\])*`/gu],
      ['number', /\b\d+(?:\.\d+)?\b/gu],
      ['keyword', /\b[A-Za-z_][A-Za-z0-9_]*\b/gu]
    ];
    for (const [kind, pattern] of patterns) {
      for (const match of source.matchAll(pattern)) {
        if (kind === 'keyword' && !KEYWORDS.has(match[0])) continue;
        matches.push({ end: match.index + match[0].length, kind, start: match.index, text: match[0] });
      }
    }
    matches.sort((left, right) => left.start - right.start || right.end - left.end);
    const tokens = [];
    let cursor = 0;
    for (const match of matches) {
      if (match.start < cursor) continue;
      if (match.start > cursor) tokens.push({ kind: 'plain', text: source.slice(cursor, match.start) });
      tokens.push({ kind: match.kind, text: match.text });
      cursor = match.end;
    }
    if (cursor < source.length) tokens.push({ kind: 'plain', text: source.slice(cursor) });
    if (tokens.length === 0) tokens.push({ kind: 'plain', text: source });
    return tokens;
  }

  const api = {
    filePatchesFromPatch,
    filePresentation,
    lineCountsFromPatch,
    syntaxTokens,
    unchangedLinesBefore
  };
  globalScope.CaosChanges = api;
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
}(typeof window === 'undefined' ? globalThis : window));
