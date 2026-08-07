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

function replayedTurnEntries(events, timestampUnix) {
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
        shortCommit: '',
        timestampUnix
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
      let call = group?.calls.find((item) => item.toolUseId === event.toolUseId);
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
  const history = [];
  for (const turn of turns || []) {
    if (turn.role === 'agent') {
      history.push(...replayedTurnEntries(
        eventsByTurn.get(turn.commit),
        turn.timestampUnix
      ));
    }
    history.push(turn);
  }
  return history;
}

export {
  activityGroupComplete,
  activityGroupExpandable,
  activityGroupSummary,
  mergeReplayedHistory,
  replayedTurnEntries,
  scrollPositionIsNearBottom,
  toolDescription
};
