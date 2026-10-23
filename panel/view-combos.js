// view-combos.js — combo list + editor.
//
// THE core product concept, taught in the UI: a combo's step list is the operator's priority
// order. docs/07-hedged-race.md: it "is never sorted, never re-weighted by the router... it is
// the operator's statement of preference, and the tie-breaker of last resort." Consequences in
// this file: the list renders in array order; reordering happens ONLY through moveStep() (drag
// or buttons), which relocates one element and leaves the rest untouched; nothing here ever
// calls .sort() on steps. Participants and the stable-fallback step are positional references,
// so they are remapped on every move (remapParticipants / remapIndex).
//
// All hedge help text below is a faithful summary of docs/07-hedged-race.md, including the
// counterintuitive part: the winner is the highest-priority participant that replied INSIDE the
// window — a lower-priority model answering faster does not win.

import { api } from './api.js';
import {
  h, icon, chip, field, toggle, emptyState, errorBlock, applyFieldErrors, clearFieldErrors,
} from './dom.js';
import {
  extractList, stepLabel, moveStep, remapParticipants, remapIndex,
  parseWinnerFail, buildWinnerFail, toggleParticipant,
} from './format.js';

const STRATEGIES = [
  ['priority', 'priority', 'Always start at step 1; on failure fall to step 2, and so on. Deterministic and sticky: the cheapest in upstream spend, and it keeps upstream prompt caches warm because the same model sees the conversation every turn.'],
  ['round-robin', 'round-robin', 'Each call starts at the next step (n mod len) and falls forward in priority order from there, wrapping. Rotation chooses the starting point only — after a failure the request still degrades down your order. The common use is rotating across accounts of one provider, whose per-account rate limits are independent.'],
  ['reset-aware-priority', 'reset-aware-priority', 'Priority order with one modification: any step with no available account is demoted below all healthy steps — demoted, not removed. It returns to the front automatically when its earliest known quota reset passes. Your order is preserved within each group.'],
];

const WINNER_METRICS = [
  ['first-response-byte', 'first response byte (TTFB)', 'Race is decided at the first byte of the winning response. The relay cancels local loser work, but providers may continue generating or billing after disconnect. For non-streaming upstreams the first body byte is the complete response, so hedging can cost a full completion per loser.'],
  ['first-complete-token', 'first complete token', 'Race is decided at the first fully decoded token. Fairer to providers that stream immediately and then think, but every loser pays one token.'],
];

const FAIL_MODES = [
  ['pick-highest-passed', 'use highest-priority that passed', 'Implemented. Do not re-dispatch: answer with the highest-priority participant that already passed the eligibility gate in the first competition. Zero added latency; the right mode when a fast, safe answer matters more than spending more upstream requests on recovery.'],
  ['rerun-excluding', 're-run excluding failed (not implemented yet)', 'Not implemented yet: the relay accepts this mode, logs that it is unimplemented, and behaves as pick-highest-passed. It is meant to re-run the whole competition over the remaining participants, excluding every model that already failed, up to max_rounds.'],
  ['pick-highest-passed-different-provider', 'highest passed, different provider (not implemented yet)', 'Not implemented yet: the relay accepts this mode, logs that it is unimplemented, and behaves as pick-highest-passed. It is meant to answer with the highest-priority participant that passed, but only if it comes from a different provider than the one that died.'],
  ['stable-fallback', 'jump to stable step (not implemented yet)', 'Not implemented yet: the relay accepts this mode, logs that it is unimplemented, and behaves as pick-highest-passed. It is meant to skip the race entirely and go to the chosen stable step.'],
];

export async function renderCombos(mount, ctx, selectedId) {
  let combos = [];
  let providers = [];

  const listEl = h('ul', { class: 'combo-list' });
  const newBtn = h('button', { class: 'btn btn-primary', type: 'button' }, 'New combo');
  const count = h('span', { class: 'count' });
  const formSlot = h('div');
  const left = h('div', { class: 'card' }, h('h2', {}, 'Combos'), listEl);
  const right = h('div', { class: 'card' });
  mount.replaceChildren(
    h('div', { class: 'toolbar' }, newBtn, h('span', { class: 'spacer' }), count),
    formSlot,
    h('div', { class: 'combo-layout' }, left, right),
  );

  newBtn.addEventListener('click', () => renderEditor(null, formSlot));

  try {
    // list_combos returns { combos: config.combos }; each ComboStep is only provider/model/policy
    // data. Reasoning capabilities belong to the provider's ModelEntry, so join them below rather
    // than pretending GET /api/combos contains compiled snapshot levels.
    combos = extractList(await api.get('/api/combos'), 'combos');
  } catch (err) {
    right.replaceChildren(errorBlock(err));
    return;
  }
  try { providers = await ctx.loadProviders(); } catch { /* combos remain usable without metadata */ }

  renderList();
  count.textContent = `${combos.length} combo${combos.length === 1 ? '' : 's'}`;

  if (selectedId === '_new') {
    renderEditor(null, formSlot);
    right.replaceChildren(emptyState('Create a combo.', 'Add a name and one or more steps to get started.'));
  } else if (selectedId) {
    const found = combos.find((c) => c.id === selectedId);
    if (found) renderSummary(right, found);
    else right.replaceChildren(emptyState('Combo not found.', 'It may have been deleted.'));
  } else {
    right.replaceChildren(combos.length
      ? emptyState('Select a combo on the left.', 'Or create a new one.')
      : emptyState('No combos yet.',
        'A combo is a named model: clients call it like any model, and it resolves to an ordered list of steps.'));
  }

  // ------------------------------------------------------------------ list

  function renderList() {
    listEl.replaceChildren();
    for (const c of combos) {
      const offeredNames = comboLevels(c.steps ?? [], providers).map((level) => `${c.name || c.id}-${level}`);
      const btn = h('button', { type: 'button', 'aria-current': c.id === selectedId ? 'true' : null },
        h('div', { class: 'cname' }, c.name || c.id),
        h('div', { class: 'cmeta' },
          `${(c.steps ?? []).length} step${(c.steps ?? []).length === 1 ? '' : 's'}`,
          chip(c.strategy ?? 'priority'),
          ...offeredNames.map((name) => chip(name, 'chip-neutral', 'Level-qualified combo name')),
          c.hedge?.enabled ? chip('hedged', 'chip-pink', 'hedged race enabled') : null,
          c.enabled === false ? chip('off') : null));
      btn.addEventListener('click', () => ctx.navigate(`#/combos/${encodeURIComponent(c.id)}`));
      listEl.append(h('li', {}, btn));
    }
  }

  // ------------------------------------------------------------------ detail drawer and summary

  function closeDrawer() {
    mount.querySelector('.drawer')?.remove();
  }

  function drawerShell(title) {
    closeDrawer();
    const head = h('div', { class: 'drawer-head' },
      h('h2', {}, title),
      h('span', { class: 'spacer' }));
    const body = h('div', { class: 'drawer-body' });
    const drawer = h('div', { class: 'drawer', role: 'region', 'aria-label': 'Combo detail' }, head, body);
    drawer.addEventListener('keydown', (e) => { if (e.key === 'Escape') closeDrawer(); });
    mount.append(drawer);
    return { head, body };
  }

  function openEditorDrawer(saved) {
    const { head, body } = drawerShell(saved ? `Edit combo ${saved.name || saved.id}` : 'New combo');
    head.append(h('button', {
      class: 'icon-btn', type: 'button', 'aria-label': 'Close combo editor',
      onclick: closeDrawer,
    }, icon('x')));
    renderEditor(saved, body);
  }

  function renderSummary(target, combo) {
    const levels = comboLevels(combo.steps ?? [], providers);
    const editBtn = h('button', { class: 'btn btn-primary', type: 'button' }, 'Edit combo');
    editBtn.addEventListener('click', () => openEditorDrawer(combo));
    target.replaceChildren(
      h('h2', {}, combo.label || combo.name || combo.id),
      h('p', { class: 'hint mono' }, `id: ${combo.id}`),
      h('p', { class: 'hint' }, combo.name || combo.id),
      h('div', { class: 'cmeta' },
        chip(`strategy: ${combo.strategy ?? 'priority'}`),
        combo.hedge?.enabled ? chip('hedged', 'chip-pink', 'hedged race enabled') : null,
        combo.enabled === false ? chip('off') : chip('enabled', 'chip-green')),
      h('section', {},
        h('h3', {}, 'Level-qualified names'),
        levels.length
          ? h('div', { class: 'cmeta' }, ...levels.map((level) => chip(`${combo.name || combo.id}-${level}`, 'chip-neutral', `Available level suffix: ${level}`)))
          : h('p', { class: 'hint' }, 'No reasoning_levels are declared on these model entries. Derived runtime levels are not included in the combo config response.')),
      h('section', {},
        h('h3', {}, 'Steps — priority order'),
        h('ol', { class: 'steps' }, ...(combo.steps ?? []).map((step, i) => {
          const model = stepModel(step, providers);
          const levelsForStep = Array.isArray(model?.reasoning_levels) ? model.reasoning_levels : [];
          const defaultLevel = model?.default_reasoning_level;
          const offersLevel = defaultLevel && levelsForStep.includes(defaultLevel);
          const canNameComboTier = !endsInLevelName(combo.name || combo.id);
          return h('li', { class: 'step-row' },
            h('span', { class: 'step-pos' }, String(i + 1)),
            h('span', { class: 'step-ref mono' }, stepLabel(step)),
            ...levelsForStep.map((level) => chip(String(level), 'chip-neutral', 'Reasoning level offered by this model')),
            defaultLevel && !offersLevel ? chip(`default ${defaultLevel}`, 'chip-pink', 'Model-specific default reasoning level') : null,
            step.enabled === false ? chip('off') : null);
        }))),
      h('div', { class: 'dialog-actions' }, editBtn),
    );
  }

  // ------------------------------------------------------------------ editor

  function renderEditor(saved, target = right) {
    const isNew = saved === null;
    // Draft is a deep copy: edits are local until Save; Revert re-reads from `combos`.
    const draft = isNew ? newDraft() : JSON.parse(JSON.stringify(saved));
    // Normalise the hedge object defensively: a combo saved without one must not crash the
    // editor, and the defaults here mirror the backend's (count 3, window 800 ms, TTFB metric).
    draft.hedge = {
      enabled: draft.hedge?.enabled ?? false,
      sticky_success_window_secs: draft.sticky_success_window_secs ?? 1800,
      participants: draft.hedge?.participants ?? [],
      count: draft.hedge?.count ?? 3,
      window_ms: draft.hedge?.window_ms ?? 800,
      winner_metric: draft.hedge?.winner_metric ?? 'first-response-byte',
      on_winner_fail: draft.hedge?.on_winner_fail ?? { 'pick-highest-passed': null },
      cancel_losers: draft.hedge?.cancel_losers ?? true,
      loser_grace_ms: draft.hedge?.loser_grace_ms ?? 0,
    };
    draft.wf = parseWinnerFail(draft.hedge.on_winner_fail); // working form of the tagged enum

    const title = h('h2', { style: 'text-transform:none;letter-spacing:0;font-size:14px;' },
      isNew ? 'New combo' : `Editing ${saved.name || saved.id}`);
    const nameIn = h('input', { class: 'input mono', type: 'text', 'data-field': 'name', value: draft.name ?? '', placeholder: 'callable model name, e.g. Brains' });
    const labelIn = h('input', { class: 'input', type: 'text', 'data-field': 'label', value: draft.label ?? '' });

    const stratSel = h('select', { class: 'input', 'data-field': 'strategy' });
    for (const [v, lbl] of STRATEGIES) stratSel.append(h('option', { value: v, selected: (draft.strategy ?? 'priority') === v }, lbl));
    const stratHint = h('p', { class: 'hint' });
    stratSel.addEventListener('change', () => { updateStrategyHint(); });

    // --- steps ---
    const stepsOl = h('ol', { class: 'steps' });
    const stepCallout = h('div', { class: 'callout' },
      h('strong', {}, 'This order is your priority order. '),
      'The panel and the router never sort, re-weight or second-guess it: step 1 is tried first, ',
      'and in a hedged race the winner is the highest-priority participant that replied within the window. ',
      'Reorder by dragging a row (or the arrow buttons) — the number on the left is the rank.');

    const addProv = h('select', { class: 'input', 'aria-label': 'Provider for new step' });
    const addModel = h('input', { class: 'input mono', type: 'text', 'aria-label': 'Model for new step', placeholder: 'model id' });
    const addDatalist = h('datalist', { id: 'combo-model-datalist' });
    addModel.setAttribute('list', 'combo-model-datalist');
    const addNote = h('input', { class: 'input', type: 'text', 'aria-label': 'Note for new step (optional)', placeholder: 'note (optional)' });
    const addBtn = h('button', { class: 'btn', type: 'button' }, 'Add step');
    const addRow = h('div', { style: 'display:flex;gap:6px;flex-wrap:wrap;align-items:center;margin-top:8px;' },
      addProv, addModel, addNote, addBtn, addDatalist);

    ctx.loadProviders().then((providers) => {
      addProv.replaceChildren(...(providers ?? []).map((p) => h('option', { value: p.id }, p.id)));
      addProv.prepend(h('option', { value: '', selected: true }, 'provider…'));
    }).catch(() => {
      addProv.replaceChildren(h('option', { value: '' }, 'providers unavailable'));
    });
    addProv.addEventListener('change', () => fillDatalist());
    function fillDatalist() {
      const p = (ctx.state.providers ?? []).find((x) => x.id === addProv.value);
      addDatalist.replaceChildren(...(p?.models ?? []).map((m) => h('option', { value: m.id })));
    }

    addBtn.addEventListener('click', () => {
      const provider = addProv.value;
      const model = addModel.value.trim();
      const problems = [];
      if (!provider) problems.push('pick a provider for the step');
      if (!model) problems.push('enter a model id for the step');
      if (problems.length) { ctx.toast.warning(problems.join(' — ')); return; }
      // Appends at the END: a new step starts at the lowest priority. It is never inserted
      // "where it fits" — only the operator decides placement.
      draft.steps.push({ provider, model, enabled: true, note: addNote.value.trim() || undefined });
      addModel.value = ''; addNote.value = '';
      renderSteps(); renderStableSelect(); renderParticipantSummary();
    });

    // --- hedge ---
    const hedgeToggle = toggle('Enable the hedged race for this combo', { checked: Boolean(draft.hedge?.enabled) });
    const hedgeOn = hedgeToggle.input;
    const stickyWindowInput = h('input', { class: 'input', type: 'number', min: '0', max: '604800', value: draft.sticky_success_window_secs });
    const stickyWindowBox = field('Sticky success window (seconds)', stickyWindowInput, { hint: 'Priority combos only; enabled with hedge. Default 1800s (30 minutes). Combo-wide across callers; inactivity refreshes after a successful response. Set 0 to disable.' });
    const hedgeCount = h('input', { class: 'input', type: 'number', min: '2', max: '255', 'data-field': 'count', value: draft.hedge?.count ?? 3 });
    const hedgeWindow = h('input', { class: 'input', type: 'number', min: '0', 'data-field': 'window_ms', value: draft.hedge?.window_ms ?? 800 });
    const metricSel = h('select', { class: 'input', 'data-field': 'winner_metric' });
    for (const [v, lbl] of WINNER_METRICS) metricSel.append(h('option', { value: v, selected: (draft.hedge?.winner_metric ?? 'first-response-byte') === v }, lbl));
    const metricHint = h('p', { class: 'hint' });
    metricSel.addEventListener('change', () => { updateMetricHint(); });

    const failSel = h('select', { class: 'input', 'data-field': 'on_winner_fail' });
    for (const [v, lbl] of FAIL_MODES) failSel.append(h('option', { value: v, selected: draft.wf.mode === v }, lbl));
    const failHint = h('p', { class: 'hint' });
    const maxRounds = h('input', { class: 'input', type: 'number', min: '1', max: '255', 'data-field': 'max_rounds', value: draft.wf.maxRounds ?? 2 });
    const stableSel = h('select', { class: 'input', 'data-field': 'step' });
    const maxRoundsField = field('Max rounds', maxRounds, { hint: 'Bound for re-run excluding.' });
    const stableField = field('Stable step', stableSel, { hint: 'The on-fail model; need not be a race participant.' });
    failSel.addEventListener('change', () => { draft.wf.mode = failSel.value; renderFailExtras(); updateFailHint(); });
    const cancelLosersToggle = toggle('Cancel losing requests once the winner is chosen', { checked: draft.hedge?.cancel_losers ?? true });
    const cancelLosers = cancelLosersToggle.input;
    const loserGrace = h('input', { class: 'input', type: 'number', min: '0', 'data-field': 'loser_grace_ms', value: draft.hedge?.loser_grace_ms ?? 0 });
    const participantSummary = h('p', { class: 'hint' });

    function renderStableSelect() {
      stableSel.replaceChildren();
      draft.steps.forEach((s, i) => stableSel.append(h('option', { value: String(i), selected: draft.wf.step === i }, `#${i + 1} ${stepLabel(s)}`)));
      stableSel.value = String(draft.wf.step ?? 0);
    }
    function renderFailExtras() {
      maxRoundsField.style.display = draft.wf.mode === 'rerun-excluding' ? '' : 'none';
      stableField.style.display = draft.wf.mode === 'stable-fallback' ? '' : 'none';
    }
    function renderParticipantSummary() {
      const n = (draft.hedge?.participants ?? []).length;
      participantSummary.textContent = n
        ? `${n} step${n === 1 ? '' : 's'} ticked as race participants. Untick all to race the first “count” enabled steps instead.`
        : 'No explicit participants: the first “count” enabled steps enter the race.';
    }

    const hedgeBox = h('fieldset', { class: 'hedge' },
      h('legend', {}, 'Hedged race and sticky success'),
      stickyWindowBox,
      hedgeToggle.row,
      h('p', { class: 'hint', style: 'margin-bottom:8px;' },
        'The window is an eligibility gate, not a stopwatch race: the winner is the highest-priority participant that produced a valid reply within the window. A lower-priority model answering faster does not win; if nobody answers in time, nothing is re-spent.'),
      h('div', { class: 'grid-fields' },
        field('Count', hedgeCount, { hint: 'Participants when none are ticked per-step.' }),
        field('Window (ms)', hedgeWindow, { hint: 'How long higher-priority steps are protected.' })),
      field('Winner metric', metricSel, { hint: null }),
      metricHint,
      field('On winner fail', failSel),
      failHint,
      h('div', { class: 'grid-fields' }, maxRoundsField, stableField),
      cancelLosersToggle.row,
      h('div', { class: 'grid-fields' },
        field('Loser grace (ms)', loserGrace, { hint: '0 = cancel immediately.' })),
      participantSummary,
    );
    hedgeOn.addEventListener('change', () => { hedgeBox.style.opacity = hedgeOn.checked ? '1' : '0.55'; stickyWindowBox.style.opacity = hedgeOn.checked ? '1' : '0.55'; });
    hedgeBox.style.opacity = hedgeOn.checked ? '1' : '0.55';
    renderStableSelect(); renderFailExtras(); renderParticipantSummary();
    updateStrategyHint(); updateMetricHint(); updateFailHint();

    // --- delivery ---
    const midSel = h('select', { class: 'input', 'data-field': 'midstream_policy' },
      h('option', { value: 'restart', selected: (draft.midstream_policy ?? 'restart') === 'restart' }, 'restart'),
      h('option', { value: 'terminate', selected: draft.midstream_policy === 'terminate' }, 'terminate'));
    const maxRetries = h('input', { class: 'input', type: 'number', min: '0', 'data-field': 'max_retries', value: draft.max_retries ?? 1 });
    const retryDelay = h('input', { class: 'input', type: 'number', min: '0', 'data-field': 'retry_delay_ms', value: draft.retry_delay_ms ?? 0 });
    const affinityToggle = toggle('Merge identical consecutive messages when translating between dialects', { checked: Boolean(draft.affinity) });
    const affinity = affinityToggle.input;
    const comboEnabledToggle = toggle('Combo enabled', { checked: draft.enabled ?? true });
    const comboEnabled = comboEnabledToggle.input;

    // --- actions ---
    const saveBtn = h('button', { class: 'btn btn-primary', type: 'button' }, isNew ? 'Create combo' : 'Save combo');
    const delBtn = h('button', { class: 'btn btn-danger', type: 'button', disabled: isNew }, 'Delete combo');
    const revertBtn = h('button', { class: 'btn', type: 'button', disabled: isNew }, 'Revert');

    saveBtn.addEventListener('click', async () => {
      clearFieldErrors(editor);
      // Keep only structural checks here. Model ids are free text on purpose: the server resolves
      // provider/model names and its field errors explain typos better than stale client metadata.
      const problems = [];
      if (!draft.name?.trim()) problems.push({ field: 'name', message: 'the callable name is required — clients will pass it as the model' });
      if (!draft.steps.length) problems.push({ field: 'steps', message: 'a combo needs at least one step' });
      if (draft.steps.some((s) => !s.provider || !String(s.model ?? '').trim())) problems.push({ field: 'steps', message: 'every step needs a provider and a model id' });
      if (draft.wf.mode === 'stable-fallback' && !(draft.wf.step >= 0 && draft.wf.step < draft.steps.length)) {
        problems.push({ field: 'step', message: 'the stable step must point at one of this combo\u2019s steps' });
      }
      if (problems.length) { applyFieldErrors(editor, { message: 'Fix these before saving.', details: problems }); return; }

      saveBtn.disabled = true;
      saveBtn.textContent = 'Saving…';
      // Rebuild the tagged enum in its wire shape; participants are step indices.
      const payload = {
        ...draft,
        name: draft.name.trim(),
        sticky_success_window_secs: Number(stickyWindowInput.value) || 0,
        steps: draft.steps.map((s) => ({ ...s, note: s.note || undefined })),
        hedge: {
          enabled: hedgeOn.checked,
          participants: draft.hedge.participants ?? [],
          count: Number(hedgeCount.value) || 3,
          window_ms: Number(hedgeWindow.value) || 0,
          winner_metric: metricSel.value,
          on_winner_fail: buildWinnerFail(draft.wf.mode, { maxRounds: Number(maxRounds.value), step: Number(stableSel.value) }),
          cancel_losers: cancelLosers.checked,
          loser_grace_ms: Number(loserGrace.value) || 0,
        },
        strategy: stratSel.value,
        midstream_policy: midSel.value,
        max_retries: Number(maxRetries.value) || 0,
        retry_delay_ms: Number(retryDelay.value) || 0,
        affinity: affinity.checked,
        enabled: comboEnabled.checked,
      };
      delete payload.wf; // panel-internal working state, never sent

      try {
        if (isNew) {
          const created = await api.post('/api/combos', payload);
          ctx.toast.success(`Combo ${created?.id ?? payload.name} created.`);
          closeDrawer();
          ctx.navigate(`#/combos/${encodeURIComponent(created?.id ?? payload.name)}`);
        } else {
          await api.put(`/api/combos/${encodeURIComponent(saved.id)}`, payload);
          ctx.toast.success(`Combo ${payload.name} saved.`);
          closeDrawer();
          ctx.navigate('#/combos');
        }
      } catch (err) {
        applyFieldErrors(editor, err); // every backend problem, listed and pinned to fields
        if (!err.details?.length) ctx.toast.error('Could not save the combo', err);
        saveBtn.disabled = false;
        saveBtn.textContent = isNew ? 'Create combo' : 'Save combo';
      }
    });

    delBtn.addEventListener('click', async () => {
      const ok = await ctx.confirm({
        title: `Delete combo “${saved.name || saved.id}”?`,
        message: 'Clients calling this model name will get “model not found”.',
        confirmLabel: 'Delete combo',
      });
      if (!ok) return;
      try {
        await api.del(`/api/combos/${encodeURIComponent(saved.id)}`);
        ctx.toast.success('Combo deleted.');
        ctx.navigate('#/combos');
      } catch (err) {
        ctx.toast.error('Delete failed', err);
      }
    });

    revertBtn.addEventListener('click', () => renderEditor(saved));

    const editor = h('form', { class: 'combo-editor', onsubmit: (e) => e.preventDefault() },
      title,
      isNew ? null : h('p', { class: 'hint mono', style: 'margin-top:-4px;' }, `id: ${saved.id}`),
      h('div', { class: 'grid-fields' },
        field('Name — the model clients call', nameIn, { hint: 'A combo is a named model: there is no special endpoint or syntax.' }),
        field('Label (display only)', labelIn)),
      field('Strategy', stratSel),
      stratHint,
      h('fieldset', { class: 'plain' },
        h('legend', {}, 'Steps — priority order, never sorted'),
        stepCallout,
        stepsOl,
        addRow),
      hedgeBox,
      h('fieldset', { class: 'plain' },
        h('legend', {}, 'Delivery'),
        field('Midstream policy', midSel, { hint: 'restart: if the winner disconnects mid-stream, restart the request from the beginning at an SSE event boundary (the client sees a restart marker). terminate: end the request. Both refuse to splice mid tool_calls.' }),
        h('div', { class: 'grid-fields' },
          field('Max retries', maxRetries, { hint: 'Retries of a failing step before falling to the next one.' }),
          field('Retry delay (ms)', retryDelay)),
        affinityToggle.row,
        comboEnabledToggle.row),
      h('div', { class: 'dialog-actions' }, revertBtn, delBtn, saveBtn),
    );

    nameIn.addEventListener('input', () => { draft.name = nameIn.value; });
    labelIn.addEventListener('input', () => { draft.label = labelIn.value; });

    target.replaceChildren(editor);
    renderSteps();
    if (isNew) nameIn.focus();

    // ------------------------------------------------------------------
    // Steps rendering & reorder mechanics

    function renderSteps() {
      stepsOl.replaceChildren();
      if (!draft.steps.length) {
        stepsOl.append(h('li', {}, emptyState('No steps yet.', 'Add the first step below — it becomes priority #1.')));
        return;
      }
      draft.steps.forEach((step, i) => {
        const pos = h('span', { class: 'step-pos' }, String(i + 1));
        const ref = h('span', { class: 'step-ref mono' }, stepLabel(step));

        const race = h('input', {
          type: 'checkbox', 'aria-label': `Step ${i + 1} races (hedged participant)`,
        });
        race.checked = (draft.hedge.participants ?? []).includes(i);
        race.addEventListener('change', () => {
          draft.hedge.participants = toggleParticipant(draft.hedge.participants, i);
          renderParticipantSummary();
        });

        const on = h('input', { type: 'checkbox', 'aria-label': `Step ${i + 1} enabled` });
        on.checked = step.enabled !== false;
        on.addEventListener('change', () => { draft.steps[i].enabled = on.checked; });

        const up = h('button', { class: 'icon-btn', type: 'button', 'aria-label': `Move step ${i + 1} up` }, icon('up'));
        const down = h('button', { class: 'icon-btn', type: 'button', 'aria-label': `Move step ${i + 1} down` }, icon('down'));
        up.disabled = i === 0;
        down.disabled = i === draft.steps.length - 1;
        up.addEventListener('click', () => doMove(i, i - 1));
        down.addEventListener('click', () => doMove(i, i + 1));

        const edit = h('button', { class: 'icon-btn', type: 'button', 'aria-label': `Edit step ${i + 1}` }, icon('pencil'));
        edit.addEventListener('click', () => renderStepEdit(i));

        const del = h('button', { class: 'icon-btn danger', type: 'button', 'aria-label': `Remove step ${i + 1}` }, icon('x'));
        del.addEventListener('click', () => {
          draft.steps.splice(i, 1);
          // Removing a step shifts every later index: remap positional references.
          draft.hedge.participants = (draft.hedge.participants ?? [])
            .filter((p) => p !== i).map((p) => (p > i ? p - 1 : p));
          if (draft.wf.step === i) draft.wf.step = 0;
          else if (draft.wf.step > i) draft.wf.step -= 1;
          renderSteps(); renderStableSelect(); renderParticipantSummary();
        });

        const li = h('li', { class: 'step-row', draggable: 'true', 'data-idx': String(i), tabindex: '-1' },
          pos,
          h('span', { class: 'icon-btn', 'aria-hidden': 'true', title: 'Drag to reorder (or use the arrows)' }, icon('grip')),
          ref,
          h('label', { class: 'step-flag' }, race, 'race'),
          h('label', { class: 'step-flag' }, on, 'on'),
          up, down, edit, del);

        li.addEventListener('dragstart', (e) => {
          e.dataTransfer.setData('text/plain', String(i));
          e.dataTransfer.effectAllowed = 'move';
          li.classList.add('dragging');
        });
        li.addEventListener('dragend', () => li.classList.remove('dragging'));
        li.addEventListener('dragover', (e) => { e.preventDefault(); li.classList.add('drag-over'); });
        li.addEventListener('dragleave', () => li.classList.remove('drag-over'));
        li.addEventListener('drop', (e) => {
          e.preventDefault();
          li.classList.remove('drag-over');
          const raw = e.dataTransfer.getData('text/plain');
          if (!/^\d+$/.test(raw)) return;
          const from = Number(raw);
          if (Number.isInteger(from)) doMove(from, i);
        });

        stepsOl.append(li);
      });
    }

    function renderStepEdit(i) {
      const li = stepsOl.querySelector(`li[data-idx="${i}"]`);
      if (!li) return;
      const step = draft.steps[i];
      const provSel = h('select', { class: 'input', 'aria-label': `Provider for step ${i + 1}` });
      ctx.loadProviders().then((providers) => {
        provSel.replaceChildren(...(providers ?? []).map((p) => h('option', { value: p.id, selected: p.id === step.provider }, p.id)));
      }).catch(() => {});
      const modelIn = h('input', { class: 'input mono', type: 'text', value: step.model, 'aria-label': `Model for step ${i + 1}` });
      const noteIn = h('input', { class: 'input', type: 'text', value: step.note ?? '', placeholder: 'note (optional)', 'aria-label': `Note for step ${i + 1}` });
      const done = h('button', { class: 'btn btn-sm btn-primary', type: 'button' }, 'Done');
      done.addEventListener('click', () => {
        draft.steps[i] = { ...step, provider: provSel.value || step.provider, model: modelIn.value.trim() || step.model, note: noteIn.value.trim() || undefined };
        renderSteps(); renderStableSelect();
      });
      li.replaceChildren(h('div', { class: 'step-edit' },
        h('span', { class: 'step-pos' }, String(i + 1)), provSel, modelIn, noteIn, done));
      modelIn.focus();
    }

    // The ONLY way order changes. After the re-render the moved row is focused so keyboard
    // users keep their place in the list.
    function doMove(from, to) {
      if (!Number.isInteger(from) || from < 0 || from >= draft.steps.length) return;
      if (to < 0 || to >= draft.steps.length || from === to) return;
      draft.steps = moveStep(draft.steps, from, to);
      // Positional references move with the rows — forgetting this would silently re-point
      // the hedged race at the wrong models after a reorder.
      draft.hedge.participants = remapParticipants(draft.hedge.participants, from, to);
      draft.wf.step = remapIndex(draft.wf.step, from, to);
      renderSteps(); renderStableSelect(); renderParticipantSummary();
      stepsOl.querySelector(`li[data-idx="${to}"]`)?.focus();
    }

    function updateStrategyHint() { stratHint.textContent = STRATEGIES.find(([v]) => v === stratSel.value)?.[2] ?? ''; }
    function updateMetricHint() { metricHint.textContent = WINNER_METRICS.find(([v]) => v === metricSel.value)?.[2] ?? ''; }
    function updateFailHint() { failHint.textContent = FAIL_MODES.find(([v]) => v === draft.wf.mode)?.[2] ?? ''; }
  }

  function newDraft() {
    return {
      name: '',
      label: '',
      steps: [],
      strategy: 'priority',
      hedge: {
        enabled: false,
        participants: [],
        count: 3,
        window_ms: 800,
        winner_metric: 'first-response-byte',
        on_winner_fail: { 'pick-highest-passed': null },
        cancel_losers: true,
        loser_grace_ms: 0,
      },
      midstream_policy: 'restart',
      max_retries: 1,
      retry_delay_ms: 0,
      affinity: true,
      enabled: true,
    };
  }
}
