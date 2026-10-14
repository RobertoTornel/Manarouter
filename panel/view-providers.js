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
  h, icon, chip, field, kv, emptyState, errorBlock, applyFieldErrors, clearFieldErrors,
} from './dom.js';
import {
  deriveProviderHealth, accountState, formatAge, formatCompact,
} from './format.js';

const DIALECTS = ['open-ai', 'anthropic', 'google', 'codex', 'copilot', 'antigravity', 'command-code'];
const KEY_STRATEGIES = ['priority', 'round-robin', 'random', 'least-used'];
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

  const tableWrap = h('div', { class: 'table-scroll' });
  const count = h('span', { class: 'count' });
  const newBtn = h('button', { class: 'btn btn-primary', type: 'button' }, 'New provider');
  const refreshBtn = h('button', { class: 'btn', type: 'button' }, 'Refresh');

  mount.replaceChildren(
    h('div', { class: 'toolbar' }, newBtn, refreshBtn, h('span', { class: 'spacer' }), count),
    tableWrap,
  );

  newBtn.addEventListener('click', () => openFormDrawer(null));
  refreshBtn.addEventListener('click', () => reload(true));

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

      const tr = h('tr', { class: 'row-click', tabindex: '0' },
        h('td', { class: 'mono' }, p.id),
        h('td', {}, p.label ?? ''),
        h('td', {}, chip(p.dialect ?? '—')),
        h('td', {}, accountSummary(accounts, enabledCount)),
        h('td', { class: 'num' }, String(models.length)),
        h('td', {}, chip(health.label, health.cls, `health: ${health.key}`)),
        h('td', { class: 'actions' }, openBtn),
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
        h('th', { scope: 'col' }, 'Label'),
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
    body.append(meta, kv([
      ['base URL', h('span', { class: 'mono' }, p.base_url ?? '—')],
      ['auth', authLabel(p.auth)],
      ['key strategy', h('span', { class: 'mono' }, p.key_strategy ?? 'priority')],
      ['note', p.note ?? '—'],
    ]));

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
      // Destination of this confirm: DELETE /api/providers/{id}. Red, because it is destructive.
      const ok = await ctx.confirm({
        title: `Delete provider “${p.id}”?`,
        message: 'Combos and aliases referencing its models will stop resolving.',
        warnLines: warns,
        confirmLabel: 'Delete provider',
        danger: true,
      });
      if (!ok) return;
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
          h('td', { class: 'num' }, String(a.priority ?? 0)),
          h('td', { class: 'num' }, String(a.weight ?? 0)),
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
      accSection.append(emptyState('No accounts.', 'Add an account to give this provider credentials.'));
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
        tb.append(h('tr', {},
          h('td', { class: 'mono' }, m.id),
          h('td', {}, chip(m.source ?? 'synced', 'chip-neutral', 'source')),
          m.hidden ? h('td', {}, chip('hidden')) : h('td', {}),
          h('td', { class: 'num' }, m.context_length ? formatCompact(m.context_length) : ''),
          h('td', { class: 'actions' }, toggle, del)));
      }
      modelSection.append(h('div', { class: 'table-scroll', style: 'max-height:220px;' },
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

    // --- timeouts / headers / overrides (read-only facts) ---
    body.append(
      h('section', {}, h('h3', {}, 'Timeouts'), kv([
        ['connect', ms(p.connect_timeout_ms)], ['TTFB', ms(p.ttfb_timeout_ms)],
        ['idle', ms(p.idle_timeout_ms)], ['total', ms(p.total_timeout_ms)],
        ['prewarm', p.prewarm ? 'yes' : 'no'],
      ])),
      h('section', {}, h('h3', {}, 'Headers'),
        mapKvSection(p.headers)),
      h('section', {}, h('h3', {}, 'Route overrides'),
        mapKvSection(p.route_overrides)),
    );
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

  function buildOAuthAccountForm(p) {
    const status = h('p', { class: 'muted' }, 'OAuth credentials are stored only in the encrypted local vault.');
    const connect = h('button', { class: 'btn primary', type: 'button' }, 'Connect account');
    const form = h('div', {}, status, connect);
    connect.addEventListener('click', async () => {
      connect.disabled = true;
      try {
        const platform = p.auth.platform;
        const { platforms = [] } = await api.get('/api/oauth');
        const details = platforms.find((x) => x.id === platform);
        if (!details) throw new Error(`OAuth platform “${platform}” is unavailable in this build.`);
        if (details.flow === 'browser') {
          const start = await api.post(`/api/oauth/${encodeURIComponent(platform)}/authorize`, { provider_id: p.id });
          window.open(start.authorization_url, '_blank', 'noopener,noreferrer');
          status.textContent = 'Complete sign-in in the browser. This router must be running on the same machine as the browser callback.';
          const done = await api.post(`/api/oauth/sessions/${encodeURIComponent(start.session_id)}/complete`, { provider_id: p.id });
          ctx.toast.success(`Connected ${done.account.label || 'OAuth account'}.`);
        } else {
          const start = await api.post(`/api/oauth/${encodeURIComponent(platform)}/device-code`, { provider_id: p.id });
          status.replaceChildren(h('span', {}, `Enter code ${start.user_code} at `), h('a', { href: start.verification_uri, target: '_blank', rel: 'noopener noreferrer' }, start.verification_uri));
          const deadline = Date.now() + start.expires_in_seconds * 1000;
          while (Date.now() < deadline) {
            await new Promise((resolve) => setTimeout(resolve, Math.max(1000, start.interval_seconds * 1000)));
            const result = await api.post(`/api/oauth/sessions/${encodeURIComponent(start.session_id)}/poll`, { provider_id: p.id });
            if (result.status === 'complete') { ctx.toast.success(`Connected ${result.account.label || 'OAuth account'}.`); await reload(true); return; }
            if (result.status === 'failed') throw new Error(result.message);
            if (result.interval_seconds) start.interval_seconds = result.interval_seconds;
            if (result.retry_after_seconds) start.interval_seconds = Math.max(start.interval_seconds, result.retry_after_seconds);
          }
          if (Date.now() >= deadline) throw new Error('OAuth device code expired. Start again.');
        }
        await reload(true);
      } catch (err) {
        ctx.toast.error('OAuth sign-in failed', err);
      } finally { connect.disabled = false; }
    });
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
    const prewarm = h('input', { type: 'checkbox' });
    prewarm.checked = p?.prewarm ?? true;
    const enabled = h('input', { type: 'checkbox' });
    enabled.checked = p?.enabled ?? true;

    const save = h('button', { class: 'btn btn-primary', type: 'submit' }, isEdit ? 'Save changes' : 'Create provider');
    const cancel = h('button', { class: 'btn', type: 'button' }, 'Cancel');
    cancel.addEventListener('click', () => closeDrawer());

    const form = h('form', { novalidate: true },
      h('div', { class: 'grid-fields' },
        field('ID (immutable once created)', idIn, { hint: 'Used in combos as the provider reference.' }),
        field('Label', labelIn)),
      h('div', { class: 'grid-fields' },
        field('Dialect', dialectIn),
        field('Key strategy', keyStrat)),
      field('Base URL', baseUrl),
      h('fieldset', { class: 'plain' }, h('legend', {}, 'Auth'), authSel, authBox),
      h('fieldset', { class: 'plain' }, h('legend', {}, 'Timeouts'), timeouts),
      h('div', { class: 'check-row' }, prewarm, h('label', {}, 'Prewarm connections at startup')),
      h('div', { class: 'check-row' }, enabled, h('label', {}, 'Provider enabled')),
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
      payload.prewarm = prewarm.checked;
      payload.enabled = enabled.checked;
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
