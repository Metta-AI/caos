(function changeHelpers(globalScope) {
  function filePatchesFromPatch(patch) {
    const files = [];
    let current = null;
    for (const line of String(patch || '').split('\n')) {
      const match = line.match(/^diff --git a\/(.+) b\/(.+)$/u);
      if (match) {
        if (current) files.push({ path: current.path, patch: current.lines.join('\n') });
        current = { path: match[2], lines: [line] };
      } else if (current) {
        current.lines.push(line);
      }
    }
    if (current) files.push({ path: current.path, patch: current.lines.join('\n') });
    return files;
  }

  const api = { filePatchesFromPatch };
  globalScope.CaosChanges = api;
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
}(typeof window === 'undefined' ? globalThis : window));
