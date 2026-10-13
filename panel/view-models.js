// view-models.js — every model across every provider, filterable by provider and source,
// with hide/show toggles. Source chips are neutral grey on purpose: synced/manual/inferred is
// provenance, not health — semantic colours stay semantic.

import { api } from './api.js';
import { h, chip, emptyState, errorBlock } from './dom.js';
import { formatCompact } from './format.js';

const SOURCES = ['synced', 'manual', 'inferred'];

export async function renderModels(mount, ctx) {
  let providers = [];

  const provSel = h('select', { class: 'input', 'aria-label': 'Filter by provider' });
  const sourceSel = h('select', { class: 'input', 'aria-label': 'Filter by source' },
    h('option', { value: '' }, 'all sources'),
    ...SOURCES.map((s) => h('option', { value: s }, s)));
  const search = h('input', { class: 'input', type: 'search', 'aria-label': 'Search model id or label', placeholder: 'search…' });
  const hiddenOnly = h('input', { type: 'checkbox', id: 'models-hidden-only' });
  const tableWrap = h('div', { class: 'table-scroll' });
  const count = h('span', { class: 'count' });

  for (const el of [provSel, sourceSel, search]) {
    el.addEventListener('change', renderTable);
    el.addEventListener('input', renderTable);
  }
  hiddenOnly.addEventListener('change', renderTable);

  mount.replaceChildren(
    h('div', { class: 'toolbar' },
      provSel, sourceSel, search,
      h('label', { class: 'step-flag', for: 'models-hidden-only' }, hiddenOnly, 'hidden only'),
      h('span', { class: 'spacer' }), count),
    tableWrap,
  );

  await reload();

  async function reload() {
    try {
      providers = await ctx.loadProviders(true);
    } catch (err) {
      tableWrap.replaceChildren(errorBlock(err));
      return;
    }
    provSel.replaceChildren(
      h('option', { value: '' }, 'all providers'),
      ...providers.map((p) => h('option', { value: p.id }, p.id)),
    );
    renderTable();
  }

  function rows() {
    const out = [];
    for (const p of providers) {
      if (provSel.value && p.id !== provSel.value) continue;
      for (const m of p.models ?? []) {
        if (sourceSel.value && (m.source ?? 'synced') !== sourceSel.value) continue;
        if (hiddenOnly.checked && !m.hidden) continue;
        const q = search.value.trim().toLowerCase();
        if (q && !`${m.id} ${m.label ?? ''}`.toLowerCase().includes(q)) continue;
        out.push({ p, m });
      }
    }
    return out;
  }

  function renderTable() {
    const list = rows();
    const totalModels = providers.reduce((n, p) => n + (p.models?.length ?? 0), 0);
    const hiddenCount = providers.reduce((n, p) => n + (p.models ?? []).filter((m) => m.hidden).length, 0);
    count.textContent = `${list.length} shown · ${totalModels} across ${providers.length} providers · ${hiddenCount} hidden`;

    if (!totalModels) {
      tableWrap.replaceChildren(emptyState('No models known.',
        'Models appear here after a provider sync, or when you add one manually in the provider detail.'));
      return;
    }
    if (!list.length) {
      tableWrap.replaceChildren(emptyState('Nothing matches these filters.'));
      return;
    }

    const tbody = h('tbody');
    for (const { p, m } of list) {
      const toggle = h('button', {
        class: 'btn btn-sm', type: 'button',
        'aria-label': `${m.hidden ? 'Show' : 'Hide'} ${m.id} on provider ${p.id}`,
      }, m.hidden ? 'Show' : 'Hide');
      toggle.addEventListener('click', () => toggleHidden(p, m, toggle));
      const del = h('button', {
        class: 'btn btn-danger btn-sm', type: 'button',
        'aria-label': `Delete ${m.id} on provider ${p.id}`,
      }, 'Delete');
      del.addEventListener('click', () => deleteModel(p, m));

      tbody.append(h('tr', {},
        h('td', { class: 'mono' }, p.id),
        h('td', { class: 'mono' }, m.id),
        h('td', {}, m.label ?? ''),
        h('td', {}, chip(m.source ?? 'synced', 'chip-neutral', 'provenance')),
        h('td', { class: 'num' }, m.context_length ? formatCompact(m.context_length) : ''),
        h('td', {}, m.hidden ? chip('hidden', 'chip-neutral', 'excluded from the relay\u2019s model listing') : null),
        h('td', { class: 'actions' }, toggle, del)));
    }
    tableWrap.replaceChildren(h('table', { class: 'tbl' },
      h('thead', {}, h('tr', {},
        h('th', { scope: 'col' }, 'Provider'),
        h('th', { scope: 'col' }, 'Model'),
        h('th', { scope: 'col' }, 'Label'),
        h('th', { scope: 'col' }, 'Source'),
        h('th', { scope: 'col' }, 'Ctx'),
        h('th', { scope: 'col' }, ''),
        h('th', { scope: 'col' }, ''))),
      tbody));
  }

  async function toggleHidden(p, m, btn) {
    btn.disabled = true;
    try {
      // POST /api/providers/{id}/models is the upsert path; `hidden` is authored metadata and
      // manual entries win over a later sync (docs/03-configuration.md). Assumption documented
      // in panel/README.md — if the backend adds a dedicated toggle route, change it here.
      await api.post(`/api/providers/${encodeURIComponent(p.id)}/models`, { ...m, hidden: !m.hidden });
      await reload();
    } catch (err) {
      btn.disabled = false;
      ctx.toast.error('Could not change visibility', err);
    }
  }

  async function deleteModel(p, m) {
    const ok = await ctx.confirm({
      title: `Delete model “${m.id}”?`,
      message: m.source === 'synced'
        ? 'A later sync may re-add it.'
        : 'It was entered manually and will not come back.',
      confirmLabel: 'Delete model',
    });
    if (!ok) return;
    try {
      await api.del(`/api/providers/${encodeURIComponent(p.id)}/models/${encodeURIComponent(m.id)}`);
      ctx.toast.success(`Model ${m.id} deleted.`);
      await reload();
    } catch (err) {
      ctx.toast.error('Delete failed', err);
    }
  }
}
