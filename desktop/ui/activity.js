(function activityHelpers(globalScope) {
  function pluralized(count, singular, plural = `${singular}s`) {
    return `${count} ${count === 1 ? singular : plural}`;
  }

  function summaryRemainder(call) {
    const summary = String(call?.summary || '').trim();
    const name = String(call?.name || '').trim();
    if (!summary) return '';
    if (name && summary.toLowerCase().startsWith(`${name.toLowerCase()} `)) {
      return summary.slice(name.length + 1);
    }
    return summary.replace(/^\$\s*/u, '');
  }

  function toolDescription(call) {
    const detail = summaryRemainder(call);
    switch (call?.name) {
      case 'bash': return detail ? `Ran ${detail}` : 'Ran a command';
      case 'read': return detail ? `Read ${detail}` : 'Read a file';
      case 'write': return detail ? `Wrote ${detail}` : 'Wrote a file';
      case 'edit': return detail ? `Edited ${detail}` : 'Edited a file';
      case 'ls': return detail ? `Listed ${detail}` : 'Listed files';
      case 'grep': return detail ? `Searched ${detail}` : 'Searched files';
      default: {
        if (call?.summary) return String(call.summary);
        return call?.name ? `Used ${call.name}` : 'Used a tool';
      }
    }
  }

  function activityGroupSummary(calls) {
    const commands = calls.filter((call) => call.name === 'bash');
    const otherCalls = calls.filter((call) => call.name !== 'bash');
    const parts = [];
    if (commands.length > 0) parts.push(pluralized(commands.length, 'command'));
    for (const call of otherCalls.slice(0, 2)) parts.push(toolDescription(call));
    const described = commands.length + Math.min(otherCalls.length, 2);
    if (calls.length > described) parts.push(`+${calls.length - described} more`);
    return parts.join(', ') || 'Working';
  }

  function activityGroupComplete(entry) {
    return Boolean(entry?.calls?.length) && entry.calls.every((call) => call.result);
  }

  function scrollPositionIsNearBottom(position, threshold = 24) {
    const remaining = position.scrollHeight - position.clientHeight - position.scrollTop;
    return remaining <= threshold;
  }

  function transientTurnEntries(history, turnStart) {
    const tail = history.slice(turnStart + 1);
    let finalAgent = -1;
    for (let index = tail.length - 1; index >= 0; index -= 1) {
      if (tail[index].role === 'agent') {
        finalAgent = index;
        break;
      }
    }
    return tail.filter((entry, index) => (
      (entry.role === 'activity' && entry.calls.length > 0)
      || (entry.role === 'agent' && index !== finalAgent)
    ));
  }

  function mergeTransientTurnEntries(history, entries) {
    if (!entries?.length) return history;
    let finalAgent = -1;
    for (let index = history.length - 1; index >= 0; index -= 1) {
      if (history[index].role === 'agent') {
        finalAgent = index;
        break;
      }
    }
    if (finalAgent < 0) return history;
    return [
      ...history.slice(0, finalAgent),
      ...entries,
      ...history.slice(finalAgent)
    ];
  }

  const api = {
    activityGroupComplete,
    activityGroupSummary,
    mergeTransientTurnEntries,
    scrollPositionIsNearBottom,
    toolDescription,
    transientTurnEntries
  };
  globalScope.CaosActivity = api;
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
}(typeof window === 'undefined' ? globalThis : window));
