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
  stepModel, stepProvider, modelLevels, providerLevels, probeTarget,
  stepAccountSelection, describeAccountSelection, selectorNames,
} from './format.js';
import { comboActivityIndex, comboRoutePreview } from './combo-live.js';
import { createSearchSelect } from './search-select.js';

// A check's result outlives the selection that produced it: an operator who checks three combos and
// then scrolls back should still see what came back. Keyed by combo id, then step index. Module
// scope, because the view can render a selected combo before its own body reaches a later `const`.
const checked = new Map();

// List preferences live in this browser, like the provider list: the combo route is global config,
// but which rows an operator has pinned and the order they dragged them into are viewing choices,
// not part of the routing document. Saving them server-side would turn one person's tidy list into
// everyone's order.
const COMBO_SORT_KEY = 'manarouter.combos.sort';
const COMBO_PINS_KEY = 'manarouter.combos.pins';
const COMBO_ORDER_KEY = 'manarouter.combos.order';
const COMBO_SORTS = new Set(['manual', 'name', 'steps', 'recent']);
const COMBO_SORT_OPTIONS = [
  ['manual', 'Manual'],
  ['name', 'Name'],
  ['steps', 'Most steps'],
  ['recent', 'Recently used'],
];

function storedIdList(key) {
  try {
    const parsed = JSON.parse(localStorage.getItem(key) ?? '[]');
    return Array.isArray(parsed) ? parsed.filter((id) => typeof id === 'string' && id) : [];
  } catch {
    return [];
  }
}

function storeIdList(key, ids) {
  try {
    localStorage.setItem(key, JSON.stringify(ids));
    return true;
  } catch {
    return false;
  }
}

function storedComboSort() {
  try {
    const value = localStorage.getItem(COMBO_SORT_KEY);
    return COMBO_SORTS.has(value) ? value : 'manual';
  } catch {
    return 'manual';
  }
}

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
  ['rerun-excluding', 're-run excluding failed models', 'Implemented. Re-run the whole competition over the remaining participants, excluding every model that already failed, up to max_rounds. Costs extra upstream requests on recovery; max_rounds bounds it so one systemic outage cannot turn one request into unbounded spend.'],
  ['pick-highest-passed-different-provider', 'highest passed, different provider', 'Implemented. Answer with the highest-priority participant that passed, but only if it comes from a different provider than the one that died. Survives a whole-provider outage or quota reset that a same-provider fallback cannot.'],
  ['stable-fallback', 'jump to stable step', 'Implemented. On winner failure, skip the race and go straight to the chosen stable step — the operator-trusted model kept available for exactly this case.'],
];

export async function renderCombos(mount, ctx, selectedId) {
  let combos = [];
  let providers = [];
  let pinned = new Set(storedIdList(COMBO_PINS_KEY));
  let manualOrder = storedIdList(COMBO_ORDER_KEY);
  let sortMode = storedComboSort();
  let activityEntries = [];
  let activityByCombo = new Map();
  let draggedId = null;
  let dragSuppressed = false;
  const tileParts = new Map();

  const listEl = h('ul', { class: 'combo-list' });
  const newBtn = h('button', { class: 'btn btn-primary', type: 'button' }, 'New combo');
  const count = h('span', { class: 'count' });
  const sortSel = h('select', { class: 'input combo-sort', 'aria-label': 'Sort combos' },
    ...COMBO_SORT_OPTIONS.map(([value, label]) => h('option', { value }, label)));
  sortSel.value = sortMode;
  const formSlot = h('div');
  const left = h('div', { class: 'card combo-list-card' },
    h('div', { class: 'combo-list-head' },
      h('div', { class: 'combo-list-title' }, h('h2', {}, 'Combos'), count),
      h('label', { class: 'combo-sort-field' }, h('span', {}, 'Sort'), sortSel)),
    listEl);
  const right = h('div', { class: 'card combo-detail-card' });
  // The editor currently mounted in the drawer, if any. It is cleared on close so an old editor's
  // dirty flag can never keep prompting after its DOM is gone. Declared before any immediate
  // renderEditor() call below, including the "#/combos/_new" route.
  let activeEditor = null;
  let closingEditor = false;
  mount.replaceChildren(
    h('div', { class: 'toolbar' }, newBtn, h('span', { class: 'spacer' })),
    formSlot,
    h('div', { class: 'combo-layout' }, left, right),
  );

  // One creation affordance, in context: with nothing selected the empty detail panel owns New;
  // once a combo is open the toolbar owns it. Visibility (not display) keeps the toolbar height,
  // so choosing a row does not shift the whole grid down by 44px. This is applied after the
  // catalogue resolves, because the first combo is selected automatically.
  newBtn.addEventListener('click', () => { void startNewEditor(); });
  sortSel.addEventListener('change', () => switchSort(sortSel.value));

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
  seedComboPreferences();
  try { setActivity(extractList(await api.get('/api/activity'), 'entries')); } catch { setActivity([]); }

  // A combo is called by its `name` (e.g. "deepseek"), but its stored id is a UUID. Links from the
  // sidebar and the overview use the name an operator recognises, so accept either rather than
  // sending a real combo to "Combo not found". Match the id first: a name that happens to look
  // like another combo's id should never win over the actual id.
  if (selectedId && selectedId !== '_new') {
    const selected = combos.find((c) => c.id === selectedId)
      ?? combos.find((c) => c.name === selectedId)
      ?? combos.find((c) => c.label === selectedId);
    selectedId = selected?.id ?? selectedId;
  }
  // A master-detail page opens on its first row. Leaving the detail pane empty on a page that
  // already lists four routes reads as unfinished, and forces a click before the page says
  // anything useful. A truly empty catalogue still gets the empty state below.
  if (!selectedId && combos.length) selectedId = orderedCombos()[0]?.id ?? null;
  newBtn.style.visibility = selectedId && selectedId !== '_new' ? '' : 'hidden';

  renderList();
  count.textContent = `${combos.length} combo${combos.length === 1 ? '' : 's'}`;

  if (selectedId === '_new') {
    renderEditor(null, formSlot);
    right.classList.add('is-empty');
    right.replaceChildren(h('div', { class: 'combo-empty combo-empty-new' },
      h('p', { class: 'combo-empty-kicker' }, 'New combo'),
      h('h2', {}, 'Build a route on the right'),
      h('p', {}, 'Give it a name, then add model steps in the order a request should try them.'),
      h('button', { class: 'btn', type: 'button', onclick: () => { void startNewEditor(); } }, 'New combo')));
  } else if (selectedId) {
    const found = combos.find((c) => c.id === selectedId);
    if (found) renderSummary(right, found);
    else {
      right.classList.add('is-empty');
      right.replaceChildren(emptyState('Combo not found.', 'It may have been deleted.'));
    }
  } else {
    renderEmptyDetail();
  }

  const activityTimer = setInterval(() => { void refreshActivity(); }, 5000);
  async function refreshActivity() {
    if (!mount.isConnected) { clearInterval(activityTimer); return; }
    if (document.hidden) return;
    try {
      const payload = await api.get('/api/activity');
      if (!mount.isConnected) { clearInterval(activityTimer); return; }
      setActivity(extractList(payload, 'entries'));
    } catch {
      // Activity is an enhancement; a gap in it must never disturb the list an operator is editing.
    }
  }

  // ------------------------------------------------------------------ list

  function seedComboPreferences() {
    const ids = new Set(combos.map((c) => c.id));
    pinned = new Set([...pinned].filter((id) => ids.has(id)));
    const next = [];
    const seen = new Set();
    for (const id of manualOrder) {
      if (ids.has(id) && !seen.has(id)) { seen.add(id); next.push(id); }
    }
    for (const combo of combos) {
      if (!seen.has(combo.id)) { seen.add(combo.id); next.push(combo.id); }
    }
    manualOrder = next;
    storeIdList(COMBO_PINS_KEY, [...pinned]);
    storeIdList(COMBO_ORDER_KEY, manualOrder);
  }

  function setActivity(entries) {
    activityEntries = entries;
    activityByCombo = comboActivityIndex(combos, entries);
    for (const [id, parts] of tileParts) {
      const combo = combos.find((c) => c.id === id);
      if (combo) paintTileState(parts, combo);
    }
  }

  /** The idle route: the configured first step, or the placeholder the list always showed. */
  function comboConfiguredRoute(combo) {
    const steps = Array.isArray(combo.steps) ? combo.steps : [];
    return steps[0] ? stepLabel(steps[0]) : 'No steps yet';
  }

  function paintTileState(parts, combo) {
    const state = activityByCombo.get(combo.id);
    parts.state.classList.toggle('is-live', Boolean(state?.live));
    parts.state.replaceChildren(
      h('span', { class: 'combo-live-dot', 'aria-hidden': 'true' }),
      document.createTextNode(state?.live ? 'live' : 'idle'));
    parts.state.title = state?.live
      ? 'A request is running through this combo now.'
      : state?.last
        ? `Last used ${agoLabel(state.last)}.`
        : 'No request recorded in this session.';
    // The route follows the relay's decision while this combo is live, and returns to the
    // configured starting point when it is not: a serving step must not get stuck on the tile.
    const configuredRoute = comboConfiguredRoute(combo);
    const preview = comboRoutePreview(combo, state ? { live: state.live, step: state.step } : null);
    parts.route.textContent = preview.source === 'serving' ? preview.route : configuredRoute;
    parts.route.title = preview.live ? preview.title : configuredRoute;
  }

  function agoLabel(timestamp) {
    const seconds = Math.max(0, Math.round((Date.now() - timestamp) / 1000));
    if (seconds < 45) return 'just now';
    if (seconds < 3600) return `${Math.max(1, Math.round(seconds / 60))}m ago`;
    if (seconds < 86400) return `${Math.round(seconds / 3600)}h ago`;
    return `${Math.round(seconds / 86400)}d ago`;
  }

  // Manual order is the only order that belongs to the operator, so every other mode is a pure
  // view over the same array. Pins are a separate stable partition on top: pinning never mutates
  // manual order, and a drag never crosses the pinned boundary.
  function orderedCombos() {
    const manualPos = new Map(manualOrder.map((id, index) => [id, index]));
    const configPos = new Map(combos.map((combo, index) => [combo.id, index]));
    const base = (combo) => manualPos.has(combo.id) ? manualPos.get(combo.id) : combos.length + configPos.get(combo.id);
    const list = combos.slice();
    if (sortMode === 'name') {
      list.sort((a, b) => (a.name || a.id).localeCompare(b.name || b.id, undefined, { sensitivity: 'base' }) || base(a) - base(b));
    } else if (sortMode === 'steps') {
      list.sort((a, b) => (b.steps?.length ?? 0) - (a.steps?.length ?? 0) || base(a) - base(b));
    } else if (sortMode === 'recent') {
      list.sort((a, b) => (activityByCombo.get(b.id)?.last ?? 0) - (activityByCombo.get(a.id)?.last ?? 0) || base(a) - base(b));
    } else {
      list.sort((a, b) => base(a) - base(b));
    }
    return [
      ...list.filter((combo) => pinned.has(combo.id)),
      ...list.filter((combo) => !pinned.has(combo.id)),
    ];
  }

  function comboHref(id) {
    return `#/combos/${encodeURIComponent(id)}`;
  }

  function focusTile(id, selector = '.tile-main') {
    for (const li of listEl.children) {
      if (li.dataset.comboId === id) { li.querySelector(selector)?.focus({ preventScroll: true }); return; }
    }
  }

  function renderList() {
    const canReorder = sortMode === 'manual';
    listEl.classList.toggle('is-manual', canReorder);
    listEl.replaceChildren();
    tileParts.clear();
    for (const c of orderedCombos()) {
      const steps = Array.isArray(c.steps) ? c.steps : [];
      const selected = c.id === selectedId;
      const isPinned = pinned.has(c.id);
      const state = h('span', { class: 'tile-state' });
      const route = h('div', { class: 'tile-route mono' });
      const parts = { state, route };
      tileParts.set(c.id, parts);
      paintTileState(parts, c);

      const pinBtn = h('button', {
        class: 'pin-btn' + (isPinned ? ' pinned' : ''),
        type: 'button',
        'aria-pressed': isPinned ? 'true' : 'false',
        'aria-label': (isPinned ? 'Unpin ' : 'Pin ') + (c.name || c.id),
        title: isPinned ? 'Unpin' : 'Pin',
      }, icon('pin'));
      pinBtn.addEventListener('click', (event) => {
        event.stopPropagation();
        togglePin(c.id);
      });

      const grip = h('span', {
        class: 'combo-grip' + (canReorder ? '' : ' is-disabled'),
        title: canReorder ? 'Drag to reorder (Alt+↑/↓)' : 'Switch to Manual to reorder',
        'aria-hidden': 'true',
      }, icon('grip'));

      const anchor = h('a', {
        class: 'tile-main',
        href: comboHref(c.id),
        'aria-current': selected ? 'page' : null,
        'aria-label': `${c.name || c.id}, ${steps.length} step${steps.length === 1 ? '' : 's'}, ${c.enabled === false ? 'disabled' : 'enabled'}`,
      },
        h('div', { class: 'tile-top' },
          h('span', { class: 'tile-name' }, c.name || c.id),
          c.enabled === false ? chip('off') : null),
        route,
        h('div', { class: 'tile-meta' },
          h('span', { class: 'tile-count' }, `${steps.length} step${steps.length === 1 ? '' : 's'}`),
          state));

      const li = h('li', {
        class: 'combo-tile' + (selected ? ' is-selected' : '') + (isPinned ? ' is-pinned' : ''),
        'data-combo-id': c.id,
        draggable: canReorder ? 'true' : 'false',
      }, grip, anchor, pinBtn);

      li.addEventListener('click', (event) => {
        if (dragSuppressed) {
          event.preventDefault();
          event.stopPropagation();
          return;
        }
        if (event.target.closest('button, a')) return;
        ctx.navigate(comboHref(c.id));
      });
      li.addEventListener('keydown', (event) => {
        if (!canReorder) return;
        if (!event.altKey || (event.key !== 'ArrowUp' && event.key !== 'ArrowDown')) return;
        const list = orderedCombos();
        const index = list.findIndex((combo) => combo.id === c.id);
        const target = list[index + (event.key === 'ArrowUp' ? -1 : 1)];
        if (!target) return;
        event.preventDefault();
        reorderCombo(c.id, target.id, event.key === 'ArrowUp' ? 'before' : 'after');
        focusTile(c.id);
      });

      li.addEventListener('dragstart', (event) => {
        if (!canReorder) { event.preventDefault(); return; }
        if (event.target.closest('button, a')) { event.preventDefault(); return; }
        draggedId = c.id;
        dragSuppressed = true;
        if (event.dataTransfer) {
          event.dataTransfer.effectAllowed = 'move';
          try { event.dataTransfer.setData('text/plain', c.id); } catch { /* External drags only. */ }
        }
        document.body?.classList.add('combo-dragging');
        li.classList.add('dragging');
      });
      li.addEventListener('dragend', endDrag);
      li.addEventListener('dragover', (event) => {
        if (!canReorder || !draggedId || draggedId === c.id) return;
        if (pinned.has(draggedId) !== isPinned) return;
        event.preventDefault();
        if (event.dataTransfer) event.dataTransfer.dropEffect = 'move';
        const rect = li.getBoundingClientRect();
        const after = event.clientY > rect.top + rect.height / 2;
        for (const row of listEl.children) row.classList.remove('drag-over', 'drag-after');
        li.classList.add('drag-over');
        li.classList.toggle('drag-after', after);
      });
      li.addEventListener('dragleave', () => li.classList.remove('drag-over', 'drag-after'));
      li.addEventListener('drop', (event) => {
        event.preventDefault();
        const source = draggedId;
        const position = li.classList.contains('drag-after') ? 'after' : 'before';
        endDrag();
        if (source) reorderCombo(source, c.id, position);
      });

      listEl.append(li);
    }
  }

  function endDrag() {
    draggedId = null;
    document.body?.classList.remove('combo-dragging');
    for (const row of listEl.children) row.classList.remove('dragging', 'drag-over', 'drag-after');
    setTimeout(() => { dragSuppressed = false; }, 0);
  }

  // Reorder the saved array itself, not the visible order. Pins sit above the array for display,
  // so freezing the screen into the array here would move a pinned combo to the top permanently.
  function reorderCombo(sourceId, targetId, position) {
    if (sourceId === targetId) return false;
    if (sortMode !== 'manual') return false;
    if (pinned.has(sourceId) !== pinned.has(targetId)) return false;
    const known = combos.map((combo) => combo.id);
    const order = manualOrder.filter((id) => known.includes(id));
    for (const id of known) if (!order.includes(id)) order.push(id);
    const from = order.indexOf(sourceId);
    if (from < 0) return false;
    order.splice(from, 1);
    let to = order.indexOf(targetId);
    if (to < 0) return false;
    if (position === 'after') to += 1;
    order.splice(to, 0, sourceId);
    manualOrder = order;
    if (!storeIdList(COMBO_ORDER_KEY, manualOrder)) {
      ctx.toast.warning('This browser could not save the combo order.');
    }
    renderList();
    return true;
  }

  // Sorting is a view over the saved manual array. It must never rewrite that array: switching to
  // Name and back has to return the operator to exactly the order they left, not a sorted snapshot.
  function switchSort(mode) {
    if (!COMBO_SORTS.has(mode) || mode === sortMode) return;
    sortMode = mode;
    try { localStorage.setItem(COMBO_SORT_KEY, sortMode); } catch { /* The mode still applies this visit. */ }
    renderList();
  }

  function togglePin(id) {
    if (pinned.has(id)) pinned.delete(id);
    else pinned.add(id);
    if (!storeIdList(COMBO_PINS_KEY, [...pinned])) {
      ctx.toast.warning('This browser could not save pinned combos.');
    }
    renderList();
    focusTile(id, '.pin-btn');
  }

  function renderEmptyDetail() {
    right.classList.add('is-empty');
    const recent = recentCombos();
    const newButton = h('button', { class: 'btn btn-primary', type: 'button' }, 'New combo');
    newButton.addEventListener('click', () => { void startNewEditor(); });
    if (!combos.length) {
      right.replaceChildren(h('div', { class: 'combo-empty' },
        h('p', { class: 'combo-empty-kicker' }, 'Combos'),
        h('h2', {}, 'No combos yet'),
        h('p', {}, 'A combo is a named model. Clients call it like any model, and it resolves to an ordered list of steps.'),
        newButton));
      return;
    }
    right.replaceChildren(h('div', { class: 'combo-empty' },
      h('p', { class: 'combo-empty-kicker' }, 'Combo detail'),
      h('h2', {}, 'Select a combo'),
      h('p', {}, 'Open a route to inspect its steps, live health, and order.'),
      recent.length
        ? h('div', { class: 'combo-recent' },
          h('h3', { class: 'section-label' }, 'Recently used'),
          h('ul', {}, ...recent.map((combo) => h('li', {},
            h('a', { href: comboHref(combo.id) },
              h('span', { class: 'combo-recent-name' }, combo.name || combo.id),
              h('span', { class: 'combo-recent-route mono' }, combo.steps?.[0] ? stepLabel(combo.steps[0]) : 'no steps'))))))
        : h('p', { class: 'combo-empty-note' }, 'No recent activity yet.'),
      newButton));
  }

  function recentCombos() {
    const seen = new Set();
    const result = [];
    const entries = [...activityEntries].sort((a, b) => (Number(b?.started_at) || 0) - (Number(a?.started_at) || 0));
    for (const entry of entries) {
      if (!entry || typeof entry.combo !== 'string') continue;
      const combo = combos.find((c) => c.name === entry.combo || c.id === entry.combo);
      if (!combo || seen.has(combo.id)) continue;
      seen.add(combo.id);
      result.push(combo);
      if (result.length === 3) break;
    }
    return result;
  }

  // ------------------------------------------------------------------ detail drawer and summary

  function closeDrawer() {
    // Detach the editor and retract its guard together. A close that only removed the DOM would
    // leave ctx.leaveGuard pointing at a form that no longer exists, so the next page change would
    // ask about edits that are already gone.
    activeEditor?.discard();
    mount.querySelector('.drawer')?.remove();
    activeEditor = null;
  }

  // Ask before an editor is replaced, not only before the page changes. New combo and Edit combo
  // can both be clicked while a drawer is open; either one would otherwise unmount unsaved edits
  // silently and leave the old guard armed.
  async function confirmDiscardEditor(message) {
    const editor = activeEditor;
    if (!editor?.isDirty()) return true;
    if (closingEditor) return false;
    closingEditor = true;
    try {
      return await ctx.confirm({
        title: 'Leave without saving?',
        message,
        confirmLabel: 'Discard changes',
        cancelLabel: 'Keep editing',
        danger: true,
      });
    } finally {
      closingEditor = false;
    }
  }

  async function requestCloseDrawer() {
    if (!(await confirmDiscardEditor(
      'This combo has unsaved changes. Closing the editor will discard them.'))) return;
    closeDrawer();
  }

  async function startNewEditor() {
    if (!(await confirmDiscardEditor(
      'This combo has unsaved changes. Starting a new combo will discard them.'))) return;
    closeDrawer();
    renderEditor(null, formSlot);
  }

  function drawerShell(title) {
    closeDrawer();
    const head = h('div', { class: 'drawer-head' },
      h('h2', {}, title),
      h('span', { class: 'spacer' }));
    const body = h('div', { class: 'drawer-body' });
    const drawer = h('div', { class: 'drawer', role: 'region', 'aria-label': 'Combo detail' }, head, body);
    drawer.addEventListener('keydown', (e) => {
      if (e.key === 'Escape') { e.preventDefault(); void requestCloseDrawer(); }
    });
    mount.append(drawer);
    return { drawer, head, body };
  }

  async function openEditorDrawer(saved, options = {}) {
    if (!(await confirmDiscardEditor(
      'This combo has unsaved changes. Opening another combo will discard them.'))) return;
    const copy = options.copy === true && saved;
    const title = copy
      ? `Duplicate combo ${saved.name || saved.id}`
      : saved ? `Edit combo ${saved.name || saved.id}` : 'New combo';
    const { drawer, head, body } = drawerShell(title);
    head.append(h('button', {
      class: 'icon-btn', type: 'button', 'aria-label': 'Close combo editor',
      onclick: () => { void requestCloseDrawer(); },
    }, icon('x')));
    renderEditor(saved, body, options);
    // A new editor focuses its name field. An existing one has no natural first input, so put
    // focus on the drawer itself rather than leaving it on the summary button behind it — Escape
    // must close this editor, and a focused element outside the drawer never reaches its handler.
    if (!body.contains(document.activeElement)) {
      drawer.tabIndex = -1;
      drawer.focus({ preventScroll: true });
    }
  }

  // A copy is a new callable name. Suffix rather than overwrite; the editor still lets the
  // operator rename it before Create, so this only has to be unique and recognisable.
  function duplicateComboName(base) {
    const root = `${base || 'combo'}-copy`;
    let name = root;
    let suffix = 2;
    while (combos.some((combo) => combo.name === name)) name = `${root}-${suffix++}`;
    return name;
  }

  function renderSummary(target, combo) {
    target.classList.remove('is-empty');
    const steps = Array.isArray(combo.steps) ? combo.steps : [];
    const firstEnabled = steps.findIndex((step) => step.enabled !== false);
    const statusSlots = new Map();
    const editBtn = h('button', { class: 'btn btn-primary', type: 'button' }, 'Edit combo');
    editBtn.addEventListener('click', () => { void openEditorDrawer(combo); });
    const duplicateBtn = h('button', { class: 'btn', type: 'button' }, 'Duplicate');
    duplicateBtn.addEventListener('click', () => { void openEditorDrawer(combo, { copy: true }); });

    const stepItems = steps.map((step, i) => {
      const statusSlot = h('span', { class: 'combo-step-status' });
      statusSlots.set(i, statusSlot);
      const off = step.enabled === false;
      let role;
      let condition;
      if (off) {
        role = 'disabled';
        condition = 'skipped when this combo compiles';
      } else if (i === firstEnabled) {
        role = combo.strategy === 'reset-aware-priority' ? 'primary · reset-aware' : 'primary';
        condition = 'first enabled step';
      } else if (i < firstEnabled || firstEnabled < 0) {
        role = 'fallback';
        condition = 'used when earlier steps are unavailable';
      } else {
        role = combo.strategy === 'reset-aware-priority' ? 'fallback · reset-aware' : 'fallback';
        condition = combo.strategy === 'reset-aware-priority'
          ? 'skips while its accounts cool down'
          : 'tried when earlier steps fail';
      }
      const extras = [];
      if (step.combo) extras.push(`expands ${step.combo}`);
      if (step.level) extras.push(`runs at ${step.level}`);
      if (step.weight) extras.push(`weight ${step.weight}`);
      if (combo.hedge?.enabled && (combo.hedge.participants ?? []).includes(i)) extras.push('hedge participant');
      return h('li', { class: 'combo-step' + (off ? ' is-off' : '') },
        h('span', { class: 'combo-step-num mono', 'aria-hidden': 'true' }, String(i + 1)),
        h('div', { class: 'combo-step-body' },
          h('div', { class: 'combo-step-route' },
            h('span', { class: 'combo-step-ref mono' }, stepLabel(step)),
            !step.combo && step.level
              ? chip(`at ${step.level}`, 'chip-neutral', 'reasoning level this step runs at')
              : null,
            step.combo ? chip('nested combo', 'chip-neutral', `this step expands the steps of ${step.combo}`) : null,
            statusSlot),
          h('div', { class: 'combo-step-sub' },
            h('span', { class: 'combo-role' }, role),
            h('span', { class: 'combo-condition' }, extras.length ? `${condition} · ${extras.join(' · ')}` : condition))));
    });

    const header = h('header', { class: 'combo-detail-head' },
      h('div', { class: 'combo-detail-identity' },
        h('p', { class: 'combo-kicker' }, strategyKicker(combo)),
        // The UUID is an implementation detail; the name is what clients call and what an
        // operator recognises. It stays in the URL, not in the reading order of the page.
        h('h2', { class: 'combo-detail-name' }, combo.label || combo.name || combo.id)),
      h('div', { class: 'combo-detail-side' },
        h('div', { class: 'combo-badges' },
          chip(`${steps.length} step${steps.length === 1 ? '' : 's'}`),
          combo.enabled === false ? chip('disabled') : chip('enabled', 'chip-green'),
          combo.hedge?.enabled ? chip('hedged', 'chip-pink', 'hedged race enabled') : null),
        h('div', { class: 'combo-detail-actions' }, duplicateBtn, editBtn)));

    const healthSection = h('section', { class: 'combo-health' });
    target.replaceChildren(
      header,
      h('p', { class: 'combo-purpose' }, comboPurpose(combo)),
      h('section', { class: 'combo-steps-section' },
        h('h3', { class: 'section-label' }, 'Steps · priority order'),
        steps.length
          ? h('ol', { class: 'combo-steps' }, ...stepItems)
          : h('p', { class: 'hint' }, 'This combo has no steps yet, so it cannot route a request.')),
      healthSection);
    renderHealth(healthSection, combo, statusSlots);
  }

  function strategyKicker(combo) {
    const strategy = combo.strategy ?? 'priority';
    if (strategy === 'reset-aware-priority') return 'Combo · reset-aware priority';
    if (strategy === 'round-robin') return 'Combo · round-robin';
    return 'Combo · priority route';
  }

  // One sentence that says what will actually happen to a request, not a repetition of the chips.
  // The detailed rules live in the steps and the docs; this is the answer to "what is this thing?"
  function comboPurpose(combo) {
    const steps = Array.isArray(combo.steps) ? combo.steps : [];
    const enabled = steps.filter((step) => step.enabled !== false);
    const first = enabled[0] ?? steps[0];
    const count = enabled.length;
    if (!steps.length) return 'No steps yet. Add a model step or a nested combo before clients can call this name.';
    if (combo.enabled === false) {
      return `Disabled. Clients calling “${combo.name || combo.id}” cannot route through it until the combo is enabled.`;
    }
    if (combo.hedge?.enabled) {
      const participants = Array.isArray(combo.hedge.participants) ? combo.hedge.participants.length : count;
      const windowMs = Number(combo.hedge.window_ms) || 0;
      return `Hedged race across ${participants || count} step${(participants || count) === 1 ? '' : 's'} with a ${windowMs} ms window. The highest-priority participant that answers inside the window wins.`;
    }
    if ((combo.strategy ?? 'priority') === 'round-robin') {
      return `Round-robin route across ${count} enabled step${count === 1 ? '' : 's'}. Each call starts at the next step and falls forward through the order from there.`;
    }
    if (combo.strategy === 'reset-aware-priority') {
      return `Reset-aware priority across ${count} enabled step${count === 1 ? '' : 's'}, starting with ${first ? stepLabel(first) : '—'}. A step whose accounts are all cooling down drops below usable steps and returns when its quota resets.`;
    }
    return `Priority route across ${count} enabled step${count === 1 ? '' : 's'}, starting with ${first ? stepLabel(first) : '—'}. Each later step is a fallback when the ones above it fail.`;
  }

  /** One step's outcome, as the checker left it. */
  function stepStatus(comboId, index) {
    return checked.get(comboId)?.get(index);
  }

  function statusChip(status) {
    if (!status) return chip('not checked', 'chip-neutral', 'never probed in this session');
    if (status.state === 'checking') return chip('checking…', 'chip-neutral');
    if (status.state === 'ok') return chip(`ok · ${status.ms} ms`, 'chip-green', 'the model answered');
    return chip(`failed · ${status.code ?? status.status}`, 'chip-pink', status.message ?? 'the model did not answer');
  }

  // A check is one tiny completion per model step, in priority order, and it is always the
  // operator's click: nothing here runs on page load or on a timer. A step that routes to another
  // combo has no provider/model pair of its own, so it is listed but not probed; the nested combo
  // carries the model steps that can answer.
  function renderHealth(section, combo, statusSlots) {
    const steps = Array.isArray(combo.steps) ? combo.steps : [];
    const checkable = steps.flatMap((step, index) => {
      const target = step.enabled === false ? null : probeTarget(step);
      return target ? [{ step, index, target }] : [];
    });
    const summary = h('p', { class: 'hint combo-health-summary' });
    const run = h('button', { class: 'btn btn-sm', type: 'button' }, 'Check steps');

    const paint = () => {
      const results = checked.get(combo.id) ?? new Map();
      const answered = checkable.filter(({ index }) => results.get(index)?.state === 'ok');
      const failed = checkable.filter(({ index }) => results.get(index)?.state === 'failed');
      for (const [index, slot] of statusSlots) {
        const step = steps[index];
        if (!step || step.combo || step.enabled === false) {
          slot.replaceChildren();
          continue;
        }
        const status = results.get(index);
        slot.replaceChildren(...(status ? [statusChip(status)] : []));
        slot.title = status?.message ?? '';
      }
      if (!checkable.length) {
        summary.textContent = 'No enabled model steps to check; a step that routes to another combo is checked in that combo.';
      } else if (!results.size) {
        summary.textContent = `Sends one trivial completion per enabled model step, in order. Nothing runs until you ask.`;
      } else if (answered.length) {
        summary.textContent = `${answered.length} of ${checkable.length} answering${failed.length ? `, ${failed.length} failing` : ''}.`;
      } else {
        summary.textContent = `Nothing answered${failed.length ? `: ${failed.map(({ index }) => `step ${index + 1}`).join(', ')}` : ''}.`;
      }
    };

    run.addEventListener('click', async () => {
      if (run.disabled) return;
      run.disabled = true;
      run.textContent = 'Checking…';
      const results = new Map();
      checked.set(combo.id, results);
      try {
        for (const { index, target } of checkable) {
          results.set(index, { state: 'checking' });
          paint();
          const started = performance.now();
          try {
            // A step's own 401 is a provider refusal, not a dead panel session: report it in this
            // row instead of signing the operator out mid-check.
            const answer = await api.post('/api/models/test', {
              provider: target.provider,
              model: target.model,
              prompt: 'Reply with the single word OK.',
            }, { skipAuthRedirect: true });
            results.set(index, {
              state: 'ok',
              ms: Math.round(performance.now() - started),
              answer: answer?.response ?? null,
            });
          } catch (err) {
            results.set(index, {
              state: 'failed',
              status: err?.status ?? 'error',
              code: err?.code ?? 'failed',
              message: err?.message ?? 'the request failed',
            });
          }
          paint();
        }
      } finally {
        run.disabled = false;
        run.textContent = 'Check steps';
      }
    });

    paint();
    section.replaceChildren(
      h('div', { class: 'combo-health-head' },
        h('div', {},
          h('h3', { class: 'section-label' }, 'Live health'),
          summary),
        run));
  }

  // ------------------------------------------------------------------ editor

  function renderEditor(saved, target = right, options = {}) {
    const copy = options.copy === true && saved;
    const isNew = saved === null || Boolean(copy);
    // Draft is a deep copy: edits are local until Save; Revert re-reads from `combos`.
    const draft = isNew
      ? (copy ? JSON.parse(JSON.stringify(saved)) : newDraft())
      : JSON.parse(JSON.stringify(saved));
    if (copy) {
      // The id belongs to the source record; keeping it would turn Create into an overwrite.
      delete draft.id;
      draft.name = duplicateComboName(saved.name || saved.id);
    }
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
    const savedSnapshot = JSON.stringify(draft);
    let dirty = false;
    let guard = null;

    function markDirty() {
      if (dirty) return;
      dirty = true;
      guard = () => ctx.confirm({
        title: 'Leave without saving?',
        message: 'This combo has unsaved changes. Leaving will discard them.',
        confirmLabel: 'Leave and discard',
        cancelLabel: 'Stay on this page',
        danger: true,
      });
      ctx.leaveGuard = guard;
    }
    function clearDirty() {
      dirty = false;
      // Only retract our own guard: a late clear from an old editor must not disarm a newer one.
      if (guard && ctx.leaveGuard === guard) ctx.leaveGuard = null;
      guard = null;
    }
    // The drawer owns this editor's lifetime. The router clears the guard after a confirmed page
    // change, but closing the drawer stays on the same route and must clean up on its own.
    activeEditor = { isDirty: () => dirty, discard: clearDirty };

    const title = h('h2', { style: 'text-transform:none;letter-spacing:0;font-size:14px;' },
      copy ? 'Copy details' : isNew ? 'New combo' : `Editing ${saved.name || saved.id}`);
    const nameIn = h('input', { class: 'input mono', type: 'text', 'data-field': 'name', value: draft.name ?? '', placeholder: 'callable model name, e.g. Brains' });
    const labelIn = h('input', { class: 'input', type: 'text', 'data-field': 'label', value: draft.label ?? '' });

    const stratSel = h('select', { class: 'input', 'data-field': 'strategy' });
    for (const [v, lbl] of STRATEGIES) stratSel.append(h('option', { value: v, selected: (draft.strategy ?? 'priority') === v }, lbl));
    const stratHint = h('p', { class: 'hint' });
    stratSel.addEventListener('change', () => { updateStrategyHint(); markDirty(); });

    // --- steps ---
    const stepsOl = h('ol', { class: 'steps' });
    const stepCallout = h('div', { class: 'callout' },
      h('strong', {}, 'This order is your priority order. '),
      'The panel and the router never sort, re-weight or second-guess it: step 1 is tried first, ',
      'and in a hedged race the winner is the highest-priority participant that replied within the window. ',
      'Reorder by dragging a row — the number on the left is the rank.');

    // Three pickers, one row. A step is either provider/model or a route to another combo; the
    // small selector swaps which ones are visible so the common case keeps two fields.
    const addKind = h('select', { class: 'input add-kind', 'aria-label': 'What this step routes to' },
      h('option', { value: 'model' }, 'model'),
      h('option', { value: 'combo' }, 'combo'));
    const addProv = createSearchSelect({
      ariaLabel: 'Provider for new step',
      placeholder: 'provider…',
      emptyText: 'no provider matches',
      onChoose: () => syncAddModels(),
    });
    const addModel = createSearchSelect({
      ariaLabel: 'Model for new step',
      placeholder: 'model id',
      emptyText: 'no model matches',
    });
    const addCombo = createSearchSelect({
      ariaLabel: 'Combo this step routes to',
      placeholder: 'combo name…',
      emptyText: 'no combo matches',
    });
    addCombo.el.hidden = true;
    const addNote = h('input', { class: 'input', type: 'text', 'aria-label': 'Note for new step (optional)', placeholder: 'note (optional)' });
    const addBtn = h('button', { class: 'btn', type: 'button' }, 'Add step');
    const addRow = h('div', { class: 'add-step-row' },
      addKind, addProv.el, addModel.el, addCombo.el, addNote, addBtn);

    function providerChoices() {
      return (providers ?? []).map((p) => ({
        value: p.id,
        label: p.id,
        hint: p.label && p.label !== p.id ? p.label : undefined,
      }));
    }
    function modelChoices(providerId) {
      const provider = (providers ?? []).find((p) => p.id === providerId);
      return (provider?.models ?? []).map((m) => ({
        value: m.id,
        label: m.id,
        hint: m.label && m.label !== m.id ? m.label : undefined,
      }));
    }
    function comboChoices() {
      return combos
        .filter((c) => c.id !== draft.id && c.name !== draft.name)
        .map((c) => ({
          value: c.name || c.id,
          label: c.name || c.id,
          hint: c.name && c.name !== c.id ? c.id : undefined,
        }));
    }
    function syncAddModels() {
      addModel.setOptions(modelChoices(addProv.value()));
    }
    function syncAddKind() {
      const toCombo = addKind.value === 'combo';
      addProv.el.hidden = toCombo;
      addModel.el.hidden = toCombo;
      addCombo.el.hidden = !toCombo;
    }
    addKind.addEventListener('change', syncAddKind);

    addProv.setOptions(providerChoices());
    addCombo.setOptions(comboChoices());
    ctx.loadProviders().then((next) => {
      providers = next ?? providers;
      addProv.setOptions(providerChoices());
      addCombo.setOptions(comboChoices());
      syncAddModels();
    }).catch(() => { /* the editor still works with what was loaded */ });
    syncAddModels();

    addBtn.addEventListener('click', () => {
      const note = addNote.value.trim() || undefined;
      if (addKind.value === 'combo') {
        const combo = addCombo.value();
        if (!combo) { ctx.toast.warning('pick a combo for the step'); return; }
        if (combo === draft.name || combo === draft.id) {
          ctx.toast.warning('a combo cannot route to itself');
          return;
        }
        // Appends at the END: a new step starts at the lowest priority. It is never inserted
        // "where it fits" — only the operator decides placement.
        draft.steps.push({ combo, enabled: true, note });
        addCombo.setValue(''); addNote.value = '';
      } else {
        const provider = addProv.value();
        const model = addModel.value();
        const problems = [];
        if (!provider) problems.push('pick a provider for the step');
        else if (!(providers ?? []).some((p) => p.id === provider)) problems.push('pick a provider from the list');
        if (!model) problems.push('enter a model id for the step');
        if (problems.length) { ctx.toast.warning(problems.join(' — ')); return; }
        draft.steps.push({ provider, model, enabled: true, note });
        addModel.setValue(''); addNote.value = '';
      }
      renderSteps(); renderStableSelect(); renderParticipantSummary();
      markDirty();
    });

    // --- hedge ---
    const hedgeToggle = toggle('Enable the hedged race for this combo', { checked: Boolean(draft.hedge?.enabled) });
    const hedgeOn = hedgeToggle.input;
    const stickyWindowInput = h('input', { class: 'input', type: 'number', min: '0', max: '604800', value: draft.sticky_success_window_secs ?? 1800 });
    stickyWindowInput.addEventListener('input', () => markDirty());
    const stickyWindowBox = field('Sticky success window (seconds)', stickyWindowInput, { hint: 'Priority and reset-aware combos. Default 1800s (30 minutes). Combo-wide across callers; inactivity refreshes after a successful response. Set 0 to disable.' });
    const hedgeCount = h('input', { class: 'input', type: 'number', min: '2', max: '255', 'data-field': 'count', value: draft.hedge?.count ?? 3 });
    hedgeCount.addEventListener('input', () => markDirty());
    const hedgeWindow = h('input', { class: 'input', type: 'number', min: '0', 'data-field': 'window_ms', value: draft.hedge?.window_ms ?? 800 });
    hedgeWindow.addEventListener('input', () => markDirty());
    const metricSel = h('select', { class: 'input', 'data-field': 'winner_metric' });
    for (const [v, lbl] of WINNER_METRICS) metricSel.append(h('option', { value: v, selected: (draft.hedge?.winner_metric ?? 'first-response-byte') === v }, lbl));
    const metricHint = h('p', { class: 'hint' });
    metricSel.addEventListener('change', () => { updateMetricHint(); markDirty(); });

    const failSel = h('select', { class: 'input', 'data-field': 'on_winner_fail' });
    for (const [v, lbl] of FAIL_MODES) failSel.append(h('option', { value: v, selected: draft.wf.mode === v }, lbl));
    const failHint = h('p', { class: 'hint' });
    const maxRounds = h('input', { class: 'input', type: 'number', min: '1', max: '255', 'data-field': 'max_rounds', value: draft.wf.maxRounds ?? 2 });
    maxRounds.addEventListener('input', () => markDirty());
    const stableSel = h('select', { class: 'input', 'data-field': 'step' });
    const maxRoundsField = field('Max rounds', maxRounds, { hint: 'Bound for re-run excluding.' });
    const stableField = field('Stable step', stableSel, { hint: 'The on-fail model; need not be a race participant.' });
    failSel.addEventListener('change', () => { draft.wf.mode = failSel.value; renderFailExtras(); updateFailHint(); markDirty(); });
    stableSel.addEventListener('change', () => { draft.wf.step = Number(stableSel.value); markDirty(); });
    const cancelLosersToggle = toggle('Cancel losing requests once the winner is chosen', { checked: draft.hedge?.cancel_losers ?? true });
    const cancelLosers = cancelLosersToggle.input;
    cancelLosers.addEventListener('change', () => markDirty());
    const loserGrace = h('input', { class: 'input', type: 'number', min: '0', 'data-field': 'loser_grace_ms', value: draft.hedge?.loser_grace_ms ?? 0 });
    loserGrace.addEventListener('input', () => markDirty());
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
    hedgeOn.addEventListener('change', () => { hedgeBox.style.opacity = hedgeOn.checked ? '1' : '0.55'; stickyWindowBox.style.opacity = hedgeOn.checked ? '1' : '0.55'; markDirty(); });
    hedgeBox.style.opacity = hedgeOn.checked ? '1' : '0.55';
    renderStableSelect(); renderFailExtras(); renderParticipantSummary();
    updateStrategyHint(); updateMetricHint(); updateFailHint();

    // --- delivery ---
    const midSel = h('select', { class: 'input', 'data-field': 'midstream_policy' },
      h('option', { value: 'restart', selected: (draft.midstream_policy ?? 'restart') === 'restart' }, 'restart'),
      h('option', { value: 'terminate', selected: draft.midstream_policy === 'terminate' }, 'terminate'));
    midSel.addEventListener('change', () => markDirty());
    const maxRetries = h('input', { class: 'input', type: 'number', min: '0', 'data-field': 'max_retries', value: draft.max_retries ?? 1 });
    maxRetries.addEventListener('input', () => markDirty());
    const retryDelay = h('input', { class: 'input', type: 'number', min: '0', 'data-field': 'retry_delay_ms', value: draft.retry_delay_ms ?? 0 });
    retryDelay.addEventListener('input', () => markDirty());
    const affinityToggle = toggle('Merge identical consecutive messages when translating between dialects', { checked: Boolean(draft.affinity) });
    const affinity = affinityToggle.input;
    affinity.addEventListener('change', () => markDirty());
    const comboEnabledToggle = toggle('Combo enabled', { checked: draft.enabled ?? true });
    const comboEnabled = comboEnabledToggle.input;
    comboEnabled.addEventListener('change', () => markDirty());

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
      // A combo target is a complete step without provider or model; a model step still needs both.
      if (draft.steps.some((s) => (s.combo ? !String(s.combo).trim() : (!s.provider || !String(s.model ?? '').trim())))) {
        problems.push({ field: 'steps', message: 'every step needs a provider and a model id, or a combo' });
      }
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
        // An empty field means "leave it at the default", not "switch it off". Reading it as 0
        // silently disabled stickiness on every combo saved through this form.
        sticky_success_window_secs: stickyWindowInput.value.trim() === '' ? 1800 : (Number(stickyWindowInput.value) || 0),
        steps: draft.steps.map((s) => {
          const step = { ...s, note: s.note || undefined };
          // A combo target ignores provider, model and pinned level at compile time. Strip them
          // rather than write fields that look meaningful but are never read.
          if (step.combo) {
            delete step.provider;
            delete step.model;
            delete step.level;
            // A nested combo supplies its own steps' account choices; a selector here would name no
            // provider's account pool and is never read.
            delete step.accounts;
          }
          return step;
        }),
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
          ctx.toast.success(`Combo ${payload.name} created.`);
          clearDirty();
          closeDrawer();
          ctx.navigate(`#/combos/${encodeURIComponent(created?.id ?? payload.name)}`);
        } else {
          const updated = await api.put(`/api/combos/${encodeURIComponent(saved.id)}`, payload);
          ctx.toast.success(`Combo ${payload.name} saved.`);
          clearDirty();
          closeDrawer();
          // `ctx.navigate('#/combos')` is a no-op when we are already on that route, so the list
          // would keep showing the pre-save copy. Patch the in-memory row and re-render in place.
          const idx = combos.findIndex((c) => c.id === saved.id);
          if (idx >= 0) {
            combos[idx] = updated?.id ? updated : { ...payload, id: saved.id };
            renderList();
            renderSummary(right, combos[idx]);
          }
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
        // The record is gone, so its editor must not ask to preserve unsaved edits on the way out.
        clearDirty();
        closeDrawer();
        const idx = combos.findIndex((c) => c.id === saved.id);
        if (idx >= 0) {
          combos.splice(idx, 1);
          if (selectedId === saved.id) selectedId = combos[0]?.id ?? null;
          renderList();
          count.textContent = `${combos.length} combo${combos.length === 1 ? '' : 's'}`;
          if (selectedId) {
            const found = combos.find((c) => c.id === selectedId);
            if (found) renderSummary(right, found);
          } else {
            renderEmptyDetail();
          }
        }
        ctx.navigate('#/combos');
      } catch (err) {
        ctx.toast.error('Delete failed', err);
      }
    });

    // Re-render in the same slot: an existing combo's editor lives in the drawer, and the inline
    // right-hand slot would leave the old, still-dirty drawer mounted beside a second form.
    revertBtn.addEventListener('click', () => { clearDirty(); renderEditor(saved, target); });

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
      // A draft has nothing to revert or delete yet; disabled controls read as a trap.
      h('div', { class: 'dialog-actions' }, isNew ? null : revertBtn, isNew ? null : delBtn, saveBtn),
    );

    nameIn.addEventListener('input', () => { draft.name = nameIn.value; markDirty(); });
    labelIn.addEventListener('input', () => { draft.label = labelIn.value; markDirty(); });

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

        // A restricted step says so where it is scanned. The picker is two clicks away, and an
        // operator reading priorities should not have to open every row to learn that one of them
        // covers two accounts out of four. Nothing ticked is the default and shows no chip at all.
        const selectionPool = stepProvider(step, providers)?.accounts;
        const pool = Array.isArray(selectionPool) ? selectionPool : [];
        const selection = stepAccountSelection(step.accounts, pool);
        const accountsChip = selection.all ? null : chip(
          `${selection.selected.length} of ${pool.length} accounts`,
          selection.missing.length ? 'chip-red' : 'chip-neutral',
          `${describeAccountSelection(step.accounts, pool)}; edit the step to change it`,
        );

        const race = h('input', {
          type: 'checkbox', 'aria-label': `Step ${i + 1} races (hedged participant)`,
        });
        race.checked = (draft.hedge.participants ?? []).includes(i);
        race.addEventListener('change', () => {
          draft.hedge.participants = toggleParticipant(draft.hedge.participants, i);
          renderParticipantSummary();
          markDirty();
        });

        const on = h('input', { type: 'checkbox', 'aria-label': `Step ${i + 1} enabled` });
        on.checked = step.enabled !== false;
        on.addEventListener('change', () => { draft.steps[i].enabled = on.checked; markDirty(); });

        // A model step may run at its own tier. A combo target has no model to pin a level on:
        // the nested combo supplies its own steps' levels, so there is no selector to offer.
        let levelSelect = null;
        if (!step.combo) {
          // The list is the model's effective levels — declared on the entry, otherwise derived
          // from the provider dialect and capped by what that dialect can send. Offering a level
          // the model does not offer is how a request becomes a silent substitution; a level
          // already configured but no longer offered is kept and labelled with the ceiling it will
          // actually run at, so the value is never silently rewritten.
          const stepMeta = stepModel(step, providers);
          const stepProviderEntry = stepProvider(step, providers);
          const offered = modelLevels(stepMeta, stepProviderEntry);
          const ceiling = offered[offered.length - 1];
          // Every level the provider's dialect can put on the wire is selectable, so a level can
          // be pinned by hand even when the model entry advertises none (a model synced without
          // declared levels used to leave "model default" as the only choice). A level the model
          // does not offer is still honest: the label says what it will actually run at, which is
          // the ceiling the relay substitutes.
          const sendable = providerLevels(stepProviderEntry, stepMeta);
          const levelOptions = [h('option', { value: '' }, 'model default')];
          for (const level of sendable) {
            levelOptions.push(h('option', {
              value: level,
              selected: step.level === level,
            }, offered.includes(level) ? level : `${level} → ${ceiling ?? 'no reasoning'}`));
          }
          if (step.level && !sendable.includes(step.level)) {
            levelOptions.push(h('option', {
              value: step.level, selected: true,
            }, `${step.level} → unsendable on this provider`));
          }
          levelSelect = h('select', {
            class: 'input step-level',
            'aria-label': `Reasoning level for step ${i + 1}`,
            title: 'Reasoning level this step runs at',
          }, ...levelOptions);
          levelSelect.addEventListener('change', () => {
            if (levelSelect.value) draft.steps[i].level = levelSelect.value;
            else delete draft.steps[i].level;
            markDirty();
          });
        }

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
          h('span', { class: 'icon-btn', 'aria-hidden': 'true', title: 'Drag to reorder' }, icon('grip')),
          ref,
          accountsChip,
          levelSelect,
          h('label', { class: 'step-flag' }, race, 'race'),
          h('label', { class: 'step-flag' }, on, 'on'),
          edit, del);

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
      const kindSel = h('select', { class: 'input', 'aria-label': `What step ${i + 1} routes to` },
        h('option', { value: 'model', selected: !step.combo }, 'model'),
        h('option', { value: 'combo', selected: Boolean(step.combo) }, 'combo'));
      const provSel = createSearchSelect({
        ariaLabel: `Provider for step ${i + 1}`,
        placeholder: 'provider…',
        value: step.provider ?? '',
        emptyText: 'no provider matches',
        onChoose: () => { fillModels(); pruneChosen(); },
      });
      const modelSel = createSearchSelect({
        ariaLabel: `Model for step ${i + 1}`,
        placeholder: 'model id',
        value: step.model ?? '',
        emptyText: 'no model matches',
      });
      const comboPick = createSearchSelect({
        ariaLabel: `Combo for step ${i + 1}`,
        placeholder: 'combo name…',
        value: step.combo ?? '',
        emptyText: 'no combo matches',
      });
      const noteIn = h('input', { class: 'input', type: 'text', value: step.note ?? '', placeholder: 'note (optional)', 'aria-label': `Note for step ${i + 1}` });
      const done = h('button', { class: 'btn btn-sm btn-primary', type: 'button' }, 'Done');

      // Which accounts this step may use. An empty selection means *every* account — that is the
      // default and the reason the picker opens with nothing ticked, so the panel has to say
      // "every account" out loud rather than leave an empty box to be read as "none".
      //
      // Selectors are kept verbatim. A step written as `alice` follows whoever carries that tag,
      // so rewriting it to an account id here would freeze a choice the operator expressed in terms
      // of a person; ticking a new account by hand stores its id, which is the right precision for
      // a one-off.
      let chosen = Array.isArray(step.accounts)
        ? step.accounts.map((sel) => String(sel ?? '').trim()).filter(Boolean)
        : [];
      const accountsBox = h('div', { class: 'step-accounts' });

      function providerAccounts() {
        const provider = localProviders().find((entry) => entry.id === provSel.value());
        return Array.isArray(provider?.accounts) ? provider.accounts : [];
      }

      function drawAccounts() {
        const pool = providerAccounts();
        const selection = stepAccountSelection(chosen, pool);
        const rows = pool.map((account) => {
          const box = h('input', {
            type: 'checkbox',
            'aria-label': `Use account ${account.label || account.identity || account.id} in step ${i + 1}`,
          });
          box.checked = chosen.some((sel) => selectorNames(sel, account));
          box.addEventListener('change', () => {
            if (box.checked) {
              if (!chosen.some((sel) => selectorNames(sel, account))) chosen = [...chosen, String(account.id ?? '')];
            } else {
              chosen = chosen.filter((sel) => !selectorNames(sel, account));
            }
            drawAccounts();
          });
          const tags = Array.isArray(account.tags) ? account.tags.filter(Boolean) : [];
          return h('label', { class: 'checkbox-row' },
            box,
            h('span', {}, account.label || account.identity || account.id),
            h('span', { class: 'hint mono' }, account.id ?? ''),
            ...(tags.length ? [chip(tags.join(', '), 'chip-neutral', `tags: ${tags.join(', ')}`)] : []));
        });
        const clear = h('button', { class: 'btn btn-sm', type: 'button' }, 'Use every account');
        clear.disabled = chosen.length === 0;
        clear.addEventListener('click', () => { chosen = []; drawAccounts(); });

        accountsBox.replaceChildren(
          h('div', { class: 'step-accounts-head' },
            h('strong', {}, 'Accounts this step may use'),
            chip(selection.all ? 'every account' : `${selection.selected.length} of ${pool.length}`, 'chip-neutral'),
            clear),
          ...(pool.length
            ? rows
            : [h('p', { class: 'hint' }, 'This provider has no accounts yet, so the step has nothing to choose between.')]),
          h('p', { class: 'hint' }, selection.all
            ? 'Nothing ticked means every account, including any added later.'
            : 'Only the ticked accounts may serve this step; an account added later is not included.'),
          ...(selection.missing.length
            ? [h('p', { class: 'form-errors' },
              `No account of this provider matches ${selection.missing.map((name) => JSON.stringify(name)).join(', ')}.`)]
            : []),
        );
        accountsBox.hidden = kindSel.value === 'combo';
      }

      // An explicit change of provider is an explicit statement of which pool applies: selectors
      // that name something in the new provider survive, the rest do not. Done only on a pick, so
      // opening the editor never silently discards a selector that is merely stale.
      function pruneChosen() {
        const pool = providerAccounts();
        chosen = chosen.filter((sel) => pool.some((account) => selectorNames(sel, account)));
        drawAccounts();
      }

      function localProviders(next) {
        return next !== undefined ? next : (ctx.state.providers ?? providers ?? []);
      }
      function fillModels() {
        const provider = localProviders().find((p) => p.id === provSel.value());
        modelSel.setOptions((provider?.models ?? []).map((m) => ({
          value: m.id,
          label: m.id,
          hint: m.label && m.label !== m.id ? m.label : undefined,
        })));
      }
      function applyKind() {
        const toCombo = kindSel.value === 'combo';
        provSel.el.hidden = toCombo;
        modelSel.el.hidden = toCombo;
        comboPick.el.hidden = !toCombo;
        // A nested combo supplies its own steps' account choices; a selector here would have no
        // provider to resolve against, so the picker does not apply to a combo target.
        accountsBox.hidden = toCombo;
      }
      function fillProviderChoices(next) {
        provSel.setOptions(localProviders(next).map((p) => ({
          value: p.id,
          label: p.id,
          hint: p.label && p.label !== p.id ? p.label : undefined,
        })));
      }
      kindSel.addEventListener('change', applyKind);
      provSel.setOptions(localProviders().map((p) => ({
        value: p.id,
        label: p.id,
        hint: p.label && p.label !== p.id ? p.label : undefined,
      })));
      comboPick.setOptions(combos
        .filter((c) => c.id !== draft.id && c.name !== draft.name)
        .map((c) => ({ value: c.name || c.id, label: c.name || c.id, hint: c.name && c.name !== c.id ? c.id : undefined })));
      fillModels();
      drawAccounts();
      applyKind();

      ctx.loadProviders().then((next) => {
        providers = next ?? providers;
        fillProviderChoices(next);
        fillModels();
        drawAccounts();
      }).catch(() => {});

      done.addEventListener('click', () => {
        const note = noteIn.value.trim() || undefined;
        if (kindSel.value === 'combo') {
          const combo = comboPick.value();
          if (!combo) { ctx.toast.warning('pick a combo for the step'); return; }
          if (combo === draft.name || combo === draft.id) {
            ctx.toast.warning('a combo cannot route to itself');
            return;
          }
          draft.steps[i] = { combo, enabled: step.enabled !== false, note };
        } else {
          const provider = provSel.value();
          const model = modelSel.value();
          if (!provider) { ctx.toast.warning('pick a provider for the step'); return; }
          if (!model) { ctx.toast.warning('enter a model id for the step'); return; }
          const next = { ...step, provider, model, note };
          delete next.combo;
          // Present only when the step is restricted: an empty array would be a second spelling of
          // "every account" in the config file, and the saved document should have one.
          if (chosen.length) next.accounts = [...chosen];
          else delete next.accounts;
          draft.steps[i] = next;
        }
        renderSteps(); renderStableSelect();
        markDirty();
      });
      li.replaceChildren(h('div', { class: 'step-edit-wrap' },
        h('div', { class: 'step-edit' },
          h('span', { class: 'step-pos' }, String(i + 1)), kindSel, provSel.el, modelSel.el, comboPick.el, noteIn, done),
        accountsBox));
      (kindSel.value === 'combo' ? comboPick : modelSel).input.focus();
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
      markDirty();
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
