import parseDiff from 'parse-diff';

import { codeTokens } from './highlight.js';

const FILE_BADGES = new Map([
  ['css', 'CSS'], ['go', 'GO'], ['html', 'HTML'], ['js', 'JS'], ['jsx', 'JS'],
  ['json', 'JSON'], ['md', 'MD'], ['nix', 'NIX'], ['py', 'PY'], ['rs', 'RS'],
  ['sh', 'SH'], ['toml', 'TOML'], ['ts', 'TS'], ['tsx', 'TS'], ['yaml', 'YML'],
  ['yml', 'YML']
]);

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

function normalizedLine(change) {
  if (change.content === '\\ No newline at end of file') {
    return { kind: 'notice', oldLine: null, newLine: null, text: 'No newline at end of file' };
  }
  if (change.type === 'add') {
    return { kind: 'add', oldLine: null, newLine: change.ln, text: change.content.slice(1) };
  }
  if (change.type === 'del') {
    return { kind: 'delete', oldLine: change.ln, newLine: null, text: change.content.slice(1) };
  }
  return {
    kind: 'context',
    oldLine: change.ln1,
    newLine: change.ln2,
    text: change.content.slice(1)
  };
}

function filePatchesFromPatch(patch) {
  return parseDiff(String(patch || '')).map((file) => {
    const from = file.from && file.from !== '/dev/null' ? file.from : null;
    const to = file.to && file.to !== '/dev/null' ? file.to : null;
    const path = to || from || 'unknown';
    const status = file.new
      ? 'added'
      : file.deleted
        ? 'deleted'
        : from && to && from !== to
          ? 'renamed'
          : 'modified';
    return {
      hunks: file.chunks.map((chunk) => ({
        oldStart: chunk.oldStart,
        oldCount: chunk.oldLines,
        newStart: chunk.newStart,
        newCount: chunk.newLines,
        context: chunk.content.replace(/^@@[^@]*@@\s?/u, ''),
        lines: chunk.changes.map(normalizedLine)
      })),
      path,
      presentation: filePresentation(path),
      stats: { additions: file.additions, deletions: file.deletions },
      status
    };
  });
}

function lineCounts(files) {
  return files.reduce(
    (total, file) => ({
      additions: total.additions + file.stats.additions,
      deletions: total.deletions + file.stats.deletions
    }),
    { additions: 0, deletions: 0 }
  );
}

function unchangedLinesBefore(hunk, previousHunk = null) {
  if (!previousHunk) return Math.max(0, Math.max(hunk.oldStart, hunk.newStart) - 1);
  const previousOldEnd = previousHunk.oldStart + previousHunk.oldCount;
  const previousNewEnd = previousHunk.newStart + previousHunk.newCount;
  return Math.max(0, hunk.oldStart - previousOldEnd, hunk.newStart - previousNewEnd);
}

function highlightedHunkLines(hunk, path) {
  const oldSource = [];
  const newSource = [];
  for (const line of hunk.lines) {
    if (line.kind === 'context' || line.kind === 'delete') oldSource.push(line.text);
    if (line.kind === 'context' || line.kind === 'add') newSource.push(line.text);
  }
  const oldTokens = codeTokens(oldSource.join('\n'), path);
  const newTokens = codeTokens(newSource.join('\n'), path);
  let oldIndex = 0;
  let newIndex = 0;
  return hunk.lines.map((line) => {
    if (line.kind === 'delete') return { ...line, tokens: oldTokens[oldIndex++] };
    if (line.kind === 'add') return { ...line, tokens: newTokens[newIndex++] };
    if (line.kind === 'context') {
      oldIndex += 1;
      return { ...line, tokens: newTokens[newIndex++] };
    }
    return { ...line, tokens: [{ content: line.text }] };
  });
}

export {
  filePatchesFromPatch,
  filePresentation,
  highlightedHunkLines,
  lineCounts,
  unchangedLinesBefore
};
