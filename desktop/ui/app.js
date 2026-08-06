const tauri = window.__TAURI__?.core;
const { renderMarkdown } = window.CaosMarkdown;
const { filePatchesFromPatch } = window.CaosChanges;
const {
  activityGroupComplete,
  activityGroupSummary,
  mergeTransientTurnEntries,
  scrollPositionIsNearBottom,
  toolDescription,
  transientTurnEntries
} = window.CaosActivity;
const DEFAULT_SIDEBAR_WIDTH = 226;
const MIN_SIDEBAR_WIDTH = 180;
const MAX_SIDEBAR_WIDTH = 420;
const SIDEBAR_WIDTH_KEY = 'caos.sidebarWidth';
const DEFAULT_INSPECTOR_WIDTH = 420;
const MIN_INSPECTOR_WIDTH = 280;
const MAX_INSPECTOR_WIDTH = 720;
const INSPECTOR_WIDTH_KEY = 'caos.inspectorWidth';
const UI_ZOOM_KEY = 'caos.uiZoom';
const MIN_UI_ZOOM = 0.8;
const MAX_UI_ZOOM = 1.6;
const UI_ZOOM_STEP = 0.1;
const PALETTE_COMMANDS = [
  { id: 'new', label: 'New conversation', shortcut: 'Ctrl+N', keywords: 'create start task', run: () => createConversation() },
  { id: 'chat', label: 'Focus conversation', shortcut: '', keywords: 'chat transcript close inspectors', run: () => closeInspectorPanes() },
  { id: 'changes', label: 'Toggle workspace changes', shortcut: 'Ctrl+Q', keywords: 'diff files pane', available: () => !elements.changesToggle.hidden, run: () => togglePane('changes') },
  { id: 'reload', label: 'Reload conversation', shortcut: 'Ctrl+R', keywords: 'refresh history', run: () => reloadSelectedConversation() },
  { id: 'rename', label: 'Rename conversation', shortcut: '/rename', keywords: 'title name', run: () => prefillCommand('/rename ') },
  { id: 'help', label: 'Show keyboard shortcuts', shortcut: 'Ctrl+H', keywords: 'help commands', run: () => setShortcutHelp(true) }
];

const state = {
  repo: null,
  conversations: [],
  selectedId: null,
  histories: new Map(),
  diffs: new Map(),
  selectedDiffFiles: new Map(),
  pendingActivityGroups: new Map(),
  transientTurnEntries: new Map(),
  turnStartIndexes: new Map(),
  running: new Set(),
  drafts: new Map(),
  panes: { changes: false },
  shortcutHelpOpen: false,
  commandPaletteOpen: false,
  commandPaletteSelection: 0,
  uiZoom: 1,
  sidebarWidth: DEFAULT_SIDEBAR_WIDTH,
  inspectorWidth: DEFAULT_INSPECTOR_WIDTH,
  resizingSidebar: false,
  resizingInspector: false,
  creatingConversation: false
};

const elements = {
  taskList: document.getElementById('task-list'),
  sidebarResizer: document.getElementById('sidebar-resizer'),
  inspector: document.getElementById('inspector'),
  inspectorResizer: document.getElementById('inspector-resizer'),
  changesPane: document.getElementById('changes-pane'),
  newTask: document.getElementById('new-task'),
  taskTitle: document.getElementById('task-title'),
  taskMeta: document.getElementById('task-meta'),
  commandPaletteButton: document.getElementById('command-palette-button'),
  changesToggle: document.getElementById('changes-toggle'),
  changeCount: document.getElementById('change-count'),
  transcript: document.getElementById('transcript'),
  transcriptScroll: document.getElementById('transcript-scroll'),
  composer: document.getElementById('composer'),
  prompt: document.getElementById('prompt'),
  sendButton: document.getElementById('send-button'),
  turnStatus: document.getElementById('turn-status'),
  fileList: document.getElementById('file-list'),
  diff: document.getElementById('diff'),
  shortcutHelp: document.getElementById('shortcut-help'),
  commandPalette: document.getElementById('command-palette'),
  commandPaletteQuery: document.getElementById('command-palette-query'),
  commandPaletteResults: document.getElementById('command-palette-results'),
  fatalError: document.getElementById('fatal-error'),
  fatalMessage: document.getElementById('fatal-message')
};

function selectedConversation() {
  return state.conversations.find((item) => item.id === state.selectedId) || null;
}

function automaticConversationTitle(message) {
  const title = message.trim().split(/\s+/u).join(' ');
  const characters = [...title];
  return characters.length <= 60 ? title : `${characters.slice(0, 59).join('')}…`;
}

function showFatal(error) {
  clearStartupLoading();
  elements.taskTitle.textContent = 'Repository unavailable';
  elements.taskMeta.textContent = '';
  elements.taskList.replaceChildren();
  elements.transcript.replaceChildren();
  elements.fatalMessage.textContent = String(error);
  elements.fatalError.hidden = false;
}

function clearStartupLoading() {
  elements.taskList.setAttribute('aria-busy', 'false');
}

function sidebarWidthBounds() {
  const inspectorWidth = state.panes.changes ? state.inspectorWidth : 0;
  return {
    min: MIN_SIDEBAR_WIDTH,
    max: Math.max(MIN_SIDEBAR_WIDTH, Math.min(MAX_SIDEBAR_WIDTH, window.innerWidth - inspectorWidth - 380))
  };
}

function setSidebarWidth(width, persist = false) {
  const bounds = sidebarWidthBounds();
  const next = Math.round(Math.min(bounds.max, Math.max(bounds.min, width)));
  state.sidebarWidth = next;
  document.documentElement.style.setProperty('--sidebar-width', `${next}px`);
  elements.sidebarResizer.setAttribute('aria-valuenow', String(next));
  elements.sidebarResizer.setAttribute('aria-valuemax', String(bounds.max));
  if (persist) {
    try {
      window.localStorage.setItem(SIDEBAR_WIDTH_KEY, String(next));
    } catch (_) {
      // A persisted width is a convenience; resizing still works without storage.
    }
  }
}

function restoreSidebarWidth() {
  let stored = DEFAULT_SIDEBAR_WIDTH;
  try {
    stored = Number(window.localStorage.getItem(SIDEBAR_WIDTH_KEY)) || stored;
  } catch (_) {
    // Use the default when storage is unavailable.
  }
  setSidebarWidth(stored);
}

function inspectorWidthBounds() {
  return {
    min: MIN_INSPECTOR_WIDTH,
    max: Math.max(
      MIN_INSPECTOR_WIDTH,
      Math.min(MAX_INSPECTOR_WIDTH, window.innerWidth - state.sidebarWidth - 380)
    )
  };
}

function setInspectorWidth(width, persist = false) {
  const bounds = inspectorWidthBounds();
  const next = Math.round(Math.min(bounds.max, Math.max(bounds.min, width)));
  state.inspectorWidth = next;
  document.documentElement.style.setProperty('--inspector-width', `${next}px`);
  elements.inspectorResizer.setAttribute('aria-valuenow', String(next));
  elements.inspectorResizer.setAttribute('aria-valuemax', String(bounds.max));
  if (persist) {
    try {
      window.localStorage.setItem(INSPECTOR_WIDTH_KEY, String(next));
    } catch (_) {
      // Resizing remains available when storage is unavailable.
    }
  }
}

function restoreInspectorWidth() {
  let stored = DEFAULT_INSPECTOR_WIDTH;
  try {
    stored = Number(window.localStorage.getItem(INSPECTOR_WIDTH_KEY)) || stored;
  } catch (_) {
    // Use the default when storage is unavailable.
  }
  setInspectorWidth(stored);
}

function normalizedUiZoom(scale) {
  const clamped = Math.min(MAX_UI_ZOOM, Math.max(MIN_UI_ZOOM, scale));
  return Math.round(clamped * 10) / 10;
}

function applyUiZoom(scale, persist = false) {
  const next = normalizedUiZoom(scale);
  state.uiZoom = next;
  const applyCssFallback = () => {
    document.documentElement.style.zoom = String(next);
  };
  if (tauri) {
    tauri.invoke('set_ui_zoom', { scale: next })
      .then(() => document.documentElement.style.removeProperty('zoom'))
      .catch(applyCssFallback);
  } else {
    applyCssFallback();
  }
  if (persist) {
    try {
      window.localStorage.setItem(UI_ZOOM_KEY, String(next));
    } catch (_) {
      // Zoom still applies for the current session when storage is unavailable.
    }
  }
}

function restoreUiZoom() {
  let stored = 1;
  try {
    stored = Number(window.localStorage.getItem(UI_ZOOM_KEY)) || stored;
  } catch (_) {
    // Use the default zoom when storage is unavailable.
  }
  applyUiZoom(stored);
}

function setStatus(text = '') {
  elements.turnStatus.textContent = text;
}

function setShortcutHelp(open) {
  if (open && state.commandPaletteOpen) setCommandPalette(false);
  state.shortcutHelpOpen = open;
  elements.shortcutHelp.hidden = !open;
  if (!open) elements.prompt.focus();
}

function matchingPaletteCommands() {
  const terms = elements.commandPaletteQuery.value.toLowerCase().trim().split(/\s+/).filter(Boolean);
  return PALETTE_COMMANDS.filter((command) => {
    if (command.available && !command.available()) return false;
    const haystack = `${command.label} ${command.keywords}`.toLowerCase();
    return terms.every((term) => haystack.includes(term));
  });
}

function selectPaletteIndex(index) {
  const buttons = [...elements.commandPaletteResults.querySelectorAll('.command-palette-item')];
  if (buttons.length === 0) {
    state.commandPaletteSelection = 0;
    return;
  }
  state.commandPaletteSelection = (index + buttons.length) % buttons.length;
  buttons.forEach((button, buttonIndex) => {
    const selected = buttonIndex === state.commandPaletteSelection;
    button.classList.toggle('is-selected', selected);
    button.setAttribute('aria-selected', String(selected));
  });
}

function executePaletteCommand(index = state.commandPaletteSelection) {
  const command = matchingPaletteCommands()[index];
  if (!command) return;
  setCommandPalette(false);
  command.run();
}

function renderCommandPalette() {
  const commands = matchingPaletteCommands();
  elements.commandPaletteResults.replaceChildren();
  if (commands.length === 0) {
    const empty = document.createElement('div');
    empty.className = 'command-palette-empty';
    empty.textContent = 'No matching commands';
    elements.commandPaletteResults.append(empty);
    state.commandPaletteSelection = 0;
    return;
  }
  state.commandPaletteSelection = Math.min(state.commandPaletteSelection, commands.length - 1);
  commands.forEach((command, index) => {
    const button = document.createElement('button');
    button.type = 'button';
    button.className = 'command-palette-item';
    button.setAttribute('role', 'option');
    const label = document.createElement('span');
    label.className = 'command-palette-item-label';
    label.textContent = command.label;
    const shortcut = document.createElement('span');
    shortcut.className = 'command-palette-item-shortcut';
    shortcut.textContent = command.shortcut;
    button.append(label, shortcut);
    button.addEventListener('mousemove', () => selectPaletteIndex(index));
    button.addEventListener('click', () => executePaletteCommand(index));
    elements.commandPaletteResults.append(button);
  });
  selectPaletteIndex(state.commandPaletteSelection);
}

function setCommandPalette(open) {
  if (open && state.shortcutHelpOpen) {
    state.shortcutHelpOpen = false;
    elements.shortcutHelp.hidden = true;
  }
  state.commandPaletteOpen = open;
  elements.commandPalette.hidden = !open;
  elements.commandPaletteButton.setAttribute('aria-expanded', String(open));
  if (open) {
    elements.commandPaletteQuery.value = '';
    state.commandPaletteSelection = 0;
    renderCommandPalette();
    requestAnimationFrame(() => elements.commandPaletteQuery.focus());
  } else {
    elements.prompt.focus();
  }
}

function resizePrompt() {
  elements.prompt.style.height = 'auto';
  elements.prompt.style.height = `${Math.min(elements.prompt.scrollHeight, 170)}px`;
}

function transcriptIsNearBottom() {
  return scrollPositionIsNearBottom(elements.transcriptScroll);
}

function scrollTranscriptToBottom() {
  requestAnimationFrame(() => {
    elements.transcriptScroll.scrollTop = elements.transcriptScroll.scrollHeight;
  });
}

function saveSelectedDraft() {
  if (state.selectedId) state.drafts.set(state.selectedId, elements.prompt.value);
}

function restoreSelectedDraft() {
  elements.prompt.value = state.drafts.get(state.selectedId) || '';
  resizePrompt();
}

function clearComposer() {
  state.drafts.set(state.selectedId, '');
  elements.prompt.value = '';
  resizePrompt();
}

function prefillCommand(command) {
  state.drafts.set(state.selectedId, command);
  elements.prompt.value = command;
  resizePrompt();
  elements.prompt.focus({ preventScroll: true });
  elements.prompt.setSelectionRange(command.length, command.length);
}

function renderSidebar() {
  elements.taskList.setAttribute('aria-busy', 'false');
  elements.taskList.replaceChildren();
  const selectedIndex = state.conversations.findIndex((item) => item.id === state.selectedId);
  const previousIndex = state.conversations.length > 1
    ? (selectedIndex - 1 + state.conversations.length) % state.conversations.length
    : -1;
  const nextIndex = state.conversations.length > 1
    ? (selectedIndex + 1) % state.conversations.length
    : -1;
  state.conversations.forEach((conversation, index) => {
    const button = document.createElement('button');
    button.type = 'button';
    button.className = 'task-item';
    if (conversation.id === state.selectedId) button.classList.add('is-selected');
    const title = document.createElement('span');
    title.className = 'task-item-title';
    title.textContent = conversation.title;
    button.append(title);
    const shortcuts = [];
    if (index < 9) shortcuts.push(`Ctrl+${index + 1}`);
    if (index === nextIndex) shortcuts.push('Ctrl+↓');
    if (index === previousIndex) shortcuts.push('Ctrl+↑');
    if (shortcuts.length > 0) {
      const shortcut = document.createElement('span');
      shortcut.className = 'shortcut-hint task-shortcut-hint';
      shortcut.setAttribute('aria-hidden', 'true');
      shortcut.textContent = shortcuts.join(' · ');
      button.append(shortcut);
    }
    if (state.running.has(conversation.id)) {
      const status = document.createElement('span');
      status.className = 'task-status is-running';
      status.setAttribute('role', 'status');
      status.setAttribute('aria-label', 'Running');
      status.title = 'Running';
      button.append(status);
    }
    button.addEventListener('click', () => selectConversation(conversation.id));
    elements.taskList.append(button);
  });
}

function renderHeader() {
  const conversation = selectedConversation();
  elements.taskMeta.replaceChildren();
  elements.taskMeta.removeAttribute('title');
  if (!conversation) {
    elements.taskTitle.textContent = state.repo?.repoName || 'CAOS';
    elements.taskTitle.removeAttribute('title');
    return;
  }
  elements.taskTitle.textContent = conversation.title;
  elements.taskTitle.title = conversation.title;
  if (conversation.draft && !conversation.started) return;
  if (!conversation.shortHead) return;
  const commit = iconElement([
    ['path', { d: 'M3 12h5' }],
    ['circle', { cx: '12', cy: '12', r: '4' }],
    ['path', { d: 'M16 12h5' }]
  ]);
  const hash = document.createElement('code');
  hash.textContent = conversation.shortHead;
  elements.taskMeta.title = conversation.head || conversation.shortHead;
  elements.taskMeta.append(commit, hash);
}

function iconElement(children) {
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  svg.setAttribute('aria-hidden', 'true');
  svg.setAttribute('viewBox', '0 0 24 24');
  for (const [tag, attributes] of children) {
    const child = document.createElementNS('http://www.w3.org/2000/svg', tag);
    for (const [name, value] of Object.entries(attributes)) child.setAttribute(name, value);
    svg.append(child);
  }
  return svg;
}

function fallbackCopyMessage(message) {
  const fallback = document.createElement('textarea');
  fallback.value = message;
  fallback.style.position = 'fixed';
  fallback.style.opacity = '0';
  document.body.append(fallback);
  fallback.select();
  const copied = document.execCommand('copy');
  fallback.remove();
  if (!copied) throw new Error('copy command was rejected');
}

async function copyMessage(message, button) {
  try {
    if (navigator.clipboard?.writeText) {
      try {
        await navigator.clipboard.writeText(message);
      } catch (_) {
        fallbackCopyMessage(message);
      }
    } else {
      fallbackCopyMessage(message);
    }
    button.setAttribute('aria-label', 'Copied message');
    button.title = 'Copied';
    window.setTimeout(() => {
      button.setAttribute('aria-label', 'Copy message');
      button.title = 'Copy message';
    }, 1200);
  } catch (_) {
    button.title = 'Could not copy message';
  }
}

function messageActionsElement(entry) {
  const actions = document.createElement('div');
  actions.className = 'message-actions';

  if (Number.isFinite(entry.timestampUnix)) {
    const date = new Date(entry.timestampUnix * 1000);
    const time = document.createElement('time');
    time.className = 'message-action-meta';
    time.dateTime = date.toISOString();
    time.title = date.toLocaleString();
    time.textContent = date.toLocaleTimeString(undefined, {
      hour: 'numeric', minute: '2-digit'
    });
    actions.append(time);
  }

  if (entry.shortCommit) {
    const commit = document.createElement('span');
    commit.className = 'message-action-meta';
    commit.title = entry.commit || entry.shortCommit;
    commit.append(iconElement([
      ['path', { d: 'M3 12h5' }],
      ['circle', { cx: '12', cy: '12', r: '4' }],
      ['path', { d: 'M16 12h5' }]
    ]));
    const hash = document.createElement('code');
    hash.textContent = entry.shortCommit;
    commit.append(hash);
    actions.append(commit);
  }

  const copy = document.createElement('button');
  copy.type = 'button';
  copy.className = 'message-action-button';
  copy.setAttribute('aria-label', 'Copy message');
  copy.title = 'Copy message';
  copy.append(iconElement([
    ['rect', { x: '9', y: '9', width: '10', height: '10', rx: '2' }],
    ['path', { d: 'M15 6V5a2 2 0 0 0-2-2H5a2 2 0 0 0-2 2v8a2 2 0 0 0 2 2h1' }]
  ]));
  copy.addEventListener('click', () => copyMessage(entry.message, copy));
  actions.append(copy);

  return actions;
}

function activityIcon(name) {
  if (name === 'bash') {
    return iconElement([
      ['rect', { x: '3', y: '4', width: '18', height: '16', rx: '3' }],
      ['path', { d: 'm7 9 3 3-3 3' }],
      ['path', { d: 'M13 15h4' }]
    ]);
  }
  if (name === 'grep') {
    return iconElement([
      ['circle', { cx: '10.5', cy: '10.5', r: '5.5' }],
      ['path', { d: 'm15 15 5 5' }]
    ]);
  }
  if (name === 'read' || name === 'ls') {
    return iconElement([
      ['path', { d: 'M4 5.5A2.5 2.5 0 0 1 6.5 3H18a2 2 0 0 1 2 2v14H6.5A2.5 2.5 0 0 1 4 16.5Z' }],
      ['path', { d: 'M4 16.5A2.5 2.5 0 0 1 6.5 14H20' }]
    ]);
  }
  return iconElement([
    ['path', { d: 'M4 20h4l11-11a2.8 2.8 0 0 0-4-4L4 16Z' }],
    ['path', { d: 'm13.5 6.5 4 4' }]
  ]);
}

function activityGroupElement(entry) {
  const section = document.createElement('section');
  section.className = 'inline-activity';
  if (entry.running) section.classList.add('is-running');

  const hasCalls = entry.calls.length > 0;
  const expandable = entry.calls.length > 1;
  const toggle = document.createElement(expandable ? 'button' : 'div');
  toggle.className = 'inline-activity-toggle';
  if (expandable) {
    toggle.type = 'button';
    toggle.setAttribute('aria-expanded', String(entry.expanded));
    const chevron = iconElement([['path', { d: 'm9 18 6-6-6-6' }]]);
    chevron.classList.add('inline-activity-chevron');
    toggle.append(chevron);
  } else {
    toggle.setAttribute('role', 'status');
  }
  const label = document.createElement('span');
  label.className = 'inline-activity-label';
  label.textContent = hasCalls
    ? activityGroupSummary(entry.calls)
    : entry.status || 'Working';
  if (entry.running) {
    const spinner = document.createElement('span');
    spinner.className = 'loading-spinner inline-activity-spinner';
    spinner.setAttribute('aria-hidden', 'true');
    toggle.append(spinner);
  }
  toggle.append(label);

  const list = document.createElement('div');
  list.className = 'inline-activity-list';
  list.setAttribute('role', 'list');
  list.hidden = !expandable || !entry.expanded;
  for (const call of entry.calls) {
    const row = document.createElement('div');
    row.className = 'inline-activity-item';
    row.setAttribute('role', 'listitem');
    if (call.result?.isError) row.classList.add('is-error');
    const icon = activityIcon(call.name);
    icon.classList.add('inline-activity-icon');
    const description = document.createElement('span');
    description.className = 'inline-activity-description';
    description.textContent = toolDescription(call);
    row.append(icon);
    if (!call.result && entry.running) {
      row.classList.add('is-running');
      const spinner = document.createElement('span');
      spinner.className = 'loading-spinner inline-activity-item-spinner';
      spinner.setAttribute('aria-label', 'Running');
      row.append(spinner);
    }
    row.append(description);
    list.append(row);
  }

  if (expandable) {
    toggle.addEventListener('click', () => {
      const keepBottomAnchored = !entry.expanded && transcriptIsNearBottom();
      entry.expanded = !entry.expanded;
      toggle.setAttribute('aria-expanded', String(entry.expanded));
      list.hidden = !entry.expanded;
      if (keepBottomAnchored) scrollTranscriptToBottom();
    });
  }
  section.append(toggle, list);
  return section;
}

function messageElement(entry) {
  if (entry.role === 'activity') return activityGroupElement(entry);
  const article = document.createElement('article');
  article.className = `message message-${entry.role}`;
  if (entry.failed) article.classList.add('is-failed');
  if (entry.role === 'human') {
    const bubble = document.createElement('div');
    bubble.className = 'message-bubble';
    renderMarkdown(bubble, entry.message);
    article.append(bubble, messageActionsElement(entry));
    return article;
  }
  const text = document.createElement('div');
  text.className = 'message-text';
  renderMarkdown(text, entry.message);
  article.append(text, messageActionsElement(entry));
  return article;
}

function beginActivityGroup(id, status = 'Preparing…') {
  const history = state.histories.get(id) || [];
  const entry = {
    role: 'activity',
    calls: [],
    expanded: true,
    running: true,
    status
  };
  history.push(entry);
  state.histories.set(id, history);
  state.pendingActivityGroups.set(id, entry);
  return entry;
}

function finishActivityGroup(id) {
  const entry = state.pendingActivityGroups.get(id);
  if (!entry) return;
  const history = state.histories.get(id) || [];
  if (entry.calls.length === 0) {
    const index = history.indexOf(entry);
    if (index >= 0) history.splice(index, 1);
  } else {
    entry.running = false;
    entry.expanded = false;
  }
  state.pendingActivityGroups.delete(id);
}

function renderTranscriptLoading() {
  elements.transcript.replaceChildren();
  const loading = document.createElement('div');
  loading.className = 'startup-loading';
  loading.setAttribute('role', 'status');
  const spinner = document.createElement('span');
  spinner.className = 'loading-spinner';
  spinner.setAttribute('aria-hidden', 'true');
  const label = document.createElement('span');
  label.textContent = 'Loading conversation…';
  loading.append(spinner, label);
  elements.transcript.append(loading);
}

function renderTranscript({ scrollToBottom = false } = {}) {
  const history = state.histories.get(state.selectedId) || [];
  elements.transcript.replaceChildren();
  if (history.length === 0) {
    const empty = document.createElement('div');
    empty.className = 'empty-chat';
    empty.textContent = 'Start a task in this repository.';
    elements.transcript.append(empty);
  } else {
    for (const entry of history) elements.transcript.append(messageElement(entry));
  }
  if (scrollToBottom) {
    scrollTranscriptToBottom();
  }
}

function updateInspectorLayout() {
  const changesOpen = state.panes.changes;
  elements.inspector.hidden = !changesOpen;
  elements.changesPane.hidden = !changesOpen;
  for (const button of document.querySelectorAll('[data-pane]')) {
    const open = state.panes[button.dataset.pane];
    button.classList.toggle('is-open', open);
    button.setAttribute('aria-expanded', String(open));
  }
  if (!changesOpen) {
    setSidebarWidth(state.sidebarWidth);
    return;
  }
  setInspectorWidth(state.inspectorWidth);
  setSidebarWidth(state.sidebarWidth);
  loadDiff(state.selectedId);
}

function setPaneOpen(pane, open) {
  state.panes[pane] = open;
  updateInspectorLayout();
}

function togglePane(pane) {
  if (pane === 'changes' && elements.changesToggle.hidden) return;
  setPaneOpen(pane, !state.panes[pane]);
}

function closeInspectorPanes() {
  state.panes.changes = false;
  updateInspectorLayout();
  elements.prompt.focus({ preventScroll: true });
}

async function selectConversation(id) {
  if (id === state.selectedId) {
    elements.prompt.focus({ preventScroll: true });
    return;
  }
  saveSelectedDraft();
  state.selectedId = id;
  renderSidebar();
  renderHeader();
  restoreSelectedDraft();
  setStatus('');
  state.panes.changes = false;
  updateInspectorLayout();
  elements.changesToggle.hidden = true;
  renderChangeCount(null);
  if (!state.histories.has(id)) {
    renderTranscriptLoading();
    await loadHistory(id);
  }
  if (state.selectedId !== id) return;
  renderTranscript({ scrollToBottom: true });
  loadDiff(id);
  elements.sendButton.disabled = state.running.has(id);
  elements.prompt.focus({ preventScroll: true });
}

async function loadHistory(id, force = false) {
  if (!id || (!force && state.histories.has(id))) return;
  try {
    const history = await tauri.invoke('get_history', { conversation: id });
    state.histories.set(
      id,
      mergeTransientTurnEntries(history, state.transientTurnEntries.get(id))
    );
  } catch (error) {
    setStatus(String(error));
  }
}

async function reloadSelectedConversation() {
  const id = state.selectedId;
  if (!id || state.running.has(id)) return;
  setStatus('Reloading…');
  await loadHistory(id, true);
  if (state.selectedId !== id) return;
  renderTranscript();
  await loadDiff(id, true);
  if (state.selectedId === id) setStatus('Reloaded');
}

async function renameSelectedConversation(requestedTitle) {
  const conversation = selectedConversation();
  if (!conversation || state.running.has(conversation.id)) return;
  setStatus('Renaming…');
  try {
    const title = await tauri.invoke('rename_conversation', {
      conversation: conversation.id,
      title: requestedTitle
    });
    conversation.title = title;
    renderSidebar();
    renderHeader();
    setStatus(`Renamed to “${title}”`);
  } catch (error) {
    setStatus(String(error));
  } finally {
    elements.prompt.focus({ preventScroll: true });
  }
}

function selectRelativeConversation(amount) {
  if (state.conversations.length < 2) return;
  const selected = state.conversations.findIndex((item) => item.id === state.selectedId);
  const next = (selected + amount + state.conversations.length) % state.conversations.length;
  selectConversation(state.conversations[next].id);
}

function lineCountsFromPatch(patch) {
  let additions = 0;
  let deletions = 0;
  for (const line of patch.split('\n')) {
    if (line.startsWith('+') && !line.startsWith('+++')) additions += 1;
    if (line.startsWith('-') && !line.startsWith('---')) deletions += 1;
  }
  return { additions, deletions };
}

function renderPatch(patch) {
  elements.diff.replaceChildren();
  for (const line of patch.split('\n')) {
    const row = document.createElement('span');
    row.className = 'diff-line';
    if (line.startsWith('+') && !line.startsWith('+++')) row.classList.add('is-add');
    if (line.startsWith('-') && !line.startsWith('---')) row.classList.add('is-delete');
    if (line.startsWith('diff ') || line.startsWith('index ') || line.startsWith('@@')) row.classList.add('is-meta');
    row.textContent = `${line}\n`;
    elements.diff.append(row);
  }
  elements.diff.scrollTop = 0;
  elements.diff.scrollLeft = 0;
}

function renderChangeCount(stats) {
  elements.changeCount.replaceChildren();
  elements.changeCount.removeAttribute('aria-label');
  if (!stats) return;
  const additions = document.createElement('span');
  additions.className = 'change-stat is-add';
  additions.textContent = `+${stats.additions}`;
  const deletions = document.createElement('span');
  deletions.className = 'change-stat is-delete';
  deletions.textContent = `-${stats.deletions}`;
  elements.changeCount.setAttribute(
    'aria-label',
    `${stats.additions} lines added, ${stats.deletions} lines deleted`
  );
  elements.changeCount.append(additions, deletions);
}

function renderDiff(payload) {
  const patch = payload?.patch || '';
  const hasChanges = patch.trim().length > 0;
  elements.changesToggle.hidden = !hasChanges;
  if (!hasChanges && state.panes.changes) {
    state.panes.changes = false;
    updateInspectorLayout();
  }
  elements.changesPane.classList.toggle('is-empty', !hasChanges);
  elements.fileList.replaceChildren();
  elements.diff.replaceChildren();
  const files = filePatchesFromPatch(patch);
  renderChangeCount(lineCountsFromPatch(patch));
  if (!hasChanges) {
    const empty = document.createElement('div');
    empty.className = 'panel-empty';
    empty.textContent = 'No workspace changes.';
    elements.diff.append(empty);
    return;
  }
  const conversationId = state.selectedId;
  const requestedPath = state.selectedDiffFiles.get(conversationId);
  const selectedFile = files.find((file) => file.path === requestedPath) || files[0];
  state.selectedDiffFiles.set(conversationId, selectedFile.path);
  files.forEach((file) => {
    const button = document.createElement('button');
    button.type = 'button';
    button.className = 'file-button';
    const selected = file.path === selectedFile.path;
    button.classList.toggle('is-active', selected);
    button.setAttribute('aria-pressed', String(selected));
    button.textContent = file.path;
    button.title = file.path;
    button.addEventListener('click', () => {
      if (state.selectedId !== conversationId) return;
      state.selectedDiffFiles.set(conversationId, file.path);
      for (const candidate of elements.fileList.querySelectorAll('.file-button')) {
        const active = candidate === button;
        candidate.classList.toggle('is-active', active);
        candidate.setAttribute('aria-pressed', String(active));
      }
      renderPatch(file.patch);
    });
    elements.fileList.append(button);
  });
  renderPatch(selectedFile.patch);
}

async function loadDiff(id, force = false) {
  if (!id) return;
  if (!force && state.diffs.has(id)) {
    if (state.selectedId === id) renderDiff(state.diffs.get(id));
    return;
  }
  if (state.selectedId === id) {
    elements.changesPane.classList.add('is-empty');
    renderChangeCount(null);
    elements.diff.textContent = 'Loading changes…';
  }
  try {
    const payload = await tauri.invoke('get_diff', { conversation: id });
    state.diffs.set(id, payload);
    if (state.selectedId === id) renderDiff(payload);
  } catch (error) {
    if (state.selectedId === id) {
      elements.changesPane.classList.add('is-empty');
      renderChangeCount(null);
      elements.diff.textContent = String(error);
    }
  }
}

function handleTurnEvent(id, event) {
  let transcriptChanged = false;
  if (event.kind === 'phaseStarted') {
    const group = state.pendingActivityGroups.get(id);
    if (group && group.calls.length === 0) {
      group.status = event.phase === 'model' ? 'Thinking…' : 'Preparing…';
      transcriptChanged = true;
    }
  } else if (event.kind === 'status') {
    const group = state.pendingActivityGroups.get(id);
    if (group && group.calls.length === 0) {
      group.status = event.text || 'Working…';
      transcriptChanged = true;
    }
  } else if (event.kind === 'toolCall') {
    let group = state.pendingActivityGroups.get(id);
    if (activityGroupComplete(group)) {
      finishActivityGroup(id);
      group = null;
    }
    group ||= beginActivityGroup(id, 'Working…');
    group.calls.push(event);
    group.running = true;
    group.expanded = true;
    transcriptChanged = true;
  } else if (event.kind === 'toolResult') {
    const history = state.histories.get(id) || [];
    for (const entry of history) {
      if (entry.role !== 'activity') continue;
      const call = entry.calls.find((item) => item.toolUseId === event.toolUseId);
      if (call) {
        call.result = event;
        if (entry === state.pendingActivityGroups.get(id) && activityGroupComplete(entry)) {
          entry.running = false;
        }
        transcriptChanged = true;
        break;
      }
    }
  } else if (event.kind === 'assistantText' && event.text) {
    const history = state.histories.get(id) || [];
    finishActivityGroup(id);
    history.push({
      role: 'agent',
      message: event.text,
      shortCommit: '',
      timestampUnix: Math.floor(Date.now() / 1000)
    });
    state.histories.set(id, history);
    transcriptChanged = true;
  }
  if (state.selectedId === id) {
    if (transcriptChanged) renderTranscript({ scrollToBottom: true });
  }
}

async function refreshConversations(selectedId) {
  const persisted = await tauri.invoke('get_conversations');
  const drafts = state.conversations.filter((item) => item.draft && item.id !== selectedId);
  state.conversations = [...persisted, ...drafts];
  if (!state.conversations.some((item) => item.id === selectedId)) {
    const persistedSelected = persisted.find((item) => item.id === selectedId);
    if (persistedSelected) state.conversations.unshift(persistedSelected);
  }
}

async function sendCurrentMessage() {
  const message = elements.prompt.value.trim();
  if (message === '/commands') {
    clearComposer();
    setCommandPalette(true);
    return;
  }
  if (message === '/help') {
    clearComposer();
    setShortcutHelp(true);
    return;
  }
  const rename = message.match(/^\/(?:rename|title)(?:\s+(.*))?$/s);
  if (rename) {
    clearComposer();
    if (!rename[1]?.trim()) {
      setStatus('Usage: /rename <new title>');
      elements.prompt.focus({ preventScroll: true });
      return;
    }
    await renameSelectedConversation(rename[1]);
    return;
  }
  const conversation = selectedConversation();
  if (!conversation || !message || state.running.has(conversation.id)) return;
  const id = conversation.id;
  if (conversation.draft && !conversation.started) {
    conversation.started = true;
    if (conversation.title === 'New conversation') {
      conversation.title = automaticConversationTitle(message);
    }
    renderHeader();
  }
  const history = state.histories.get(id) || [];
  state.transientTurnEntries.delete(id);
  state.turnStartIndexes.set(id, history.length);
  history.push({
    role: 'human',
    message,
    shortCommit: '',
    timestampUnix: Math.floor(Date.now() / 1000)
  });
  state.histories.set(id, history);
  beginActivityGroup(id);
  clearComposer();
  state.running.add(id);
  elements.sendButton.disabled = true;
  setStatus('');
  renderSidebar();
  renderTranscript({ scrollToBottom: true });

  const onEvent = new tauri.Channel();
  onEvent.onmessage = (event) => handleTurnEvent(id, event);
  try {
    const outcome = await tauri.invoke('send_message', {
      conversation: id,
      message,
      title: conversation.title,
      onEvent
    });
    finishActivityGroup(id);
    const optimistic = state.histories.get(id) || [];
    const turnStart = state.turnStartIndexes.get(id) ?? optimistic.length;
    state.transientTurnEntries.set(id, transientTurnEntries(optimistic, turnStart));
    state.histories.delete(id);
    state.diffs.delete(id);
    await refreshConversations(id);
    await loadHistory(id, true);
    loadDiff(id, true);
    if (state.selectedId === id) {
      renderHeader();
      renderTranscript({ scrollToBottom: true });
      setStatus('');
    }
  } catch (error) {
    finishActivityGroup(id);
    const optimistic = state.histories.get(id) || [];
    const turnStart = state.turnStartIndexes.get(id) ?? optimistic.length - 1;
    const human = optimistic[turnStart];
    if (human?.role === 'human' && human.message === message) human.failed = true;
    optimistic.push({
      role: 'agent',
      message: String(error),
      failed: true,
      timestampUnix: Math.floor(Date.now() / 1000)
    });
    if (state.selectedId === id) {
      renderTranscript({ scrollToBottom: true });
      setStatus('');
    }
  } finally {
    state.pendingActivityGroups.delete(id);
    state.turnStartIndexes.delete(id);
    state.running.delete(id);
    if (state.selectedId === id) elements.sendButton.disabled = false;
    renderSidebar();
  }
}

async function createConversation() {
  const existingDraft = state.conversations.find((item) => item.draft && !item.started);
  if (existingDraft) {
    await selectConversation(existingDraft.id);
    return;
  }
  if (state.creatingConversation) return;
  state.creatingConversation = true;
  elements.newTask.disabled = true;
  try {
    saveSelectedDraft();
    const conversation = await tauri.invoke('new_conversation');
    state.conversations.unshift(conversation);
    state.histories.set(conversation.id, []);
    await selectConversation(conversation.id);
    elements.prompt.focus();
  } catch (error) {
    setStatus(String(error));
  } finally {
    state.creatingConversation = false;
    elements.newTask.disabled = false;
  }
}

async function initialize() {
  if (!tauri) {
    showFatal('The Tauri bridge is unavailable. Run this interface through the CAOS desktop binary.');
    return;
  }
  try {
    const payload = await tauri.invoke('bootstrap');
    state.repo = payload;
    state.conversations = payload.conversations;
    clearStartupLoading();
    if (state.conversations.length === 0) {
      await createConversation();
    } else {
      await selectConversation(state.conversations[0].id);
    }
  } catch (error) {
    showFatal(error);
  }
}

elements.newTask.addEventListener('click', createConversation);
elements.composer.addEventListener('submit', (event) => {
  event.preventDefault();
  sendCurrentMessage();
});
elements.prompt.addEventListener('input', () => {
  state.drafts.set(state.selectedId, elements.prompt.value);
  resizePrompt();
});

function commitPromptEdit() {
  elements.prompt.dispatchEvent(new Event('input'));
}

function deletePreviousWord() {
  const start = elements.prompt.selectionStart;
  const end = elements.prompt.selectionEnd;
  if (start !== end) {
    elements.prompt.setRangeText('', start, end, 'end');
    commitPromptEdit();
    return;
  }
  const before = elements.prompt.value.slice(0, start);
  const whitespaceLength = before.match(/\s+$/u)?.[0].length || 0;
  const beforeWord = before.slice(0, before.length - whitespaceLength);
  const wordLength = beforeWord.match(/\S+$/u)?.[0].length || 0;
  elements.prompt.setRangeText('', start - whitespaceLength - wordLength, end, 'end');
  commitPromptEdit();
}

function deleteToEndOfLine() {
  const start = elements.prompt.selectionStart;
  const selectionEnd = elements.prompt.selectionEnd;
  if (start !== selectionEnd) {
    elements.prompt.setRangeText('', start, selectionEnd, 'end');
    commitPromptEdit();
    return;
  }
  const newline = elements.prompt.value.indexOf('\n', start);
  const end = newline === start
    ? start + 1
    : newline === -1 ? elements.prompt.value.length : newline;
  elements.prompt.setRangeText('', start, end, 'end');
  commitPromptEdit();
}

elements.prompt.addEventListener('keydown', (event) => {
  const key = event.key.toLowerCase();
  if (event.ctrlKey && !event.shiftKey && event.key === 'Enter' && !event.isComposing) {
    event.preventDefault();
    sendCurrentMessage();
    return;
  }
  if (event.ctrlKey && key === 'j' && !event.isComposing) {
    event.preventDefault();
    elements.prompt.setRangeText('\n', elements.prompt.selectionStart, elements.prompt.selectionEnd, 'end');
    elements.prompt.dispatchEvent(new Event('input'));
    return;
  }
  if (event.ctrlKey && key === 'a') {
    event.preventDefault();
    const start = elements.prompt.value.lastIndexOf('\n', elements.prompt.selectionStart - 1) + 1;
    elements.prompt.setSelectionRange(start, start);
    return;
  }
  if (event.ctrlKey && key === 'e') {
    event.preventDefault();
    const newline = elements.prompt.value.indexOf('\n', elements.prompt.selectionEnd);
    const end = newline === -1 ? elements.prompt.value.length : newline;
    elements.prompt.setSelectionRange(end, end);
    return;
  }
  if (event.ctrlKey && key === 'w') {
    event.preventDefault();
    deletePreviousWord();
    return;
  }
  if (event.ctrlKey && key === 'k') {
    event.preventDefault();
    deleteToEndOfLine();
    return;
  }
  if (event.ctrlKey && key === 'c' && elements.prompt.value) {
    event.preventDefault();
    elements.prompt.value = '';
    elements.prompt.dispatchEvent(new Event('input'));
  }
});

document.addEventListener('keydown', (event) => {
  if (event.ctrlKey) document.body.classList.add('is-control-held');
  const key = event.key.toLowerCase();
  if (event.metaKey && ['-', '_', '=', '+', '0'].includes(key)) {
    event.preventDefault();
    if (key === '0') {
      applyUiZoom(1, true);
    } else {
      applyUiZoom(state.uiZoom + (key === '-' || key === '_' ? -UI_ZOOM_STEP : UI_ZOOM_STEP), true);
    }
    return;
  }
  if (event.ctrlKey && event.shiftKey && key === 'p') {
    event.preventDefault();
    setCommandPalette(!state.commandPaletteOpen);
    return;
  }
  if (state.commandPaletteOpen) {
    if (event.key === 'Escape') {
      event.preventDefault();
      setCommandPalette(false);
    }
    return;
  }
  if (event.ctrlKey && key === 'h') {
    event.preventDefault();
    setShortcutHelp(!state.shortcutHelpOpen);
    return;
  }
  if (state.shortcutHelpOpen) {
    if (event.key === 'Escape') {
      event.preventDefault();
      setShortcutHelp(false);
    }
    return;
  }
  if (event.ctrlKey && !event.shiftKey && /^[1-9]$/u.test(event.key)) {
    event.preventDefault();
    const conversation = state.conversations[Number(event.key) - 1];
    if (conversation) selectConversation(conversation.id);
  } else if (event.ctrlKey && key === 'n') {
    event.preventDefault();
    createConversation();
  } else if (event.ctrlKey && event.key === 'ArrowUp') {
    event.preventDefault();
    selectRelativeConversation(-1);
  } else if (event.ctrlKey && event.key === 'ArrowDown') {
    event.preventDefault();
    selectRelativeConversation(1);
  } else if (event.ctrlKey && key === 'q') {
    event.preventDefault();
    togglePane('changes');
  } else if (event.ctrlKey && key === 'r') {
    event.preventDefault();
    reloadSelectedConversation();
  } else if (event.key === 'Escape' && state.panes.changes) {
    event.preventDefault();
    closeInspectorPanes();
  } else if (event.key === 'PageUp' || event.key === 'PageDown') {
    event.preventDefault();
    const amount = Math.max(160, Math.round(elements.transcriptScroll.clientHeight * .65));
    elements.transcriptScroll.scrollBy({
      top: event.key === 'PageUp' ? -amount : amount,
      behavior: 'smooth'
    });
  }
});

document.addEventListener('keyup', (event) => {
  if (event.key === 'Control' || !event.ctrlKey) document.body.classList.remove('is-control-held');
});

window.addEventListener('blur', () => document.body.classList.remove('is-control-held'));

elements.shortcutHelp.addEventListener('click', (event) => {
  if (event.target.closest('[data-close-shortcuts]')) setShortcutHelp(false);
});

elements.commandPaletteButton.addEventListener('click', () => {
  setCommandPalette(!state.commandPaletteOpen);
});

elements.commandPalette.addEventListener('click', (event) => {
  if (event.target.closest('[data-close-command-palette]')) setCommandPalette(false);
});

elements.commandPaletteQuery.addEventListener('input', () => {
  state.commandPaletteSelection = 0;
  renderCommandPalette();
});

elements.commandPaletteQuery.addEventListener('keydown', (event) => {
  const commands = matchingPaletteCommands();
  if (['ArrowUp', 'ArrowDown', 'Enter', 'Escape'].includes(event.key)) event.stopPropagation();
  if (event.key === 'ArrowUp') {
    event.preventDefault();
    selectPaletteIndex(state.commandPaletteSelection - 1);
  } else if (event.key === 'ArrowDown') {
    event.preventDefault();
    selectPaletteIndex(state.commandPaletteSelection + 1);
  } else if (event.key === 'Enter' && commands.length > 0) {
    event.preventDefault();
    executePaletteCommand();
  } else if (event.key === 'Escape') {
    event.preventDefault();
    setCommandPalette(false);
  }
});

for (const button of document.querySelectorAll('[data-pane]')) {
  button.addEventListener('click', () => togglePane(button.dataset.pane));
}

for (const button of document.querySelectorAll('[data-close-pane]')) {
  button.addEventListener('click', () => setPaneOpen(button.dataset.closePane, false));
}

elements.sidebarResizer.addEventListener('pointerdown', (event) => {
  if (event.button !== 0) return;
  event.preventDefault();
  state.resizingSidebar = true;
  document.body.classList.add('is-resizing-sidebar');
  elements.sidebarResizer.setPointerCapture(event.pointerId);
  setSidebarWidth(event.clientX);
});

window.addEventListener('pointermove', (event) => {
  if (!state.resizingSidebar) return;
  setSidebarWidth(event.clientX);
});

function finishSidebarResize(event) {
  if (!state.resizingSidebar) return;
  state.resizingSidebar = false;
  document.body.classList.remove('is-resizing-sidebar');
  if (elements.sidebarResizer.hasPointerCapture(event.pointerId)) {
    elements.sidebarResizer.releasePointerCapture(event.pointerId);
  }
  setSidebarWidth(state.sidebarWidth, true);
}

window.addEventListener('pointerup', finishSidebarResize);
window.addEventListener('pointercancel', finishSidebarResize);
elements.sidebarResizer.addEventListener('dblclick', () => setSidebarWidth(DEFAULT_SIDEBAR_WIDTH, true));
elements.sidebarResizer.addEventListener('keydown', (event) => {
  if (!['ArrowLeft', 'ArrowRight'].includes(event.key)) return;
  event.preventDefault();
  const direction = event.key === 'ArrowLeft' ? -1 : 1;
  setSidebarWidth(state.sidebarWidth + direction * (event.shiftKey ? 32 : 12), true);
});

elements.inspectorResizer.addEventListener('pointerdown', (event) => {
  if (event.button !== 0) return;
  event.preventDefault();
  state.resizingInspector = true;
  document.body.classList.add('is-resizing-inspector');
  elements.inspectorResizer.setPointerCapture(event.pointerId);
  setInspectorWidth(window.innerWidth - event.clientX);
});

window.addEventListener('pointermove', (event) => {
  if (!state.resizingInspector) return;
  setInspectorWidth(window.innerWidth - event.clientX);
});

function finishInspectorResize(event) {
  if (!state.resizingInspector) return;
  state.resizingInspector = false;
  document.body.classList.remove('is-resizing-inspector');
  if (elements.inspectorResizer.hasPointerCapture(event.pointerId)) {
    elements.inspectorResizer.releasePointerCapture(event.pointerId);
  }
  setInspectorWidth(state.inspectorWidth, true);
}

window.addEventListener('pointerup', finishInspectorResize);
window.addEventListener('pointercancel', finishInspectorResize);
elements.inspectorResizer.addEventListener('dblclick', () => setInspectorWidth(DEFAULT_INSPECTOR_WIDTH, true));
elements.inspectorResizer.addEventListener('keydown', (event) => {
  if (!['ArrowLeft', 'ArrowRight'].includes(event.key)) return;
  event.preventDefault();
  const direction = event.key === 'ArrowLeft' ? 1 : -1;
  setInspectorWidth(state.inspectorWidth + direction * (event.shiftKey ? 32 : 12), true);
});

window.addEventListener('resize', () => {
  setInspectorWidth(state.inspectorWidth);
  setSidebarWidth(state.sidebarWidth);
});

restoreUiZoom();
restoreSidebarWidth();
restoreInspectorWidth();
initialize();
