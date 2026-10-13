// view-aliases.js — simple table editor: model-name aliases (from → to).
// The Alias shape (mr-core/src/model.rs): { from, to, generated }. Editing means delete + add —
// the API exposes POST and DELETE only (PUT /api/aliases/{name} does not exist in the contract).

import { api } from './api.js';
import { h, chip, field, emptyState, errorBlock, applyFieldErrors, clearFieldErrors } from './dom.js';
import { extractList } from './format.js';

export async function renderAliases(mount, ctx) {
  let aliases = [];

  const tableWrap = h('div', { class: 'table-scroll' });
  const count = h('span', { class: 'count' });

  const fromIn = h('input', { class: 'input mono', type: 'text', required: true, 'data-field': 'from', placeholder: 'alias name' });
  const toIn = h('input', { class: 'input mono', type: 'text', required: true, 'data-field': 'to', placeholder: 'target model or combo' });
  const addBtn = h('button', { class: 'btn btn-primary', type: 'submit' }, 'Add alias');
  const addForm = h('form', { class: 'card', novalidate: true },
    h('h2', {}, 'Add alias'),
    h('p', { class: 'hint' }, 'A request for “from” is rewritten and served by “to”. Aliases live in the same namespace as models and combos.'),
    h('div', { class: 'grid-fields' },
      field('From', fromIn),
      field('To', toIn)),
    addBtn);

  mount.replaceChildren(
    h('div', { class: 'toolbar' }, h('span', { class: 'spacer' }), count),
    addForm,
    tableWrap,
  );

  addForm.addEventListener('submit', async (e) => {
    e.preventDefault();
    clearFieldErrors(addForm);
    addBtn.disabled = true;
    try {
      await api.post('/api/aliases', { from: fromIn.value.trim(), to: toIn.value.trim() });
      ctx.toast.success(`Alias ${fromIn.value.trim()} → ${toIn.value.trim()} added.`);
      fromIn.value = '';
      toIn.value = '';
      await reload();
    } catch (err) {
      applyFieldErrors(addForm, err);
      if (!err.details?.length) ctx.toast.error('Could not add the alias', err);
      addBtn.disabled = false;
    }
  });

  await reload();

  async function reload() {
    try {
      aliases = extractList(await api.get('/api/aliases'), 'aliases');
    } catch (err) {
      tableWrap.replaceChildren(errorBlock(err));
      return;
    }
    count.textContent = `${aliases.length} alias${aliases.length === 1 ? '' : 'es'}`;
    renderTable();
  }

  function renderTable() {
    if (!aliases.length) {
      tableWrap.replaceChildren(emptyState('No aliases.', 'Add one above, e.g. fast → Brains.'));
      return;
    }
    const tbody = h('tbody');
    for (const a of aliases) {
      const del = h('button', {
        class: 'btn btn-danger btn-sm', type: 'button',
        'aria-label': `Delete alias ${a.from}`,
      }, 'Delete');
      del.addEventListener('click', async () => {
        const ok = await ctx.confirm({
          title: `Delete alias “${a.from}”?`,
          message: `Requests for ${a.from} will stop resolving to ${a.to}.`,
          confirmLabel: 'Delete alias',
        });
        if (!ok) return;
        try {
          await api.del(`/api/aliases/${encodeURIComponent(a.from)}`);
          ctx.toast.success('Alias deleted.');
          await reload();
        } catch (err) {
          ctx.toast.error('Delete failed', err);
        }
      });
      tbody.append(h('tr', {},
        h('td', { class: 'mono' }, a.from),
        h('td', { class: 'hint' }, '→'),
        h('td', { class: 'mono' }, a.to),
        h('td', {}, a.generated ? chip('generated', 'chip-neutral', 'Created automatically, e.g. from provider metadata') : null),
        h('td', { class: 'actions' }, del)));
    }
    tableWrap.replaceChildren(h('table', { class: 'tbl' },
      h('thead', {}, h('tr', {},
        h('th', { scope: 'col' }, 'From'),
        h('th', { scope: 'col' }, ''),
        h('th', { scope: 'col' }, 'To'),
        h('th', { scope: 'col' }, ''),
        h('th', { scope: 'col' }, ''))),
      tbody));
  }
}
