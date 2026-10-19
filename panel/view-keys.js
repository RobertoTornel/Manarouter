// view-keys.js — client keys: list, allow-list editing, and the create flow that shows the
// plaintext exactly once (POST /api/keys returns { key: "mr-...", ... } and never again).

import { api } from './api.js';
import {
  h, chip, field, toggle, emptyState, errorBlock, applyFieldErrors, clearFieldErrors,
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
        h('td', {}, rollingCaps(k.limits)),
        h('td', { class: 'actions' }, editBtn, rotateBtn, delBtn)));
    }
    tableWrap.replaceChildren(
      h('p', { class: 'hint' },
        'Edit a key to change its rolling caps. This API does not carry per-key usage, so it is not shown as zero — the Quota page lists the recorded request usage. Enforcement is not confirmed.'),
      h('table', { class: 'tbl' },
        h('thead', {}, h('tr', {},
          h('th', { scope: 'col' }, 'Label'),
          h('th', { scope: 'col' }, 'Prefix'),
          h('th', { scope: 'col' }, 'State'),
          h('th', { scope: 'col' }, 'Allowed models'),
          h('th', { scope: 'col' }, 'Allowed routes'),
          h('th', { scope: 'col' }, 'Req limit'),
          h('th', { scope: 'col' }, 'Rolling caps'),
          h('th', { scope: 'col' }, ''))),
        tbody));
  }

  function allowSummary(list) {
    if (!list || !list.length) return '—';
    if (list.length <= 2) return list.join(' ');
    return `${list.slice(0, 2).join(' ')} +${list.length - 2}`;
  }

  function rollingCaps(limits) {
    const windows = [
      ['5h', limits?.session_5h],
      ['1w', limits?.weekly_1w],
      ['30d', limits?.monthly_30d],
    ];
    const configured = windows.some(([, cap]) => cap && (Number(cap.tokens) > 0 || Number(cap.requests) > 0));
    if (!configured) return 'No rolling caps (uncapped)';
    return h('div', {}, ...windows.map(([name, cap]) => {
      if (!cap) return h('div', {}, `${name}: uncapped`);
      return h('div', {}, `${name}: ${capValue(cap.tokens)} tokens / ${capValue(cap.requests)} requests`);
    }));
  }

  function capValue(value) {
    const n = Number(value);
    return Number.isFinite(n) && n > 0 ? formatCompact(n) : '∞';
  }

  function capValuePayload(form, windowKey) {
    const tokens = form.querySelector(`[data-field="limits.${windowKey}.tokens"]`);
    const requests = form.querySelector(`[data-field="limits.${windowKey}.requests"]`);
    const tokenCount = parseCap(tokens);
    const requestCount = parseCap(requests);
    return tokenCount === null || requestCount === null
      ? null
      : { tokens: tokenCount, requests: requestCount };
  }

  function parseCap(input) {
    const value = input?.value.trim() ?? '';
    const n = Number(value);
    if (!/^\d+$/.test(value) || !Number.isSafeInteger(n)) {
      input?.setAttribute('aria-invalid', 'true');
      return null;
    }
    input.removeAttribute('aria-invalid');
    return n;
  }

  function capInputs(limits = {}) {
    const windows = [
      ['session_5h', '5h'],
      ['weekly_1w', '1w'],
      ['monthly_30d', '30d'],
    ];
    return windows.flatMap(([key, label]) => {
      const cap = limits?.[key] ?? {};
      return [
        field(`${label} token cap (0 = uncapped)`, h('input', {
          class: 'input', type: 'number', min: '0', step: '1', inputmode: 'numeric',
          'data-field': `limits.${key}.tokens`, value: cap.tokens ?? 0,
        })),
        field(`${label} request cap (0 = uncapped)`, h('input', {
          class: 'input', type: 'number', min: '0', step: '1', inputmode: 'numeric',
          'data-field': `limits.${key}.requests`, value: cap.requests ?? 0,
        })),
      ];
    });
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
    const enabled = toggle('Key enabled', { checked: k?.enabled ?? true });

    const save = h('button', { class: 'btn btn-primary', type: 'submit' }, isEdit ? 'Save key' : 'Create key');
    const cancel = h('button', { class: 'btn', type: 'button' }, 'Cancel');
    cancel.addEventListener('click', () => { editingId = null; formSlot.replaceChildren(); });

    const form = h('form', { novalidate: true },
      isEdit ? null : h('p', { class: 'hint' },
        'The full key is shown exactly once, right after it is created. Only a hash and a short prefix are stored server-side.'),
      h('div', { class: 'grid-fields' },
        field('Label', label),
        field('Request limit (0 = unlimited)', limit)),
      h('fieldset', { class: 'card' },
        h('legend', {}, 'Rolling caps'),
        h('p', { class: 'hint' }, 'Enter 0 for uncapped. Token caps apply to prompt + completion tokens over rolling windows.'),
        h('div', { class: 'grid-fields' },
          ...capInputs(k?.limits))),
      field('Allowed models (one per line, empty = all models)', models, { hint: 'Empty list means no restriction.' }),
      field('Allowed routes (one per line, empty = all routes)', routes),
      field('Note', note),
      enabled.row,
      h('div', { class: 'dialog-actions' }, cancel, save));

    form.addEventListener('submit', async (e) => {
      e.preventDefault();
      clearFieldErrors(form);
      save.disabled = true;
      const limits = {
        session_5h: capValuePayload(form, 'session_5h'),
        weekly_1w: capValuePayload(form, 'weekly_1w'),
        monthly_30d: capValuePayload(form, 'monthly_30d'),
      };
      if (Object.values(limits).some((cap) => cap === null)) {
        ctx.toast.warning('Enter whole-number caps of 0 or greater.');
        save.disabled = false;
        return;
      }
      const payload = {
        label: label.value || 'unnamed key',
        allow_models: linesToList(models.value),
        allow_routes: linesToList(routes.value),
        limit_requests: Number(limit.value) || 0,
        note: note.value || null,
        enabled: enabled.input.checked,
        limits,
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
