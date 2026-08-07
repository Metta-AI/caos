import {
  activityGroupComplete,
  activityGroupExpandable,
  activityGroupSummary,
  mergeReplayedHistory,
  scrollPositionIsNearBottom,
  toolDescription
} from './activity.js';
import { copyText } from './clipboard.js';
import {
  modelChoices,
  modelLabel,
  parseComposerCommand,
  slashCommandMatches
} from './commands.js';
import {
  filePatchesFromPatch,
  highlightedHunkLines,
  lineCounts,
  unchangedLinesBefore
} from './changes.js';
import { appendTokens, initializeHighlighting } from './highlight.js';
import { renderMarkdown } from './markdown.js';

const tauri = window.__TAURI__?.core;
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
  { id: 'checkout', label: 'Check out conversation', shortcut: 'Ctrl+L', keywords: 'load workspace git', run: () => checkoutSelectedConversation() },
  { id: 'publish', label: 'Publish pull request', shortcut: 'Ctrl+P twice', keywords: 'push pr github branch', run: () => openPublishDialog() },
  { id: 'changes', label: 'Toggle workspace changes', shortcut: 'Ctrl+Q', keywords: 'diff files pane', available: () => !elements.changesToggle.hidden, run: () => toggleChangesPane() },
  { id: 'tools', label: 'Show available tools', shortcut: 'Ctrl+Shift+T', keywords: 'commands agent project', run: () => toggleToolsPane() },
  { id: 'reload', label: 'Reload conversation', shortcut: 'Ctrl+R', keywords: 'refresh history', run: () => reloadSelectedConversation() },
  { id: 'rename', label: 'Rename conversation', shortcut: '/rename', keywords: 'title name', run: () => prefillCommand('/rename ') },
  { id: 'archive', label: 'Archive conversation', shortcut: 'Ctrl+E', keywords: 'close remove', run: () => archiveSelectedConversation() },
  { id: 'restore', label: 'Restore archived conversation', shortcut: '', keywords: 'unarchive reopen', run: () => openArchiveDialog() },
  { id: 'help', label: 'Show keyboard shortcuts', shortcut: 'Ctrl+H', keywords: 'help commands', run: () => setShortcutHelp(true) }
];

const state = {
  repo: null,
  conversations: [],
  selectedId: null,
  histories: new Map(),
  diffs: new Map(),
  selectedDiffFiles: new Map(),
  diffFileQueries: new Map(),
  pendingActivityGroups: new Map(),
  turnStartIndexes: new Map(),
  running: new Set(),
  composerDrafts: new Map(),
  conversationModels: new Map(),
  changesOpen: false,
  selectedAction: null,
  shortcutHelpOpen: false,
  commandPaletteOpen: false,
  commandPaletteSelection: 0,
  modelChoices: modelChoices(null),
  initialModel: '',
  modelMenuOpen: false,
  slashCommandSelection: 0,
  slashCommandDismissed: false,
  publishDialogOpen: false,
  archiveDialogOpen: false,
  uiZoom: 1,
  sidebarWidth: DEFAULT_SIDEBAR_WIDTH,
  inspectorWidth: DEFAULT_INSPECTOR_WIDTH,
  creatingConversation: false
};

const elements = {
  taskList: document.getElementById('task-list'),
  taskWorkspace: document.getElementById('task-workspace'),
  sidebarResizer: document.getElementById('sidebar-resizer'),
  inspector: document.getElementById('inspector'),
  inspectorResizer: document.getElementById('inspector-resizer'),
  changesPane: document.getElementById('changes-pane'),
  changesSummary: document.getElementById('changes-summary'),
  actionPane: document.getElementById('action-pane'),
  actionPaneTitle: document.getElementById('action-pane-title'),
  actionResult: document.getElementById('action-result'),
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
  slashCommandMenu: document.getElementById('slash-command-menu'),
  modelButton: document.getElementById('model-button'),
  modelLabel: document.getElementById('model-label'),
  modelMenu: document.getElementById('model-menu'),
  sendButton: document.getElementById('send-button'),
  turnStatus: document.getElementById('turn-status'),
  fileList: document.getElementById('file-list'),
  fileFilter: document.getElementById('file-filter'),
  diffFileHeader: document.getElementById('diff-file-header'),
  diff: document.getElementById('diff'),
  shortcutHelp: document.getElementById('shortcut-help'),
  commandPalette: document.getElementById('command-palette'),
  commandPaletteQuery: document.getElementById('command-palette-query'),
  commandPaletteResults: document.getElementById('command-palette-results'),
  publishDialog: document.getElementById('publish-dialog'),
  publishBase: document.getElementById('publish-base'),
  publishConfirm: document.getElementById('publish-confirm'),
  archiveDialog: document.getElementById('archive-dialog'),
  archiveList: document.getElementById('archive-list'),
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
  const inspectorWidth = state.changesOpen || state.selectedAction ? state.inspectorWidth : 0;
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
  if (open) setModelMenu(false);
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
  if (open) setModelMenu(false);
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
  if (state.selectedId) state.composerDrafts.set(state.selectedId, elements.prompt.value);
}

function restoreSelectedDraft() {
  elements.prompt.value = state.composerDrafts.get(state.selectedId) || '';
  resizePrompt();
  state.slashCommandDismissed = false;
  renderSlashCommandMenu();
}

function clearComposer() {
  state.composerDrafts.set(state.selectedId, '');
  elements.prompt.value = '';
  resizePrompt();
  state.slashCommandDismissed = false;
  renderSlashCommandMenu();
}

function prefillCommand(command) {
  state.composerDrafts.set(state.selectedId, command);
  elements.prompt.value = command;
  resizePrompt();
  state.slashCommandDismissed = false;
  renderSlashCommandMenu();
  elements.prompt.focus({ preventScroll: true });
  elements.prompt.setSelectionRange(command.length, command.length);
}

function selectedModel() {
  if (!state.selectedId) return state.initialModel;
  return state.conversationModels.has(state.selectedId)
    ? state.conversationModels.get(state.selectedId)
    : state.initialModel;
}

function renderModelControl() {
  const selected = selectedModel();
  elements.modelLabel.textContent = modelLabel(selected, state.modelChoices);
  elements.modelButton.title = selected || 'Use the CAOS default model';
  elements.modelMenu.replaceChildren();
  for (const model of state.modelChoices) {
    const button = document.createElement('button');
    button.type = 'button';
    button.className = 'model-option';
    button.setAttribute('role', 'option');
    button.setAttribute('aria-selected', String(model.value === selected));
    const label = document.createElement('span');
    label.textContent = model.label;
    const detail = document.createElement('small');
    detail.textContent = model.detail;
    button.append(label, detail);
    button.addEventListener('click', () => {
      if (state.selectedId) state.conversationModels.set(state.selectedId, model.value);
      setModelMenu(false);
      renderModelControl();
    });
    elements.modelMenu.append(button);
  }
}

function setModelMenu(open) {
  state.modelMenuOpen = open;
  elements.modelMenu.hidden = !open;
  elements.modelButton.setAttribute('aria-expanded', String(open));
  if (open) renderModelControl();
}

function renderSlashCommandMenu() {
  const matches = state.slashCommandDismissed ? [] : slashCommandMatches(elements.prompt.value);
  elements.slashCommandMenu.replaceChildren();
  elements.slashCommandMenu.hidden = matches.length === 0;
  if (matches.length === 0) {
    state.slashCommandSelection = 0;
    return;
  }
  state.slashCommandSelection = Math.min(state.slashCommandSelection, matches.length - 1);
  matches.forEach((command, index) => {
    const button = document.createElement('button');
    button.type = 'button';
    button.className = 'slash-command-item';
    button.classList.toggle('is-selected', index === state.slashCommandSelection);
    button.setAttribute('role', 'option');
    button.setAttribute('aria-selected', String(index === state.slashCommandSelection));
    const usage = document.createElement('code');
    usage.textContent = command.usage;
    const description = document.createElement('span');
    description.textContent = command.description;
    button.append(usage, description);
    button.addEventListener('mousedown', (event) => event.preventDefault());
    button.addEventListener('click', () => completeSlashCommand(index));
    elements.slashCommandMenu.append(button);
  });
}

function completeSlashCommand(index = state.slashCommandSelection) {
  const command = slashCommandMatches(elements.prompt.value)[index];
  if (!command) return false;
  const takesArgument = command.usage.includes('<');
  prefillCommand(`${command.name}${takesArgument ? ' ' : ''}`);
  elements.slashCommandMenu.hidden = true;
  return true;
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
    button.dataset.conversationId = conversation.id;
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
    button.addEventListener('click', async () => {
      await selectConversation(conversation.id, false);
      const selectedButton = [...elements.taskList.querySelectorAll('.task-item')]
        .find((item) => item.dataset.conversationId === conversation.id);
      selectedButton?.focus({ preventScroll: true });
    });
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

async function copyMessage(message, button) {
  try {
    await copyText(message);
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

function actionCallIsSelected(call) {
  if (state.selectedAction?.conversationId !== state.selectedId) return false;
  return state.selectedAction.call === call
    || Boolean(call.toolUseId && state.selectedAction.call?.toolUseId === call.toolUseId);
}

function activityGroupElement(entry) {
  const section = document.createElement('section');
  section.className = 'inline-activity';
  if (entry.running) section.classList.add('is-running');

  const hasCalls = entry.calls.length > 0;
  const expandable = activityGroupExpandable(entry);
  const directCall = !expandable && entry.calls.length === 1 ? entry.calls[0] : null;
  const toggle = document.createElement(expandable || directCall ? 'button' : 'div');
  toggle.className = 'inline-activity-toggle';
  let chevron = null;
  if (expandable) {
    toggle.type = 'button';
    toggle.setAttribute('aria-expanded', String(entry.expanded));
    chevron = iconElement([['path', { d: 'm9 18 6-6-6-6' }]]);
    chevron.classList.add('inline-activity-chevron');
  } else if (directCall) {
    toggle.type = 'button';
    toggle.setAttribute('aria-controls', 'action-pane');
    if (actionCallIsSelected(directCall)) toggle.classList.add('is-result-selected');
  } else {
    toggle.setAttribute('role', 'status');
  }
  const label = document.createElement('span');
  label.className = 'inline-activity-label';
  label.textContent = hasCalls
    ? activityGroupSummary(entry.calls)
    : entry.status || 'Working';
  toggle.append(label);
  if (entry.running) {
    const spinner = document.createElement('span');
    spinner.className = 'loading-spinner inline-activity-spinner';
    spinner.setAttribute('aria-hidden', 'true');
    toggle.append(spinner);
  }
  if (chevron) toggle.append(chevron);

  const list = document.createElement('div');
  list.className = 'inline-activity-list';
  list.setAttribute('role', 'list');
  list.hidden = !expandable || !entry.expanded;
  for (const call of entry.calls) {
    const item = document.createElement('div');
    item.setAttribute('role', 'listitem');
    const row = document.createElement('button');
    row.type = 'button';
    row.className = 'inline-activity-item';
    row.setAttribute('aria-controls', 'action-pane');
    if (actionCallIsSelected(call)) row.classList.add('is-selected');
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
    row.addEventListener('click', () => openActionResult(call, row));
    item.append(row);
    list.append(item);
  }

  if (expandable) {
    toggle.addEventListener('click', () => {
      const keepBottomAnchored = !entry.expanded && transcriptIsNearBottom();
      entry.expanded = !entry.expanded;
      toggle.setAttribute('aria-expanded', String(entry.expanded));
      list.hidden = !entry.expanded;
      if (keepBottomAnchored) scrollTranscriptToBottom();
    });
  } else if (directCall) {
    toggle.addEventListener('click', () => openActionResult(directCall, toggle));
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
  const changesOpen = state.changesOpen;
  const actionOpen = Boolean(state.selectedAction);
  const inspectorOpen = changesOpen || actionOpen;
  elements.inspector.hidden = !inspectorOpen;
  elements.taskWorkspace.classList.toggle('is-changes-view', changesOpen);
  elements.changesPane.hidden = !changesOpen;
  elements.actionPane.hidden = !actionOpen;
  elements.inspector.classList.toggle('has-stacked-panes', changesOpen && actionOpen);
  elements.changesToggle.classList.toggle('is-open', changesOpen);
  elements.changesToggle.setAttribute('aria-expanded', String(changesOpen));
  if (!inspectorOpen) {
    setSidebarWidth(state.sidebarWidth);
    return;
  }
  setInspectorWidth(state.inspectorWidth);
  setSidebarWidth(state.sidebarWidth);
  if (changesOpen) loadDiff(state.selectedId);
  if (actionOpen) renderActionResult();
}

function renderActionResult() {
  const selection = state.selectedAction;
  if (!selection) return;
  if (selection.kind === 'tools') {
    elements.actionPaneTitle.textContent = 'Available tools';
    elements.actionPaneTitle.removeAttribute('title');
    elements.actionResult.replaceChildren();
    if (selection.loading) {
      const loading = document.createElement('div');
      loading.className = 'panel-empty';
      loading.textContent = 'Loading project tools…';
      elements.actionResult.append(loading);
      return;
    }
    if (selection.error) {
      const error = document.createElement('div');
      error.className = 'panel-empty is-error';
      error.textContent = selection.error;
      elements.actionResult.append(error);
      return;
    }
    const builtinsHeading = document.createElement('h3');
    builtinsHeading.className = 'tool-set-heading';
    builtinsHeading.textContent = 'Always available';
    const builtins = document.createElement('div');
    builtins.className = 'tool-set-list';
    for (const [names, docs] of [
      ['read, ls, write, edit', 'Inline workspace operations'],
      ['bash', 'Commands in the workspace sandbox'],
      ['grep', 'Cached regular-expression search']
    ]) {
      const item = document.createElement('article');
      item.className = 'tool-set-item';
      const name = document.createElement('code');
      name.textContent = names;
      const description = document.createElement('p');
      description.textContent = docs;
      item.append(name, description);
      builtins.append(item);
    }
    const projectHeading = document.createElement('h3');
    projectHeading.className = 'tool-set-heading';
    projectHeading.textContent = 'Project tools';
    const source = document.createElement('div');
    source.className = 'tool-set-source';
    source.textContent = `Source: ${selection.tools.source}`;
    elements.actionResult.append(builtinsHeading, builtins, projectHeading, source);
    if (selection.tools.tools.length === 0) {
      const empty = document.createElement('div');
      empty.className = 'panel-empty';
      empty.textContent = 'This conversation has no project-defined tools.';
      elements.actionResult.append(empty);
      return;
    }
    const list = document.createElement('div');
    list.className = 'tool-set-list';
    for (const tool of selection.tools.tools) {
      const item = document.createElement('article');
      item.className = 'tool-set-item';
      const heading = document.createElement('div');
      heading.className = 'tool-set-item-heading';
      const name = document.createElement('code');
      name.textContent = tool.name;
      const image = document.createElement('small');
      image.textContent = /^[0-9a-f]{40,}$/iu.test(tool.image)
        ? tool.image.slice(0, 7)
        : tool.image;
      const docs = document.createElement('p');
      docs.textContent = tool.docs || 'No description.';
      heading.append(name, image);
      item.append(heading, docs);
      list.append(item);
    }
    elements.actionResult.append(list);
    return;
  }
  const { call } = selection;
  const title = toolDescription(call);
  elements.actionPaneTitle.textContent = title;
  elements.actionPaneTitle.title = title;
  elements.actionResult.replaceChildren();
  if (!call.result) {
    const pending = document.createElement('div');
    pending.className = 'panel-empty';
    pending.textContent = 'Waiting for this action to finish…';
    elements.actionResult.append(pending);
    return;
  }
  const output = document.createElement('pre');
  output.className = 'action-result-output';
  if (call.result.isError) output.classList.add('is-error');
  const code = document.createElement('code');
  code.textContent = String(call.result.content || '').trimEnd() || 'No output.';
  output.append(code);
  elements.actionResult.append(output);
}

function clearActionHighlights() {
  for (const selected of elements.transcript.querySelectorAll(
    '.inline-activity-item.is-selected, .inline-activity-toggle.is-result-selected'
  )) {
    selected.classList.remove('is-selected', 'is-result-selected');
  }
}

function openActionResult(call, source) {
  state.selectedAction = { conversationId: state.selectedId, call };
  clearActionHighlights();
  source.classList.add(source.classList.contains('inline-activity-item')
    ? 'is-selected'
    : 'is-result-selected');
  updateInspectorLayout();
}

function closeInspectorPane(pane) {
  if (pane === 'action') {
    state.selectedAction = null;
    clearActionHighlights();
  } else if (pane === 'changes') {
    state.changesOpen = false;
  }
  updateInspectorLayout();
}

function toggleChangesPane() {
  if (elements.changesToggle.hidden) return;
  const opening = !state.changesOpen;
  state.changesOpen = opening;
  if (opening) {
    state.selectedAction = null;
    clearActionHighlights();
  }
  updateInspectorLayout();
}

function resetInspector() {
  state.changesOpen = false;
  state.selectedAction = null;
  clearActionHighlights();
  updateInspectorLayout();
}

function closeInspectorPanes() {
  resetInspector();
  elements.prompt.focus({ preventScroll: true });
}

async function selectConversation(id, focusPrompt = true) {
  if (id === state.selectedId) {
    if (focusPrompt) elements.prompt.focus({ preventScroll: true });
    return;
  }
  saveSelectedDraft();
  state.selectedId = id;
  setModelMenu(false);
  renderSidebar();
  renderHeader();
  renderModelControl();
  restoreSelectedDraft();
  setStatus('');
  resetInspector();
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
  if (focusPrompt) elements.prompt.focus({ preventScroll: true });
}

async function loadHistory(id, force = false) {
  if (!id || (!force && state.histories.has(id))) return;
  try {
    const history = await tauri.invoke('get_history', { conversation: id });
    state.histories.set(id, mergeReplayedHistory(history.turns, history.turnEvents));
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

async function checkoutSelectedConversation() {
  const conversation = selectedConversation();
  if (!conversation) return;
  if (state.running.has(conversation.id)) {
    setStatus("Finish this conversation's operation before checking it out");
    return;
  }
  if (conversation.draft) {
    setStatus('This conversation has no commit to check out');
    return;
  }
  setStatus('Checking out conversation…');
  try {
    const head = await tauri.invoke('checkout_conversation', { conversation: conversation.id });
    setStatus(`Checked out ${head} in detached HEAD`);
  } catch (error) {
    setStatus(String(error));
  } finally {
    elements.prompt.focus({ preventScroll: true });
  }
}

function setPublishDialog(open) {
  state.publishDialogOpen = open;
  elements.publishDialog.hidden = !open;
  if (!open) {
    elements.publishConfirm.disabled = false;
    elements.prompt.focus({ preventScroll: true });
  }
}

async function openPublishDialog() {
  const conversation = selectedConversation();
  if (!conversation) return;
  if (state.running.has(conversation.id)) {
    setStatus("Finish this conversation's operation before publishing it");
    return;
  }
  if (conversation.draft) {
    setStatus('There are no conversation changes to publish');
    return;
  }
  setCommandPalette(false);
  setPublishDialog(true);
  elements.publishBase.value = '';
  elements.publishBase.placeholder = 'Loading default branch…';
  elements.publishConfirm.disabled = true;
  try {
    const branch = await tauri.invoke('default_publish_branch');
    if (!state.publishDialogOpen) return;
    elements.publishBase.value = branch;
    elements.publishBase.placeholder = branch;
    elements.publishConfirm.disabled = false;
    elements.publishBase.focus();
    elements.publishBase.select();
  } catch (error) {
    setPublishDialog(false);
    setStatus(String(error));
  }
}

async function confirmPublish() {
  const conversation = selectedConversation();
  if (!conversation || elements.publishConfirm.disabled) return;
  elements.publishConfirm.disabled = true;
  const base = elements.publishBase.value.trim();
  setStatus('Publishing a clean conversation branch…');
  try {
    const url = await tauri.invoke('publish_conversation', {
      conversation: conversation.id,
      base: base || null
    });
    setPublishDialog(false);
    setStatus(`Published ${url}`);
  } catch (error) {
    setStatus(String(error));
    elements.publishConfirm.disabled = false;
    elements.publishBase.focus();
  }
}

async function archiveSelectedConversation() {
  const conversation = selectedConversation();
  if (!conversation) return;
  if (state.running.has(conversation.id)) {
    setStatus("Finish this conversation's operation before archiving it");
    return;
  }
  setCommandPalette(false);
  setStatus('Archiving conversation…');
  try {
    await tauri.invoke('archive_conversation', { conversation: conversation.id });
    const index = state.conversations.indexOf(conversation);
    state.conversations.splice(index, 1);
    state.histories.delete(conversation.id);
    state.diffs.delete(conversation.id);
    state.composerDrafts.delete(conversation.id);
    state.conversationModels.delete(conversation.id);
    state.selectedId = null;
    if (state.conversations.length === 0) {
      await createConversation();
    } else {
      await selectConversation(state.conversations[Math.min(index, state.conversations.length - 1)].id);
    }
    renderSidebar();
    setStatus('Conversation archived');
  } catch (error) {
    setStatus(String(error));
  }
}

function setArchiveDialog(open) {
  state.archiveDialogOpen = open;
  elements.archiveDialog.hidden = !open;
  if (!open) elements.prompt.focus({ preventScroll: true });
}

async function openArchiveDialog() {
  setCommandPalette(false);
  setArchiveDialog(true);
  elements.archiveList.replaceChildren();
  const loading = document.createElement('div');
  loading.className = 'panel-empty';
  loading.textContent = 'Loading archived conversations…';
  elements.archiveList.append(loading);
  try {
    const conversations = await tauri.invoke('get_archived_conversations');
    if (!state.archiveDialogOpen) return;
    elements.archiveList.replaceChildren();
    if (conversations.length === 0) {
      const empty = document.createElement('div');
      empty.className = 'panel-empty';
      empty.textContent = 'No archived conversations.';
      elements.archiveList.append(empty);
      return;
    }
    for (const conversation of conversations) {
      const button = document.createElement('button');
      button.type = 'button';
      button.className = 'archive-item';
      const title = document.createElement('span');
      title.textContent = conversation.title;
      const meta = document.createElement('code');
      meta.textContent = conversation.shortHead;
      button.append(title, meta);
      button.addEventListener('click', () => restoreArchivedConversation(conversation.id, button));
      elements.archiveList.append(button);
    }
    elements.archiveList.querySelector('button')?.focus();
  } catch (error) {
    elements.archiveList.textContent = String(error);
  }
}

async function restoreArchivedConversation(id, button) {
  button.disabled = true;
  try {
    const conversation = await tauri.invoke('restore_conversation', { conversation: id });
    state.conversations.unshift(conversation);
    setArchiveDialog(false);
    state.selectedId = null;
    await selectConversation(conversation.id);
    renderSidebar();
    setStatus('Conversation restored');
  } catch (error) {
    setStatus(String(error));
    button.disabled = false;
  }
}

async function toggleToolsPane() {
  const conversation = selectedConversation();
  if (!conversation) return;
  if (state.selectedAction?.kind === 'tools'
    && state.selectedAction.conversationId === conversation.id) {
    closeInspectorPane('action');
    return;
  }
  state.changesOpen = false;
  state.selectedAction = { kind: 'tools', conversationId: conversation.id, loading: true };
  updateInspectorLayout();
  try {
    const tools = await tauri.invoke('get_tools', { conversation: conversation.id });
    if (state.selectedAction?.kind !== 'tools'
      || state.selectedAction.conversationId !== conversation.id) return;
    state.selectedAction = { kind: 'tools', conversationId: conversation.id, tools };
  } catch (error) {
    if (state.selectedAction?.kind !== 'tools'
      || state.selectedAction.conversationId !== conversation.id) return;
    state.selectedAction = { kind: 'tools', conversationId: conversation.id, error: String(error) };
  }
  renderActionResult();
}

function selectRelativeConversation(amount) {
  if (state.conversations.length < 2) return;
  const selected = state.conversations.findIndex((item) => item.id === state.selectedId);
  const next = (selected + amount + state.conversations.length) % state.conversations.length;
  selectConversation(state.conversations[next].id);
}

function changeStatsElement(stats, className = '') {
  const container = document.createElement('span');
  container.className = `change-stats ${className}`.trim();
  const additions = document.createElement('span');
  additions.className = 'change-stat is-add';
  additions.textContent = `+${stats.additions}`;
  const deletions = document.createElement('span');
  deletions.className = 'change-stat is-delete';
  deletions.textContent = `-${stats.deletions}`;
  container.setAttribute(
    'aria-label',
    `${stats.additions} lines added, ${stats.deletions} lines deleted`
  );
  container.append(additions, deletions);
  return container;
}

function renderChangeCount(stats) {
  for (const container of [elements.changeCount, elements.changesSummary]) {
    container.replaceChildren();
    container.removeAttribute('aria-label');
    if (!stats) continue;
    const rendered = changeStatsElement(stats);
    container.setAttribute('aria-label', rendered.getAttribute('aria-label'));
    container.append(...rendered.childNodes);
  }
}

function fileBadgeElement(file) {
  const badge = document.createElement('span');
  badge.className = 'file-badge';
  badge.dataset.extension = file.presentation.extension || 'file';
  badge.textContent = file.presentation.badge;
  return badge;
}

function renderDiffFileHeader(file) {
  elements.diffFileHeader.replaceChildren();
  elements.diffFileHeader.hidden = !file;
  if (!file) return;
  const identity = document.createElement('div');
  identity.className = 'diff-file-identity';
  const path = document.createElement('span');
  path.className = 'diff-file-path';
  path.textContent = file.path;
  identity.append(fileBadgeElement(file), path);
  const details = document.createElement('div');
  details.className = 'diff-file-details';
  const status = document.createElement('span');
  status.className = `file-status-label is-${file.status}`;
  status.textContent = file.status;
  details.append(status, changeStatsElement(file.stats, 'is-compact'));
  elements.diffFileHeader.append(identity, details);
}

function diffLineElement(line) {
  const row = document.createElement('div');
  row.className = `diff-row is-${line.kind}`;
  const oldNumber = document.createElement('span');
  oldNumber.className = 'diff-line-number';
  oldNumber.textContent = line.oldLine ?? '';
  const newNumber = document.createElement('span');
  newNumber.className = 'diff-line-number';
  newNumber.textContent = line.newLine ?? '';
  const marker = document.createElement('span');
  marker.className = 'diff-marker';
  marker.textContent = line.kind === 'add' ? '+' : line.kind === 'delete' ? '−' : '';
  const code = document.createElement('code');
  code.className = 'diff-code';
  if (line.kind === 'notice') {
    code.textContent = line.text;
  } else {
    appendTokens(code, line.tokens);
  }
  row.append(oldNumber, newNumber, marker, code);
  return row;
}

function collapsedDiffRegion(lines) {
  const row = document.createElement('div');
  row.className = 'diff-collapse';
  const icon = iconElement([
    ['path', { d: 'm8 9 4-4 4 4' }],
    ['path', { d: 'm16 15-4 4-4-4' }]
  ]);
  const label = document.createElement('span');
  label.textContent = `${lines} unmodified ${lines === 1 ? 'line' : 'lines'}`;
  row.append(icon, label);
  return row;
}

function renderPatch(file) {
  elements.diff.replaceChildren();
  renderDiffFileHeader(file);
  if (!file) return;
  if (file.hunks.length === 0) {
    const empty = document.createElement('div');
    empty.className = 'panel-empty';
    empty.textContent = 'File metadata or binary contents changed.';
    elements.diff.append(empty);
    return;
  }
  let previousHunk = null;
  for (const hunk of file.hunks) {
    const hiddenLines = unchangedLinesBefore(hunk, previousHunk);
    if (hiddenLines > 0) {
      elements.diff.append(collapsedDiffRegion(hiddenLines));
    }
    for (const line of highlightedHunkLines(hunk, file.path)) {
      elements.diff.append(diffLineElement(line));
    }
    previousHunk = hunk;
  }
  elements.diff.scrollTop = 0;
  elements.diff.scrollLeft = 0;
}

function renderDiffFileList(files, selectedFile, conversationId) {
  elements.fileList.replaceChildren();
  const query = (state.diffFileQueries.get(conversationId) || '').trim().toLowerCase();
  const visibleFiles = files.filter((file) => file.path.toLowerCase().includes(query));
  if (visibleFiles.length === 0) {
    const empty = document.createElement('div');
    empty.className = 'file-list-empty';
    empty.textContent = 'No matching files';
    elements.fileList.append(empty);
    return;
  }
  const groups = new Map();
  for (const file of visibleFiles) {
    if (!groups.has(file.presentation.directory)) groups.set(file.presentation.directory, []);
    groups.get(file.presentation.directory).push(file);
  }
  for (const [directory, groupFiles] of groups) {
    const group = document.createElement('section');
    group.className = 'file-group';
    const heading = document.createElement('div');
    heading.className = 'file-group-heading';
    const chevron = iconElement([['path', { d: 'm8 10 4 4 4-4' }]]);
    const directoryLabel = document.createElement('span');
    directoryLabel.textContent = directory;
    heading.append(chevron, directoryLabel);
    group.append(heading);
    for (const file of groupFiles) {
      const button = document.createElement('button');
      button.type = 'button';
      button.className = 'file-button';
      const selected = file.path === selectedFile.path;
      button.classList.toggle('is-active', selected);
      button.setAttribute('aria-pressed', String(selected));
      button.title = file.path;
      const identity = document.createElement('span');
      identity.className = 'file-button-identity';
      const name = document.createElement('span');
      name.className = 'file-button-name';
      name.textContent = file.presentation.name;
      identity.append(fileBadgeElement(file), name);
      const meta = document.createElement('span');
      meta.className = 'file-button-meta';
      const stats = changeStatsElement(file.stats, 'is-compact');
      const status = document.createElement('span');
      status.className = `file-status is-${file.status}`;
      status.setAttribute('aria-label', file.status);
      meta.append(stats, status);
      button.append(identity, meta);
      button.addEventListener('click', () => {
        if (state.selectedId !== conversationId) return;
        state.selectedDiffFiles.set(conversationId, file.path);
        renderDiffFileList(files, file, conversationId);
        renderPatch(file);
      });
      group.append(button);
    }
    elements.fileList.append(group);
  }
}

function renderDiff(value) {
  const patch = String(value || '');
  const hasChanges = patch.trim().length > 0;
  elements.changesToggle.hidden = !hasChanges;
  if (!hasChanges && state.changesOpen) {
    state.changesOpen = false;
    updateInspectorLayout();
  }
  elements.changesPane.classList.toggle('is-empty', !hasChanges);
  elements.fileList.replaceChildren();
  elements.diff.replaceChildren();
  renderDiffFileHeader(null);
  const files = filePatchesFromPatch(patch);
  renderChangeCount(lineCounts(files));
  if (!hasChanges || files.length === 0) {
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
  elements.fileFilter.value = state.diffFileQueries.get(conversationId) || '';
  renderDiffFileList(files, selectedFile, conversationId);
  renderPatch(selectedFile);
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
    renderDiffFileHeader(null);
    elements.fileList.replaceChildren();
    elements.diff.textContent = 'Loading changes…';
  }
  try {
    const patch = await tauri.invoke('get_diff', { conversation: id });
    state.diffs.set(id, patch);
    if (state.selectedId === id) renderDiff(patch);
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
        if (state.selectedAction?.conversationId === id
          && state.selectedAction.call?.toolUseId === call.toolUseId) {
          state.selectedAction.call = call;
          renderActionResult();
        }
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
}

async function sendCurrentMessage() {
  let message = elements.prompt.value.trim();
  let updateTree = false;
  const command = parseComposerCommand(message);
  if (command) {
    if (command.kind === 'commands') {
      clearComposer();
      setCommandPalette(true);
      return;
    }
    if (command.kind === 'help') {
      clearComposer();
      setShortcutHelp(true);
      return;
    }
    if (command.kind === 'rename') {
      clearComposer();
      if (!command.argument) {
        setStatus('Usage: /rename <new title>');
        elements.prompt.focus({ preventScroll: true });
        return;
      }
      await renameSelectedConversation(command.argument);
      return;
    }
    if (command.kind === 'from') {
      clearComposer();
      if (!command.argument) {
        setStatus('Usage: /from <commit>');
        elements.prompt.focus({ preventScroll: true });
        return;
      }
      await createConversation(command.argument);
      return;
    }
    if (command.kind === 'update-tree') {
      if (!command.argument) {
        setStatus('Usage: /update-tree <message>');
        return;
      }
      message = command.argument;
      updateTree = true;
    }
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
    const completion = await tauri.invoke('send_message', {
      conversation: id,
      message,
      title: conversation.title,
      model: selectedModel() || null,
      updateTree,
      onEvent
    });
    conversation.title = completion.title;
    finishActivityGroup(id);
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

async function createConversation(base = null) {
  const existingDraft = !base
    ? state.conversations.find((item) => item.draft && !item.started)
    : null;
  if (existingDraft) {
    await selectConversation(existingDraft.id);
    return;
  }
  if (state.creatingConversation) return;
  state.creatingConversation = true;
  elements.newTask.disabled = true;
  const inheritedModel = selectedModel();
  try {
    saveSelectedDraft();
    const conversation = await tauri.invoke('new_conversation', { base });
    state.conversations.unshift(conversation);
    state.histories.set(conversation.id, []);
    state.conversationModels.set(conversation.id, inheritedModel);
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
    const [payload] = await Promise.all([
      tauri.invoke('bootstrap'),
      initializeHighlighting()
    ]);
    state.repo = payload;
    state.initialModel = payload.initialModel || '';
    state.modelChoices = modelChoices(state.initialModel);
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

elements.newTask.addEventListener('click', () => createConversation());
elements.composer.addEventListener('submit', (event) => {
  event.preventDefault();
  sendCurrentMessage();
});
elements.prompt.addEventListener('input', () => {
  state.composerDrafts.set(state.selectedId, elements.prompt.value);
  state.slashCommandSelection = 0;
  state.slashCommandDismissed = false;
  resizePrompt();
  renderSlashCommandMenu();
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
  if (!elements.slashCommandMenu.hidden && !event.ctrlKey && !event.metaKey) {
    const matches = slashCommandMatches(elements.prompt.value);
    if (event.key === 'ArrowUp' || event.key === 'ArrowDown') {
      event.preventDefault();
      const amount = event.key === 'ArrowUp' ? -1 : 1;
      state.slashCommandSelection =
        (state.slashCommandSelection + amount + matches.length) % matches.length;
      renderSlashCommandMenu();
      return;
    }
    if (event.key === 'Tab' || event.key === 'Enter') {
      event.preventDefault();
      completeSlashCommand();
      return;
    }
    if (event.key === 'Escape') {
      event.preventDefault();
      state.slashCommandDismissed = true;
      renderSlashCommandMenu();
      return;
    }
  }
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
  if (state.publishDialogOpen) {
    if (event.ctrlKey && !event.shiftKey && key === 'p') {
      event.preventDefault();
      confirmPublish();
    } else if (event.key === 'Escape') {
      event.preventDefault();
      setPublishDialog(false);
    }
    return;
  }
  if (state.archiveDialogOpen) {
    if (event.key === 'Escape') {
      event.preventDefault();
      setArchiveDialog(false);
    }
    return;
  }
  if (state.modelMenuOpen && event.key === 'Escape') {
    event.preventDefault();
    setModelMenu(false);
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
  } else if (event.ctrlKey && !event.shiftKey && key === 'l') {
    event.preventDefault();
    checkoutSelectedConversation();
  } else if (event.ctrlKey && !event.shiftKey && key === 'p') {
    event.preventDefault();
    openPublishDialog();
  } else if (event.ctrlKey && !event.shiftKey && key === 'e'
    && event.target !== elements.prompt) {
    event.preventDefault();
    archiveSelectedConversation();
  } else if (event.ctrlKey && event.shiftKey && key === 't') {
    event.preventDefault();
    toggleToolsPane();
  } else if (event.ctrlKey && key === 'q') {
    event.preventDefault();
    toggleChangesPane();
  } else if (event.ctrlKey && key === 'r') {
    event.preventDefault();
    reloadSelectedConversation();
  } else if (event.key === 'Escape' && (state.changesOpen || state.selectedAction)) {
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

elements.modelButton.addEventListener('click', (event) => {
  event.stopPropagation();
  setModelMenu(!state.modelMenuOpen);
});

document.addEventListener('click', (event) => {
  if (state.modelMenuOpen && !event.target.closest('.model-control')) setModelMenu(false);
});

elements.publishDialog.addEventListener('click', (event) => {
  if (event.target.closest('[data-close-publish]')) setPublishDialog(false);
});
elements.publishConfirm.addEventListener('click', confirmPublish);
elements.publishBase.addEventListener('keydown', (event) => {
  if (event.key === 'Enter') {
    event.preventDefault();
    confirmPublish();
  }
});

elements.archiveDialog.addEventListener('click', (event) => {
  if (event.target.closest('[data-close-archives]')) setArchiveDialog(false);
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

elements.changesToggle.addEventListener('click', toggleChangesPane);

elements.fileFilter.addEventListener('input', () => {
  const conversationId = state.selectedId;
  if (!conversationId) return;
  state.diffFileQueries.set(conversationId, elements.fileFilter.value);
  const files = filePatchesFromPatch(state.diffs.get(conversationId) || '');
  if (files.length === 0) return;
  const requestedPath = state.selectedDiffFiles.get(conversationId);
  const selectedFile = files.find((file) => file.path === requestedPath) || files[0];
  renderDiffFileList(files, selectedFile, conversationId);
});

for (const button of document.querySelectorAll('[data-close-pane]')) {
  button.addEventListener('click', () => closeInspectorPane(button.dataset.closePane));
}

function installWidthResizer({
  handle,
  bodyClass,
  defaultWidth,
  currentWidth,
  pointerWidth,
  keyboardDirection,
  setWidth
}) {
  let active = false;
  handle.addEventListener('pointerdown', (event) => {
    if (event.button !== 0) return;
    event.preventDefault();
    active = true;
    document.body.classList.add(bodyClass);
    handle.setPointerCapture(event.pointerId);
    setWidth(pointerWidth(event));
  });
  window.addEventListener('pointermove', (event) => {
    if (active) setWidth(pointerWidth(event));
  });
  const finish = (event) => {
    if (!active) return;
    active = false;
    document.body.classList.remove(bodyClass);
    if (handle.hasPointerCapture(event.pointerId)) handle.releasePointerCapture(event.pointerId);
    setWidth(currentWidth(), true);
  };
  window.addEventListener('pointerup', finish);
  window.addEventListener('pointercancel', finish);
  handle.addEventListener('dblclick', () => setWidth(defaultWidth, true));
  handle.addEventListener('keydown', (event) => {
    if (!['ArrowLeft', 'ArrowRight'].includes(event.key)) return;
    event.preventDefault();
    const direction = event.key === 'ArrowLeft' ? -1 : 1;
    const step = event.shiftKey ? 32 : 12;
    setWidth(currentWidth() + direction * keyboardDirection * step, true);
  });
}

installWidthResizer({
  handle: elements.sidebarResizer,
  bodyClass: 'is-resizing-sidebar',
  defaultWidth: DEFAULT_SIDEBAR_WIDTH,
  currentWidth: () => state.sidebarWidth,
  pointerWidth: (event) => event.clientX,
  keyboardDirection: 1,
  setWidth: setSidebarWidth
});

installWidthResizer({
  handle: elements.inspectorResizer,
  bodyClass: 'is-resizing-inspector',
  defaultWidth: DEFAULT_INSPECTOR_WIDTH,
  currentWidth: () => state.inspectorWidth,
  pointerWidth: (event) => window.innerWidth - event.clientX,
  keyboardDirection: -1,
  setWidth: setInspectorWidth
});

window.addEventListener('resize', () => {
  setInspectorWidth(state.inspectorWidth);
  setSidebarWidth(state.sidebarWidth);
});

restoreUiZoom();
restoreSidebarWidth();
restoreInspectorWidth();
initialize();
