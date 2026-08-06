(function commandHelpers(globalScope) {
  const SLASH_COMMANDS = [
    { name: '/from', usage: '/from <commit>', description: 'Start a conversation from a completed turn' },
    { name: '/help', usage: '/help', description: 'Show keyboard shortcuts and slash commands' },
    { name: '/title', usage: '/title <new title>', description: 'Rename the selected conversation' },
    { name: '/rename', usage: '/rename <new title>', description: 'Rename the selected conversation' },
    { name: '/update-tree', usage: '/update-tree <message>', description: 'Fold working-tree edits into the turn' },
    { name: '/commands', usage: '/commands', description: 'Open the searchable command palette' }
  ];

  const BASE_MODELS = [
    { value: '', label: 'Auto', detail: 'CAOS default' },
    { value: 'claude-opus-4-8', label: 'Opus 4.8', detail: 'Most capable' },
    { value: 'claude-sonnet-5', label: 'Sonnet 5', detail: 'Faster' }
  ];

  function parseComposerCommand(text) {
    const match = String(text || '').match(/^\/(commands|help|rename|title|from|update-tree)(?:\s+([\s\S]*))?$/u);
    if (!match) return null;
    const kind = match[1] === 'title' ? 'rename' : match[1];
    return { kind, argument: (match[2] || '').trim() };
  }

  function slashCommandMatches(text) {
    const value = String(text || '');
    if (!value.startsWith('/') || /\s/u.test(value)) return [];
    const query = value.toLowerCase();
    return SLASH_COMMANDS.filter((command) => command.name.startsWith(query));
  }

  function modelChoices(initialModel) {
    const initial = String(initialModel || '').trim();
    if (!initial || BASE_MODELS.some((model) => model.value === initial)) return [...BASE_MODELS];
    return [...BASE_MODELS, { value: initial, label: initial, detail: 'From --model' }];
  }

  function modelLabel(value, choices = BASE_MODELS) {
    const model = choices.find((choice) => choice.value === String(value || ''));
    return model?.label || String(value || '') || 'Auto';
  }

  const api = {
    BASE_MODELS,
    SLASH_COMMANDS,
    modelChoices,
    modelLabel,
    parseComposerCommand,
    slashCommandMatches
  };
  globalScope.CaosCommands = api;
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
}(typeof window === 'undefined' ? globalThis : window));
