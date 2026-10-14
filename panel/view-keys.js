// view-keys.js — client keys: list, allow-list editing, and the create flow that shows the
// plaintext exactly once (POST /api/keys returns { key: "mr-...", ... } and never again).

import { api } from './api.js';
import {
  h, chip, field, emptyState, errorBlock, applyFieldErrors, clearFieldErrors,
} from './dom.js';
import { extractList, linesToList, listToLines, formatCompact } from './format.js';

export async function renderKeys(mount, ctx) {
  let keys = [];
  let editingId = null; // null = closed, 'new' = creating, id string = editing

  const tableWrap = h('div', { class: 'table-scroll' });
  const formSlot = h('div');
  const count = h('span', { class: 'count' });
  const newBtn = h('button', { class: 'btn btn-primary', type: 'button' }, 'New key');

  mount.replaceChildren(
    h('div', { class: 'toolbar' }, newBtn, h('span', { class: 'spacer' }), count),
    formSlot,
    tableWrap,
  );

  newBtn.addEventListener('click', () => { editingId = 'new'; renderForm(null); });

  await reload();

  async function reload() {
    try {
      keys = extractList(await api.get('/api/keys'), 'keys', 'client_keys');
    } catch (err) {
      tableWrap.replaceChildren(errorBlock(err));
      return;
    }
    count.textContent = `${keys.length} key${keys.length === 1 ? '' : 's'}`;
    if (editingId) {
      const current = keys.find((k) => k.id === editingId);
      renderForm(current ?? null);
    } else {
      formSlot.replaceChildren();
    }
    renderTable();
  }

  function renderTable() {
    if (!keys.length) {
      tableWrap.replaceChildren(emptyState('No client keys.',
        'Client keys authenticate requests to the relay. Create one to hand to a client.'));
      return;
    }
    const tbody = h('tbody');
    for (const k of keys) {
      const editBtn = h('button', { class: 'btn btn-sm', type: 'button' }, 'Edit');
      editBtn.addEventListener('click', () => { editingId = k.id; renderForm(k); tableWrap.scrollIntoView({ block: 'nearest' }); });
      const rotateBtn = h('button', { class: 'btn btn-sm', type: 'button' }, 'Rotate');
      rotateBtn.addEventListener('click', () => rotateKey(k));
      const delBtn = h('button', { class: 'btn btn-danger btn-sm', type: 'button' }, 'Delete');
      delBtn.addEventListener('click', () => deleteKey(k));
      tbody.append(h('tr', {},
        h('td', {}, k.label ?? ''),
        h('td', { class: 'mono' }, k.prefix ?? '(prefix unavailable)'),
        h('td', {}, k.enabled === false ? chip('disabled', 'chip-neutral') : chip('enabled', 'chip-green')),
        h('td', { class: 'mono' }, allowSummary(k.allow_models)),
        h('td', { class: 'mono' }, allowSummary(k.allow_routes)),
        h('td', { class: 'num' }, k.limit_requests ? formatCompact(k.limit_requests) : '∞'),
        h('td', { class: 'actions' }, editBtn, rotateBtn, delBtn)));
    }
    tableWrap.replaceChildren(h('table', { class: 'tbl' },
      h('thead', {}, h('tr', {},
        h('th', { scope: 'col' }, 'Label'),
        h('th', { scope: 'col' }, 'Prefix'),
        h('th', { scope: 'col' }, 'State'),
        h('th', { scope: 'col' }, 'Allowed models'),
        h('th', { scope: 'col' }, 'Allowed routes'),
        h('th', { scope: 'col' }, 'Req limit'),
        h('th', { scope: 'col' }, ''))),
      tbody));
  }

  function allowSummary(list) {
    if (!list || !list.length) return '—';
    if (list.length <= 2) return list.join(' ');
    return `${list.slice(0, 2).join(' ')} +${list.length - 2}`;
  }

  // ------------------------------------------------------------------ form

  function renderForm(k) {
    const isEdit = Boolean(k);
    const label = h('input', { class: 'input', type: 'text', 'data-field': 'label', value: k?.label ?? '' });
    const models = h('textarea', { class: 'input', 'data-field': 'allow_models', rows: '4', 'aria-label': 'Allowed models, one per line' });
    models.value = listToLines(k?.allow_models);
    const routes = h('textarea', { class: 'input', 'data-field': 'allow_routes', rows: '3', 'aria-label': 'Allowed routes, one per line' });
    routes.value = listToLines(k?.allow_routes);
    const limit = h('input', { class: 'input', type: 'number', min: '0', 'data-field': 'limit_requests', value: k?.limit_requests ?? 0 });
    const note = h('input', { class: 'input', type: 'text', 'data-field': 'note', value: k?.note ?? '' });
    const enabled = h('input', { type: 'checkbox' });
    enabled.checked = k?.enabled ?? true;

    const save = h('button', { class: 'btn btn-primary', type: 'submit' }, isEdit ? 'Save key' : 'Create key');
    const cancel = h('button', { class: 'btn', type: 'button' }, 'Cancel');
    cancel.addEventListener('click', () => { editingId = null; formSlot.replaceChildren(); });

    const form = h('form', { novalidate: true },
      isEdit ? null : h('p', { class: 'hint' },
        'The full key is shown exactly once, right after it is created. Only a hash and a short prefix are stored server-side.'),
      h('div', { class: 'grid-fields' },
        field('Label', label),
        field('Request limit (0 = unlimited)', limit)),
      field('Allowed models (one per line, empty = all models)', models, { hint: 'Empty list means no restriction.' }),
      field('Allowed routes (one per line, empty = all routes)', routes),
      field('Note', note),
      h('div', { class: 'check-row' }, enabled, h('label', {}, 'Key enabled')),
      h('div', { class: 'dialog-actions' }, cancel, save));

    form.addEventListener('submit', async (e) => {
      e.preventDefault();
      clearFieldErrors(form);
      save.disabled = true;
      const payload = {
        label: label.value || 'unnamed key',
        allow_models: linesToList(models.value),
        allow_routes: linesToList(routes.value),
        limit_requests: Number(limit.value) || 0,
        note: note.value || null,
        enabled: enabled.checked,
      };
      try {
        if (isEdit) {
          await api.put(`/api/keys/${encodeURIComponent(k.id)}`, payload);
          ctx.toast.success('Key saved.');
          editingId = null;
          await reload();
        } else {
          // The response carries the plaintext exactly once — hand it to the reveal dialog
          // immediately; it is never stored in panel state.
          const created = await api.post('/api/keys', payload);
          editingId = null;
          formSlot.replaceChildren();
          if (created && typeof created.key === 'string' && created.key) {
            await ctx.secretKey({ key: created.key, label: `Client key ${created.label ?? ''}`.trim() });
          } else {
            ctx.toast.warning('Key created, but the response did not include the plaintext — check the server log.');
          }
          await reload();
        }
      } catch (err) {
        applyFieldErrors(form, err);
        if (!err.details?.length) ctx.toast.error('Could not save the key', err);
        save.disabled = false;
      }
    });

    formSlot.replaceChildren(h('div', { class: 'card' }, h('h2', {}, isEdit ? `Edit key ${k.id}` : 'New client key'), form));
    label.focus();
  }

  async function rotateKey(k) {
    const ok = await ctx.confirm({
      title: `Rotate key “${k.label || k.id}”?`,
      message: 'The current key will stop working immediately. Save the replacement shown once after rotation.',
      confirmLabel: 'Rotate key',
    });
    if (!ok) return;
    try {
      const result = await api.post(`/api/keys/${encodeURIComponent(k.id)}/rotate`, {});
      if (typeof result.key === 'string') {
        await ctx.secretKey({ key: result.key, label: `Replacement key — ${k.label || k.id}` });
      } else {
        ctx.toast.warning('Key rotated, but the response did not include the replacement plaintext.');
      }
      await reload();
    } catch (err) {
      ctx.toast.error('Key rotation failed', err);
    }
  }

  async function deleteKey(k) {
    const ok = await ctx.confirm({
      title: `Delete key “${k.label || k.id}”?`,
      message: 'Every client authenticating with this key will stop working immediately.',
      confirmLabel: 'Delete key',
    });
    if (!ok) return;
    try {
      await api.del(`/api/keys/${encodeURIComponent(k.id)}`);
      ctx.toast.success('Key deleted.');
      if (editingId === k.id) editingId = null;
      await reload();
    } catch (err) {
      ctx.toast.error('Delete failed', err);
    }
  }
}
