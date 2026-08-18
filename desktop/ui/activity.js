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

function activityGroupExpandable(entry) {
  return Boolean(entry?.calls?.length > 1)
    || Boolean(entry?.calls?.some((call) => call.name === 'bash'));
}

function scrollPositionIsNearBottom(position, threshold = 24) {
  const remaining = position.scrollHeight - position.clientHeight - position.scrollTop;
  return remaining <= threshold;
}

function sameToolCall(left, right) {
  return left?.toolUseId === right?.toolUseId
    && left?.request === right?.request
    && Number(left?.round || 0) === Number(right?.round || 0);
}

function replayedTurnEntries(events) {
  const entries = [];
  let group = null;
  const finishGroup = () => {
    if (!group) return;
    if (group.calls.length > 0) entries.push(group);
    group = null;
  };

  for (const event of events || []) {
    if (event.kind === 'assistantText' && event.text) {
      finishGroup();
      entries.push({
        role: 'agent',
        message: event.text,
        shortCommit: ''
      });
    } else if (event.kind === 'toolCall') {
      if (activityGroupComplete(group)) finishGroup();
      group ||= {
        role: 'activity',
        calls: [],
        expanded: false,
        running: false,
        status: ''
      };
      group.calls.push({ ...event });
    } else if (event.kind === 'toolResult') {
      let call = group?.calls.find((item) => sameToolCall(item, event));
      if (!call) {
        group ||= {
          role: 'activity',
          calls: [],
          expanded: false,
          running: false,
          status: ''
        };
        call = {
          kind: 'toolCall',
          stepCommit: event.stepCommit,
          request: event.request,
          round: event.round,
          toolUseId: event.toolUseId,
          name: 'result',
          summary: `result ${event.toolUseId}`
        };
        group.calls.push(call);
      }
      call.result = { ...event };
    }
  }
  finishGroup();
  return entries;
}

function mergeReplayedHistory(turns, turnEvents) {
  const eventsByTurn = new Map(
    (turnEvents || []).map((turn) => [turn.turnCommit, turn.events])
  );
  const consumed = new Set();
  const history = [];
  for (const turn of turns || []) {
    if (turn.role === 'agent' && eventsByTurn.has(turn.commit)) {
      consumed.add(turn.commit);
      history.push(...replayedTurnEntries(eventsByTurn.get(turn.commit)));
    }
    history.push(turn);
  }
  const durableActivity = (turnEvents || [])
    .filter((turn) => !consumed.has(turn.turnCommit))
    .flatMap((turn) => replayedTurnEntries(turn.events));
  if (durableActivity.length > 0) {
    let finalAgent = -1;
    history.forEach((entry, index) => {
      if (entry.role === 'agent') finalAgent = index;
    });
    history.splice(finalAgent < 0 ? history.length : finalAgent, 0, ...durableActivity);
  }
  return history;
}

export {
  activityGroupComplete,
  activityGroupExpandable,
  activityGroupSummary,
  mergeReplayedHistory,
  replayedTurnEntries,
  sameToolCall,
  scrollPositionIsNearBottom,
  toolDescription
};
