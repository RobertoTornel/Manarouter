// view-providers.js — dense provider table + per-provider detail drawer.
//
// Table: id, label, dialect, accounts (state chips), models count, health.
// Drawer: accounts (add/enable/disable/delete), models (add manual / hide / delete),
// timeouts, headers, route overrides, and the per-provider "sync models" action.
//
// NOTE ON SERVER STRINGS: provider ids, labels, model ids and error text come from the server
// and from third-party catalogues. Everything is attached with textContent (via h()); there is
// deliberately no innerHTML anywhere in this file.

import { api } from './api.js';
import {
  h, icon, chip, field, toggle, kv, emptyState, errorBlock, applyFieldErrors, clearFieldErrors,
} from './dom.js';
import {
  deriveProviderHealth, accountState, formatAge, formatCompact,
} from './format.js';

// Must list exactly the spellings `Dialect::parse` accepts (mr-core `dialect.rs`). A value here
// that the backend rejects turns "Add provider" into a save-time error, which is how the
// pre-rename `command-code` spelling was caught.
const DIALECTS = ['open-ai', 'anthropic', 'google', 'codex', 'copilot', 'antigravity', 'command-code-go'];
const KEY_STRATEGIES = ['priority', 'round-robin', 'random', 'least-used'];
const DELETE_PROVIDER_CONFIRM_KEY = 'manarouter.confirm.deleteProvider';
// Sourced from catalog/providers.toml and the OAuth dialect docs. These are documented production
// endpoints, but remain suggestions: account-specific gateways or a deliberate proxy can differ.
const DIALECT_BASE_URLS = {
  codex: 'https://chatgpt.com/backend-api/codex',
  antigravity: 'https://cloudcode-pa.googleapis.com',
  'command-code-go': 'https://api.commandcode.ai',
  copilot: 'https://api.githubcopilot.com',
};
const AUTH_KINDS = [
  ['bearer', 'Authorization: Bearer'],
  ['api-key-header', 'API key header (x-api-key)'],
  ['header', 'Custom header'],
  ['query-param', 'Query parameter'],
  ['oauth', 'OAuth (platform-managed)'],
];

export async function renderProviders(mount, ctx) {
  let providers = [];
  let drawerId = null;
  // Keep in-progress inline number edits across any redraw caused by a refresh or another action.
  const inlineDrafts = new Map();

  const tableWrap = h('div', { class: 'table-scroll' });
  const count = h('span', { class: 'count' });
  const newBtn = h('button', { class: 'btn btn-primary', type: 'button' }, 'New provider');
  const refreshBtn = h('button', { class: 'btn', type: 'button' }, 'Refresh');
  // The preference is an explicit opt-out from warnings, never an automatic delete action. Keeping
  // the checkbox in the list toolbar also makes reversing it discoverable without opening a drawer.
  const deleteWarning = toggle('Do not warn me again', {
    checked: deleteProviderWarningDisabled(),
    hint: 'Provider deletion still requires an explicit Delete click.',
  });
  deleteWarning.input.addEventListener('change', () => {
    try {
      if (deleteWarning.input.checked) localStorage.setItem(DELETE_PROVIDER_CONFIRM_KEY, 'true');
      else localStorage.removeItem(DELETE_PROVIDER_CONFIRM_KEY);
    } catch {
      ctx.toast.warning('This browser could not save the provider deletion preference.');
    }
  });

  mount.replaceChildren(
    h('div', { class: 'toolbar' }, newBtn, refreshBtn, deleteWarning.row, h('span', { class: 'spacer' }), count),
    tableWrap,
  );

  newBtn.addEventListener('click', () => openFormDrawer(null));
  refreshBtn.addEventListener('click', () => reload(true));

  function deleteProviderWarningDisabled() {
    try { return localStorage.getItem(DELETE_PROVIDER_CONFIRM_KEY) === 'true'; }
    catch { return false; }
  }

  function refreshInlineDrawer(providerId) {
    const p = providers.find((provider) => provider.id === providerId);
    const body = mount.querySelector('.drawer .drawer-body');
    if (p && drawerId === providerId && body) renderDetail(body, p);
  }

  async function reload(force = false) {
    try {
      providers = (await ctx.loadProviders(force)).slice().sort((a, b) => String(a.id).localeCompare(String(b.id)));
    } catch (err) {
      tableWrap.replaceChildren(errorBlock(err));
      return;
    }
    renderTable();
    if (drawerId) {
      const fresh = providers.find((p) => p.id === drawerId);
      if (fresh) openDetailDrawer(fresh, { silent: true });
      else closeDrawer();
    }
  }

  function renderTable() {
    count.textContent = `${providers.length} provider${providers.length === 1 ? '' : 's'}`;
    if (!providers.length) {
      tableWrap.replaceChildren(emptyState('No providers configured yet.',
        'A provider is an upstream account source (OpenAI, Anthropic, a proxy…) that the router forwards to.'));
      return;
    }
    const tbody = h('tbody');
    for (const p of providers) {
      const health = deriveProviderHealth(p);
      const accounts = Array.isArray(p.accounts) ? p.accounts : [];
      const enabledCount = accounts.filter((a) => a && a.enabled !== false).length;
      const models = Array.isArray(p.models) ? p.models : [];

      const openBtn = h('button', { class: 'btn btn-sm', type: 'button' }, 'Open');
      openBtn.addEventListener('click', (e) => { e.stopPropagation(); openDetailDrawer(p); });

      // An OAuth provider with no accounts cannot serve anything, and the fix is a single action.
      // Surfacing it on the row means "sign in" is one click from the list rather than: open the
      // drawer, scroll to Accounts, find Connect account.
      const needsOauth = p.auth?.type === 'oauth' && accounts.length === 0;

      const tr = h('tr', { class: 'row-click', tabindex: '0' },
        h('td', { class: 'mono' }, p.id),
        h('td', {}, p.label ?? ''),
        h('td', {}, chip(p.dialect ?? '—')),
        h('td', {}, accountSummary(accounts, enabledCount)),
        h('td', { class: 'num' }, String(models.length)),
        h('td', {}, chip(health.label, health.cls, `health: ${health.key}`)),
        h('td', { class: 'actions' },
          ...(needsOauth
            ? [(() => {
                const connect = h('button', { class: 'btn btn-sm btn-primary', type: 'button' }, 'Connect');
                connect.addEventListener('click', (e) => { e.stopPropagation(); openDetailDrawer(p); });
                return connect;
              })()]
            : []),
          openBtn),
      );
      const activate = () => openDetailDrawer(p);
      tr.addEventListener('click', activate);
      // Enter opens only when the row itself is focused; Enter on the inner Open button must
      // not double-trigger through bubbling.
      tr.addEventListener('keydown', (e) => { if (e.key === 'Enter' && e.target === tr) activate(); });
      tbody.append(tr);
    }
    tableWrap.replaceChildren(h('table', { class: 'tbl' },
      h('thead', {}, h('tr', {},
        h('th', { scope: 'col' }, 'ID'),
        h('th', { scope: 'col' }, 'Provider Name'),
        h('th', { scope: 'col' }, 'Dialect'),
        h('th', { scope: 'col' }, 'Accounts'),
        h('th', { scope: 'col' }, 'Models'),
        h('th', { scope: 'col' }, 'Health'),
        h('th', { scope: 'col' }, ''))),
      tbody));
  }

  function accountSummary(accounts, enabledCount) {
    const wrap = h('div', {});
    if (!accounts.length) wrap.append(h('span', { class: 'hint' }, 'none'));
    else {
      wrap.append(h('span', {}, `${enabledCount}/${accounts.length} enabled`));
      for (const a of accounts.slice(0, 4)) {
        const s = accountState(a);
        wrap.append(chip(s.label, s.cls, `account ${a.id ?? ''}: ${s.key}`));
      }
      if (accounts.length > 4) wrap.append(chip(`+${accounts.length - 4}`));
    }
    return wrap;
  }

  // ------------------------------------------------------------------ drawer

  function closeDrawer() {
    drawerId = null;
    mount.querySelector('.drawer')?.remove();
  }

  function drawerShell(title) {
    const head = h('div', { class: 'drawer-head' },
      h('h2', {}, title),
      h('span', { class: 'spacer' }));
    const body = h('div', { class: 'drawer-body' });
    const drawer = h('div', { class: 'drawer', role: 'region', 'aria-label': 'Provider detail' }, head, body);
    drawer.addEventListener('keydown', (e) => { if (e.key === 'Escape') closeDrawer(); });
    mount.append(drawer);
    return { drawer, head, body };
  }

  async function openDetailDrawer(provider) {
    closeDrawer();
    drawerId = provider.id;
    const { head, body } = drawerShell(provider.label ? `${provider.label} (${provider.id})` : String(provider.id));
    head.append(closeButton());
    renderDetail(body, provider);
  }

  function closeButton() {
    const b = h('button', { class: 'icon-btn', type: 'button', 'aria-label': 'Close provider detail' }, icon('x'));
    b.addEventListener('click', closeDrawer);
    return b;
  }

  function renderDetail(body, p) {
    const health = deriveProviderHealth(p);

    body.replaceChildren();

    // --- summary ---
    const meta = h('div', { style: 'display:flex;gap:6px;flex-wrap:wrap;margin-bottom:8px;' },
      chip(p.dialect ?? '—'),
      chip(health.label, health.cls, `health: ${health.key}`),
      p.enabled === false ? chip('provider disabled', 'chip-neutral', 'intentionally off — not an error') : null,
      p.models_synced_at ? chip(`models synced ${formatAge(p.models_synced_at)}`) : null);
    body.append(meta);

    // --- actions ---
    const syncBtn = h('button', { class: 'btn', type: 'button' }, icon('sync'), ' Sync models');
    const editBtn = h('button', { class: 'btn', type: 'button' }, 'Edit');
    const enableBtn = h('button', { class: 'btn', type: 'button' }, p.enabled === false ? 'Enable provider' : 'Disable provider');
    const delBtn = h('button', { class: 'btn btn-danger', type: 'button' }, 'Delete provider');
    const actions = h('div', { class: 'toolbar', style: 'margin-top:10px;' }, syncBtn, editBtn, enableBtn, delBtn);
    body.append(actions);

    syncBtn.addEventListener('click', async () => {
      if (syncBtn.disabled) return; // no double submit while a sync is running
      syncBtn.disabled = true;
      syncBtn.replaceChildren(icon('sync'), ' Syncing…');
      syncBtn.querySelector('svg')?.classList.add('spin');
      try {
        await api.post(`/api/providers/${encodeURIComponent(p.id)}/models/sync`);
        ctx.toast.success(`Model sync for ${p.id} finished.`);
        await reload(true);
      } catch (err) {
        syncBtn.disabled = false;
        syncBtn.replaceChildren(icon('sync'), ' Sync models');
        ctx.toast.error(`Model sync for ${p.id} failed`, err);
      }
    });

    editBtn.addEventListener('click', () => openFormDrawer(p));

    enableBtn.addEventListener('click', async () => {
      enableBtn.disabled = true;
      try {
        // Spread the server object so fields this form does not display survive the round trip.
        await api.put(`/api/providers/${encodeURIComponent(p.id)}`, { ...p, enabled: p.enabled === false });
        ctx.toast.success(`Provider ${p.id} ${p.enabled === false ? 'enabled' : 'disabled'}.`);
        await reload(true);
      } catch (err) {
        enableBtn.disabled = false;
        ctx.toast.error('Could not update the provider', err);
      }
    });

    delBtn.addEventListener('click', async () => {
      const accounts = Array.isArray(p.accounts) ? p.accounts : [];
      const warns = accounts.length
        ? [`This provider has ${accounts.length} account${accounts.length === 1 ? '' : 's'}. Deleting it removes their stored credentials too — they cannot be recovered from the panel.`]
        : [];
      // Even with the warning preference enabled, this handler only runs after the explicit Delete
      // click. The trade-off is one fewer safeguard against an accidental click, not auto-deletion.
      if (!deleteProviderWarningDisabled()) {
        const ok = await ctx.confirm({
          title: `Delete provider “${p.id}”?`,
          message: 'Combos and aliases referencing its models will stop resolving.',
          warnLines: warns,
          confirmLabel: 'Delete provider',
          danger: true,
        });
        if (!ok) return;
      }
      try {
        await api.del(`/api/providers/${encodeURIComponent(p.id)}`);
        ctx.toast.success(`Provider ${p.id} deleted.`);
        closeDrawer();
        await reload(true);
      } catch (err) {
        ctx.toast.error('Delete failed', err);
      }
    });

    // --- accounts ---
    const accounts = Array.isArray(p.accounts) ? p.accounts : [];
    const accSection = h('section', {}, h('h3', {}, `Accounts (${accounts.length})`));
    if (accounts.length) {
      const tb = h('tbody');
      for (const a of accounts) {
        const s = accountState(a);
        const toggle = h('button', {
          class: 'btn btn-sm', type: 'button',
          'aria-label': `${a.enabled === false ? 'Enable' : 'Disable'} account ${a.id}`,
        }, a.enabled === false ? 'Enable' : 'Disable');
        toggle.addEventListener('click', () => toggleAccount(p, a, toggle));
        const del = h('button', {
          class: 'icon-btn danger', type: 'button', 'aria-label': `Delete account ${a.id}`,
        }, icon('trash'));
        del.addEventListener('click', () => deleteAccount(p, a));
        tb.append(h('tr', {},
          h('td', { class: 'mono' }, a.id ?? ''),
          h('td', {}, a.label ?? a.identity ?? ''),
          h('td', {}, chip(s.label, s.cls, `state: ${s.key}`)),
          buildInlineAccountNumber(p, a, 'priority', 'priority'),
          buildInlineAccountNumber(p, a, 'weight', 'weight'),
          h('td', { class: 'actions' }, toggle, del)));
      }
      accSection.append(h('div', { class: 'table-scroll', style: 'max-height:200px;' },
        h('table', { class: 'tbl' },
          h('thead', {}, h('tr', {},
            h('th', { scope: 'col' }, 'ID'), h('th', { scope: 'col' }, 'Label'),
            h('th', { scope: 'col' }, 'State'), h('th', { scope: 'col' }, 'Pri'),
            h('th', { scope: 'col' }, 'Weight'), h('th', { scope: 'col' }, ''))),
          tb)));
    } else {
      accSection.append(emptyState('No accounts.',
        p.auth?.type === 'oauth'
          ? 'Connect an account below to let this provider serve requests.'
          : 'Add an account to give this provider credentials.'));
    }
    accSection.append(buildAccountForm(p));
    body.append(accSection);

    // --- models ---
    const models = Array.isArray(p.models) ? p.models : [];
    const modelSection = h('section', {}, h('h3', {}, `Models (${models.length})`));
    if (models.length) {
      const tb = h('tbody');
      for (const m of models) {
        const toggle = h('button', {
          class: 'btn btn-sm', type: 'button',
          'aria-label': `${m.hidden ? 'Show' : 'Hide'} model ${m.id}`,
        }, m.hidden ? 'Show' : 'Hide');
        toggle.addEventListener('click', () => toggleModelHidden(p, m, toggle));
        const del = h('button', {
          class: 'icon-btn danger', type: 'button', 'aria-label': `Delete model ${m.id}`,
        }, icon('trash'));
        del.addEventListener('click', () => deleteModel(p, m));
        const metadata = [];
        if (Array.isArray(m.reasoning_levels) && m.reasoning_levels.length) {
          metadata.push(...m.reasoning_levels.map((level) => chip(String(level), 'chip-neutral', 'reasoning level')));
        }
        if (Array.isArray(m.input_modalities) && m.input_modalities.length) {
          metadata.push(...m.input_modalities.map((modality) => chip(String(modality), 'chip-neutral', 'input modality')));
        }
        const pricing = modelPricing(m);
        if (pricing) metadata.push(chip(pricing, 'chip-neutral', 'pricing per million tokens'));
        const idCell = h('td', {},
          h('span', { class: 'mono', style: 'overflow-wrap:anywhere;' }, m.id),
          metadata.length ? h('div', { style: 'display:flex;gap:4px;flex-wrap:wrap;margin-top:4px;' }, metadata) : null);
        tb.append(h('tr', {},
          idCell,
          h('td', {}, chip(m.source ?? 'synced', 'chip-neutral', 'source')),
          m.hidden ? h('td', {}, chip('hidden')) : h('td', {}),
          buildInlineModelNumber(p, m, 'context_length', 'context length'),
          h('td', { class: 'actions' }, toggle, del)));
      }
      modelSection.append(h('div', { class: 'table-scroll', style: 'max-height: min(48vh, 460px); overflow:auto;' },
        h('table', { class: 'tbl' },
          h('thead', {}, h('tr', {},
            h('th', { scope: 'col' }, 'Model'), h('th', { scope: 'col' }, 'Source'),
            h('th', { scope: 'col' }, ''), h('th', { scope: 'col' }, 'Ctx'),
            h('th', { scope: 'col' }, ''))),
          tb)));
    } else {
      modelSection.append(emptyState('No models yet.', 'Run “Sync models” or add one manually below.'));
    }
    modelSection.append(buildModelForm(p));
    body.append(modelSection);

    // Base URL, auth and note follow Models so the provider's main content stays easy to scan.
    body.append(h('section', {}, h('h3', {}, 'Connection details'), kv([
      ['base URL', h('span', { class: 'mono' }, p.base_url ?? '—')],
      ['auth', authLabel(p.auth)],
      ['key strategy', h('span', { class: 'mono' }, p.key_strategy ?? 'priority')],
      ['note', p.note ?? '—'],
    ])));

    // --- timeouts / headers / overrides ---
    const timeoutSection = h('section', {}, h('h3', {}, 'Timeouts'));
    const timeoutFields = [
      ['connect_timeout_ms', 'connect'], ['ttfb_timeout_ms', 'TTFB'],
      ['idle_timeout_ms', 'idle'], ['total_timeout_ms', 'total'],
    ];
    for (const [key, label] of timeoutFields) timeoutSection.append(buildInlineNumber(p, key, label, 'Provider timeout in milliseconds.'));
    timeoutSection.append(h('p', { class: 'hint' }, 'Blank values inherit the configured timeout defaults.'));
    const prewarmState = chip(p.prewarm ? 'on' : 'off', 'chip-neutral', 'per-provider prewarm override');
    timeoutSection.append(kv([
      ['prewarm override', h('span', {}, prewarmState, ' ', h('span', { class: 'hint' }, 'Global latency.prewarm also applies server-side.'))],
    ]));
    body.append(
      timeoutSection,
      h('section', {}, h('h3', {}, 'Headers'), mapKvSection(p.headers)),
      h('section', {}, h('h3', {}, 'Route overrides'), mapKvSection(p.route_overrides)),
    );
  }

  function modelPricing(model) {
    const pricing = model.pricing ?? {};
    const input = pricing.input_per_mtok ?? model.input_price_per_mtok;
    const output = pricing.output_per_mtok ?? model.output_price_per_mtok;
    if (input == null && output == null) return null;
    const rate = (value) => value == null ? '—' : formatCompact(Number(value));
    return `$${rate(input)} / $${rate(output)} / 1M in/out`;
  }

  async function updateProviderField(p, patch, errorTitle = 'Could not update the provider') {
    try {
      await api.put(`/api/providers/${encodeURIComponent(p.id)}`, { ...p, ...patch });
      await reload(true);
      return true;
    } catch (err) {
      ctx.toast.error(errorTitle, err);
      return false;
    }
  }

  function buildInlineAccountNumber(p, account, key, label) {
    const slot = `account:${p.id}:${account.id}:${key}`;
    return buildInlineNumericCell(account[key], label, slot, async (value) => {
      await api.put(`/api/providers/${encodeURIComponent(p.id)}/accounts/${encodeURIComponent(account.id)}`,
        { ...account, [key]: value });
      await reload(true);
    }, (err) => ctx.toast.error(`Could not update account ${label}`, err), { allowNegative: key === 'priority' });
  }

  function buildInlineModelNumber(p, model, key, label) {
    const slot = `model:${p.id}:${model.id}:${key}`;
    return buildInlineNumericCell(model[key], label, slot, async (value) => {
      await api.post(`/api/providers/${encodeURIComponent(p.id)}/models`, { ...model, [key]: value });
      await reload(true);
    }, (err) => ctx.toast.error(`Could not update model ${label}`, err), { compact: true });
  }

  function buildInlineNumericCell(value, label, slot, persist, reportError, { compact = false, suffix = '', allowNegative = false } = {}) {
    const cell = h('td', { class: compact ? 'num' : '' });
    const showValue = (n) => n == null ? '—' : `${compact ? formatCompact(Number(n)) : n}${suffix}`;
    const edit = h('button', { class: 'btn btn-sm', type: 'button', 'aria-label': `Edit ${label}` }, 'Edit');
    const renderView = () => cell.replaceChildren(h('span', { class: 'mono' }, showValue(value)), edit);
    const draft = inlineDrafts.get(slot);
    if (draft !== undefined) renderEditor(draft);
    else renderView();

    function renderEditor(initial) {
      const input = h('input', { class: 'input', type: 'number', min: allowNegative ? '-2147483648' : '0', step: '1', value: initial, 'aria-label': label });
      const save = h('button', { class: 'btn btn-sm btn-primary', type: 'button' }, 'Save');
      const cancel = h('button', { class: 'btn btn-sm', type: 'button' }, 'Cancel');
      cell.replaceChildren(h('div', { style: 'display:flex;gap:4px;align-items:center;min-width:140px;' }, input, save, cancel));
      input.addEventListener('input', () => inlineDrafts.set(slot, input.value));
      const restore = () => {
        inlineDrafts.delete(slot);
        renderView();
      };
      const finish = async () => {
        if (!input.value.trim()) {
          ctx.toast.warning(`Enter a value for ${label}.`);
          return;
        }
        const number = Number(input.value);
        if (!Number.isSafeInteger(number) || (!allowNegative && number < 0) || (allowNegative && number < -2147483648) || number > (allowNegative ? 2147483647 : 4294967295)) {
          ctx.toast.warning(`${label} must be a whole number in the supported range.`);
          return;
        }
        inlineDrafts.set(slot, input.value);
        save.disabled = true;
        try {
          await persist(number);
          inlineDrafts.delete(slot);
          refreshInlineDrawer(p.id);
        } catch (err) {
          save.disabled = false;
          reportError(err);
        }
      };
      save.addEventListener('click', finish);
      input.addEventListener('keydown', (event) => {
        if (event.key === 'Enter') { event.preventDefault(); finish(); }
        if (event.key === 'Escape') { event.preventDefault(); restore(); }
      });
      cancel.addEventListener('click', restore);
      input.addEventListener('blur', () => {
        // Blur keeps a valid draft for later editing; save is explicit to avoid losing intent.
        inlineDrafts.set(slot, input.value);
      });
    }

    edit.addEventListener('click', () => {
      renderEditor(inlineDrafts.get(slot) ?? value ?? '');
    });
    return cell;
  }

  function buildInlineNumber(p, key, label, hint) {
    const slot = `provider:${p.id}:${key}`;
    const row = h('div', { class: 'field' });
    const value = p[key];
    const draft = inlineDrafts.get(slot);
    const edit = h('button', { class: 'btn btn-sm', type: 'button', 'aria-label': `Edit ${label} timeout` }, 'Edit');
    const renderView = () => {
      row.replaceChildren(h('div', { class: 'label-line' }, h('span', {}, label), edit),
        h('span', { class: 'mono' }, value == null ? '—' : `${value} ms`),
        hint ? h('p', { class: 'hint' }, hint) : null);
    };
    const renderEditor = (initial) => {
      const input = h('input', { class: 'input', type: 'number', min: '0', step: '1', value: initial, 'aria-label': `${label} timeout in milliseconds` });
      const save = h('button', { class: 'btn btn-sm btn-primary', type: 'button' }, 'Save');
      const cancel = h('button', { class: 'btn btn-sm', type: 'button' }, 'Cancel');
      row.replaceChildren(h('div', { class: 'label-line' }, h('span', {}, label)),
        h('div', { style: 'display:flex;gap:6px;align-items:center;' }, input, save, cancel),
        hint ? h('p', { class: 'hint' }, hint) : null);
      input.addEventListener('input', () => inlineDrafts.set(slot, input.value));
      const closeEditor = () => {
        inlineDrafts.delete(slot);
        const latest = providers.find((provider) => provider.id === p.id) ?? p;
        const body = row.closest('.drawer-body');
        if (body) renderDetail(body, latest);
      };
      const finish = async () => {
        if (!input.value.trim()) { ctx.toast.warning('Enter a timeout value in milliseconds.'); return; }
        const number = Number(input.value);
        if (!Number.isSafeInteger(number) || number < 0 || number > 18446744073709551615) { ctx.toast.warning('Timeout must be a non-negative whole number in the supported range.'); return; }
        inlineDrafts.set(slot, input.value);
        save.disabled = true;
        if (await updateProviderField(p, { [key]: number })) {
          inlineDrafts.delete(slot);
          refreshInlineDrawer(p.id);
        } else save.disabled = false;
      };
      save.addEventListener('click', finish);
      input.addEventListener('keydown', (event) => {
        if (event.key === 'Enter') { event.preventDefault(); finish(); }
        if (event.key === 'Escape') { event.preventDefault(); closeEditor(); }
      });
      input.addEventListener('blur', () => inlineDrafts.set(slot, input.value));
      cancel.addEventListener('click', closeEditor);
    };
    if (draft !== undefined) renderEditor(draft);
    else renderView();
    edit.addEventListener('click', () => renderEditor(inlineDrafts.get(slot) ?? value ?? ''));
    return row;
  }

  function ms(v) { return v == null ? '—' : `${v} ms`; }

  function authLabel(auth) {
    if (!auth) return 'bearer (default)';
    if (typeof auth === 'string') return auth;
    const type = auth.type ?? 'bearer';
    const extra = auth.header ?? auth.name ?? auth.param ?? auth.platform ?? '';
    return extra ? `${type} (${extra})` : type;
  }

  function mapKvSection(map) {
    const entries = Object.entries(map ?? {});
    if (!entries.length) return h('p', { class: 'hint' }, 'none');
    return kv(entries.map(([k, v]) => [k, h('span', { class: 'mono' }, String(v))]));
  }

  async function toggleAccount(p, a, btn) {
    btn.disabled = true;
    try {
      await api.put(`/api/providers/${encodeURIComponent(p.id)}/accounts/${encodeURIComponent(a.id)}`,
        { ...a, enabled: a.enabled === false });
      await reload(true);
    } catch (err) {
      btn.disabled = false;
      ctx.toast.error('Could not update the account', err);
    }
  }

  async function deleteAccount(p, a) {
    const ok = await ctx.confirm({
      title: `Delete account “${a.id}”?`,
      message: 'Its stored credential is removed with it.',
      confirmLabel: 'Delete account',
    });
    if (!ok) return;
    try {
      await api.del(`/api/providers/${encodeURIComponent(p.id)}/accounts/${encodeURIComponent(a.id)}`);
      ctx.toast.success('Account deleted.');
      await reload(true);
    } catch (err) {
      ctx.toast.error('Delete failed', err);
    }
  }

  async function toggleModelHidden(p, m, btn) {
    btn.disabled = true;
    try {
      // POST /api/providers/{id}/models is the upsert path; hidden is authored metadata and the
      // manual entry wins over anything a later sync knows (docs/03-configuration.md §models).
      await api.post(`/api/providers/${encodeURIComponent(p.id)}/models`, { ...m, hidden: !m.hidden });
      await reload(true);
    } catch (err) {
      btn.disabled = false;
      ctx.toast.error('Could not change model visibility', err);
    }
  }

  async function deleteModel(p, m) {
    const ok = await ctx.confirm({
      title: `Delete model “${m.id}”?`,
      message: m.source === 'synced'
        ? 'It was synced from the provider; a later sync may bring it back.'
        : 'It was entered manually and will not come back.',
      confirmLabel: 'Delete model',
    });
    if (!ok) return;
    try {
      await api.del(`/api/providers/${encodeURIComponent(p.id)}/models/${encodeURIComponent(m.id)}`);
      ctx.toast.success(`Model ${m.id} deleted.`);
      await reload(true);
    } catch (err) {
      ctx.toast.error('Delete failed', err);
    }
  }

  // ------------------------------------------------------------------ forms

  function buildAccountForm(p) {
    if (p.auth?.type === 'oauth') return buildOAuthAccountForm(p);
    const label = h('input', { class: 'input', type: 'text', 'data-field': 'label' });
    const identity = h('input', { class: 'input mono', type: 'text', 'data-field': 'identity', placeholder: 'e.g. work@account' });
    const secret = h('input', { class: 'input mono', type: 'password', 'data-field': 'secret', autocomplete: 'off' });
    const add = h('button', { class: 'btn', type: 'submit' }, 'Add account');
    const form = h('form', { novalidate: true },
      h('div', { class: 'grid-fields' },
        field('Label', label),
        field('Identity (optional)', identity),
        field('Secret / API key', secret)),
      add);
    form.addEventListener('submit', async (e) => {
      e.preventDefault();
      clearFieldErrors(form);
      add.disabled = true;
      try {
        // Field names are the Account struct's (mr-core/src/model.rs); the secret is sent once
        // and stored server-side as a secret_ref. Validation errors land next to their fields.
        await api.post(`/api/providers/${encodeURIComponent(p.id)}/accounts`,
          { label: label.value || null, identity: identity.value || null, secret: secret.value || null });
        ctx.toast.success('Account added.');
        await reload(true);
      } catch (err) {
        applyFieldErrors(form, err);
        if (!err.details?.length) ctx.toast.error('Could not add the account', err);
        add.disabled = false;
      }
    });
    return form;
  }

  // ------------------------------------------------------------------ OAuth

  /// The one-click sign-in for an OAuth provider.
  ///
  /// Two flows are offered where the platform supports both. The browser redirect is the default
  /// because on the usual deployment — router and browser on one machine — it is a single click.
  /// The device code exists for the cases where a redirect cannot work at all: a headless server, a
  /// container, or a browser on a different host. Offering only the redirect would make those
  /// deployments impossible to log in from the panel.
  function buildOAuthAccountForm(p) {
    const status = h('p', { class: 'muted' }, 'OAuth credentials are stored only in the encrypted local vault.');
    const progress = h('p', { class: 'muted' });
    const actions = h('div', { class: 'toolbar', style: 'padding:0;border:0;background:none;' });
    const form = h('div', {}, status, actions, progress);

    let details = null;

    // Discover what this platform can do before drawing buttons, so the panel never offers a flow
    // that would fail. Until that answers, no button exists — a button that 404s is worse than a
    // brief absence.
    (async () => {
      try {
        const { platforms = [] } = await api.get('/api/oauth');
        details = platforms.find((x) => x.id === p.auth.platform) || null;
      } catch {
        details = null;
      }
      if (!details) {
        form.replaceChildren(h('p', { class: 'muted' },
          `OAuth platform “${p.auth.platform}” is unavailable in this build.`));
        return;
      }
      const flows = Array.isArray(details.flows) && details.flows.length ? details.flows : [details.flow];
      actions.replaceChildren(
        ...(flows.includes('browser')
          ? [actionButton('Connect with browser', 'btn btn-primary', () => runBrowser() )]
          : []),
        ...(flows.includes('device')
          ? [actionButton(flows.includes('browser') ? 'Use a device code instead' : 'Connect with a device code',
              flows.includes('browser') ? 'btn' : 'btn btn-primary', () => runDevice())]
          : []),
      );
    })();

    function actionButton(label, cls, onClick) {
      const b = h('button', { class: cls, type: 'button' }, label);
      b.addEventListener('click', () => {
        for (const el of actions.querySelectorAll('button')) el.disabled = true;
        onClick()
          .catch((err) => {
            // `details` carries the server's own explanation where there is one.
            ctx.toast.error('OAuth sign-in failed', err);
            progress.replaceChildren(errorBlock(err));
          })
          .finally(() => {
            for (const el of actions.querySelectorAll('button')) el.disabled = false;
          });
      });
      return b;
    }

    /// Open the provider's page, then wait for the loopback callback.
    ///
    /// The completion call is issued *after* the tab is opened and blocks until the browser comes
    /// back, which is what makes this a single click: the operator approves in the tab and this
    /// resolves on its own. The window is opened before the await so the browser does not treat it
    /// as a popup triggered by nothing.
    async function runBrowser() {
      const start = await api.post(`/api/oauth/${encodeURIComponent(details.id)}/authorize`, { provider_id: p.id });
      window.open(start.authorization_url, '_blank', 'noopener,noreferrer');
      progress.replaceChildren(
        h('span', {}, 'Waiting for you to finish signing in… '),
        h('a', { href: start.authorization_url, target: '_blank', rel: 'noopener noreferrer' }, 'Reopen the page'),
      );
      const done = await api.post(`/api/oauth/sessions/${encodeURIComponent(start.session_id)}/complete`, { provider_id: p.id });
      progress.replaceChildren(h('span', {}, 'Connected.'));
      ctx.toast.success(`Connected ${done.account?.label || 'OAuth account'}.`);
      await reload(true);
    }

    async function runDevice() {
      const start = await api.post(`/api/oauth/${encodeURIComponent(details.id)}/device-code`, { provider_id: p.id });
      progress.replaceChildren(
        h('span', {}, 'Open '),
        h('a', { href: start.verification_uri, target: '_blank', rel: 'noopener noreferrer' }, start.verification_uri),
        h('span', {}, ' and enter this code: '),
        h('strong', { class: 'mono', style: 'letter-spacing:2px;font-size:15px;' }, start.user_code),
      );
      window.open(start.verification_uri, '_blank', 'noopener,noreferrer');
      const deadline = Date.now() + start.expires_in_seconds * 1000;
      let intervalMs = Math.max(1000, start.interval_seconds * 1000);
      while (Date.now() < deadline) {
        await new Promise((resolve) => setTimeout(resolve, intervalMs));
        const result = await api.post(`/api/oauth/sessions/${encodeURIComponent(start.session_id)}/poll`, { provider_id: p.id });
        if (result.status === 'complete') {
          progress.replaceChildren(h('span', {}, 'Connected.'));
          ctx.toast.success(`Connected ${result.account?.label || 'OAuth account'}.`);
          await reload(true);
          return;
        }
        if (result.status === 'failed') throw new Error(result.message);
        if (result.interval_seconds) intervalMs = Math.max(intervalMs, result.interval_seconds * 1000);
        if (result.retry_after_seconds) intervalMs = Math.max(intervalMs, result.retry_after_seconds * 1000);
      }
      throw new Error('The device code expired before it was approved. Start again.');
    }

    return form;
  }

  function buildModelForm(p) {
    const id = h('input', { class: 'input mono', type: 'text', required: true, 'data-field': 'id', placeholder: 'upstream-model-id' });
    const ctxLen = h('input', { class: 'input', type: 'number', min: '0', 'data-field': 'context_length' });
    const add = h('button', { class: 'btn', type: 'submit' }, 'Add model');
    const form = h('form', { novalidate: true },
      h('div', { class: 'grid-fields' },
        field('Model ID (manual)', id),
        field('Context length (optional)', ctxLen)),
      add);
    form.addEventListener('submit', async (e) => {
      e.preventDefault();
      clearFieldErrors(form);
      add.disabled = true;
      try {
        await api.post(`/api/providers/${encodeURIComponent(p.id)}/models`, {
          id: id.value.trim(),
          source: 'manual', // manual entries are authoritative over later syncs
          context_length: ctxLen.value ? Number(ctxLen.value) : undefined,
        });
        ctx.toast.success(`Model ${id.value.trim()} added.`);
        await reload(true);
      } catch (err) {
        applyFieldErrors(form, err);
        if (!err.details?.length) ctx.toast.error('Could not add the model', err);
        add.disabled = false;
      }
    });
    return form;
  }

  /** Create (p === null) or edit an existing provider. */
  function openFormDrawer(p) {
    closeDrawer();
    const { head, body } = drawerShell(p ? `Edit provider ${p.id}` : 'New provider');
    head.append(closeButton());

    const isEdit = Boolean(p);
    const idIn = h('input', { class: 'input mono', type: 'text', 'data-field': 'id', value: p?.id ?? '', disabled: isEdit });
    const labelIn = h('input', { class: 'input', type: 'text', 'data-field': 'label', value: p?.label ?? '' });
    const dialectIn = h('select', { class: 'input', 'data-field': 'dialect' },
      ...DIALECTS.map((d) => h('option', { value: d, selected: (p?.dialect ?? 'open-ai') === d }, d)));
    const baseUrl = h('input', { class: 'input mono', type: 'text', 'data-field': 'base_url', value: p?.base_url ?? '', placeholder: 'https://…' });
    const note = h('input', { class: 'input', type: 'text', 'data-field': 'note', value: p?.note ?? '' });

    const auth = p?.auth ?? { type: 'bearer' };
    const authType = typeof auth === 'string' ? 'bearer' : (auth.type ?? 'bearer');
    const authSel = h('select', { class: 'input', 'data-field': 'type' },
      ...AUTH_KINDS.map(([v, lbl]) => h('option', { value: v, selected: authType === v }, lbl)));
    const authHeader = h('input', { class: 'input mono', type: 'text', 'data-field': 'header', value: auth.header ?? 'x-api-key' });
    const authName = h('input', { class: 'input mono', type: 'text', 'data-field': 'name', value: auth.name ?? '' });
    const authPrefix = h('input', { class: 'input mono', type: 'text', 'data-field': 'prefix', value: auth.prefix ?? '' });
    const authParam = h('input', { class: 'input mono', type: 'text', 'data-field': 'param', value: auth.param ?? 'key' });
    const authPlatform = h('input', { class: 'input mono', type: 'text', 'data-field': 'platform', value: auth.platform ?? '' });
    const authBox = h('div', {});
    authSel.addEventListener('change', () => renderAuthParams());
    function renderAuthParams() {
      const t = authSel.value;
      authBox.replaceChildren();
      if (t === 'api-key-header') authBox.append(field('Header', authHeader, { hint: 'Header that carries the key.' }));
      else if (t === 'header') authBox.append(h('div', { class: 'grid-fields' },
        field('Header name', authName), field('Prefix (optional)', authPrefix, { hint: 'e.g. “Bearer ”' })));
      else if (t === 'query-param') authBox.append(field('Query parameter', authParam));
      else if (t === 'oauth') authBox.append(field('Platform', authPlatform, { hint: 'OAuth platform id, e.g. codex.' }));
    }
    renderAuthParams();

    const timeouts = h('div', { class: 'grid-fields' });
    const tFields = {};
    for (const t of ['connect_timeout_ms', 'ttfb_timeout_ms', 'idle_timeout_ms', 'total_timeout_ms']) {
      const input = h('input', { class: 'input', type: 'number', min: '0', 'data-field': t, value: p?.[t] ?? '' });
      tFields[t] = input;
      timeouts.append(field(t.replace('_timeout_ms', ' (ms)'), input));
    }

    const keyStrat = h('select', { class: 'input', 'data-field': 'key_strategy' },
      ...KEY_STRATEGIES.map((k) => h('option', { value: k, selected: (p?.key_strategy ?? 'priority') === k }, k)));
    const prewarmToggle = toggle('Prewarm connections at startup', {
      checked: p?.prewarm ?? false,
      hint: 'Per-provider override; global latency.prewarm still applies server-side.',
    });
    const enabledToggle = toggle('Provider enabled', { checked: p?.enabled ?? true });

    // Track user edits rather than testing the current value: an operator who clears the URL
    // intentionally should not have the dialect handler silently fill it back in.
    let baseUrlDirty = false;
    let lastSuggestedBaseUrl = '';
    baseUrl.addEventListener('input', () => { baseUrlDirty = true; });
    const baseUrlSuggestion = h('p', { class: 'hint' });
    dialectIn.addEventListener('change', () => {
      const suggestion = DIALECT_BASE_URLS[dialectIn.value];
      const current = baseUrl.value.trim();
      if (!suggestion || baseUrlDirty || (current && current !== lastSuggestedBaseUrl)) return;
      baseUrl.value = suggestion;
      lastSuggestedBaseUrl = suggestion;
      baseUrlSuggestion.textContent = `Suggested documented endpoint: ${suggestion}. Change it if this deployment uses another endpoint.`;
    });
    if (!isEdit && DIALECT_BASE_URLS[dialectIn.value]) {
      baseUrlSuggestion.textContent = `Suggested documented endpoint for ${dialectIn.value}: ${DIALECT_BASE_URLS[dialectIn.value]}. Change it if this deployment uses another endpoint.`;
    }

    const save = h('button', { class: 'btn btn-primary', type: 'submit' }, isEdit ? 'Save changes' : 'Create provider');
    const cancel = h('button', { class: 'btn', type: 'button' }, 'Cancel');
    cancel.addEventListener('click', () => closeDrawer());

    const form = h('form', { novalidate: true },
      h('div', { class: 'grid-fields' },
        field('Provider ID', idIn, { hint: 'Used in combos as the provider reference; immutable once created.' }),
        field('Provider Name', labelIn)),
      h('div', { class: 'grid-fields' },
        field('Dialect', dialectIn),
        h('div', { class: 'field' }, h('label', {}, 'Key strategy'),
          h('div', { style: 'display:flex;gap:8px;align-items:center;' },
            keyStrat, h('span', { class: 'hint' }, 'Selects the account key rotation mode.')))),
      field('Base URL', baseUrl),
      baseUrlSuggestion,
      h('fieldset', { class: 'plain' }, h('legend', {}, 'Auth'), authSel, authBox),
      h('fieldset', { class: 'plain' }, h('legend', {}, 'Timeouts'), timeouts),
      prewarmToggle.row,
      enabledToggle.row,
      field('Note (optional)', note),
      h('div', { class: 'dialog-actions' }, cancel, save));

    form.addEventListener('submit', async (e) => {
      e.preventDefault();
      clearFieldErrors(form);
      save.disabled = true;
      save.textContent = 'Saving…';
      const payload = isEdit
        ? { ...p } // preserve everything the form does not show (headers, models, extra…)
        : {};
      payload.id = idIn.value.trim();
      payload.label = labelIn.value || null;
      payload.dialect = dialectIn.value;
      payload.base_url = baseUrl.value.trim();
      payload.note = note.value || null;
      payload.key_strategy = keyStrat.value;
      payload.prewarm = prewarmToggle.input.checked;
      payload.enabled = enabledToggle.input.checked;
      const t = authSel.value;
      payload.auth = t === 'bearer' ? { type: 'bearer' }
        : t === 'api-key-header' ? { type: 'api-key-header', header: authHeader.value || 'x-api-key' }
          : t === 'header' ? { type: 'header', name: authName.value, prefix: authPrefix.value }
            : t === 'query-param' ? { type: 'query-param', param: authParam.value || 'key' }
              : { type: 'oauth', platform: authPlatform.value };
      // Timeouts: only send a value when the operator typed one. Assigning undefined would
      // stringify the key away and silently reset the server-side value on an edit.
      for (const [k, input] of Object.entries(tFields)) {
        if (input.value !== '') payload[k] = Number(input.value);
      }

      try {
        if (isEdit) {
          await api.put(`/api/providers/${encodeURIComponent(p.id)}`, payload);
          ctx.toast.success(`Provider ${payload.id} saved.`);
        } else {
          await api.post('/api/providers', payload);
          ctx.toast.success(`Provider ${payload.id} created.`);
        }
        drawerId = payload.id;
        await reload(true);
      } catch (err) {
        applyFieldErrors(form, err);
        if (!err.details?.length) ctx.toast.error('Could not save the provider', err);
        save.disabled = false;
        save.textContent = isEdit ? 'Save changes' : 'Create provider';
      }
    });

    body.append(form);
    if (!isEdit) idIn.focus();
  }

  await reload();
}
