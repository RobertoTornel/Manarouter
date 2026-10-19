// dom.js — the only place this panel builds DOM.
//
// WHY there is no innerHTML anywhere in this panel: provider labels, model ids, doc text and
// error messages all come from the server and from third-party catalogues. Anything built with
// innerHTML turns those strings into markup; a malicious or merely unlucky provider name would
// become script. Text nodes cannot execute, so every server-supplied string enters the DOM
// through createTextNode (h() does this for plain strings) or el.textContent. If you are about
// to type innerHTML, stop and use h().

let uid = 0;

/**
 * Build an element. attrs keys: class, text (textContent), dataset, aria / data passthrough,
 * `on*` event listeners, `true` for boolean attributes. Children: strings become text nodes,
 * null/false are skipped, arrays are flattened.
 */
export function h(tag, attrs = null, ...children) {
  const el = document.createElement(tag);
  if (attrs) {
    for (const [k, v] of Object.entries(attrs)) {
      if (v === null || v === undefined || v === false) continue;
      if (k === 'class') el.className = v;
      else if (k === 'text') el.textContent = v;
      else if (k === 'dataset') Object.assign(el.dataset, v);
      else if (k.startsWith('on') && typeof v === 'function') el.addEventListener(k.slice(2), v);
      else if (v === true) el.setAttribute(k, '');
      else el.setAttribute(k, String(v));
    }
  }
  appendChildren(el, children);
  return el;
}

function appendChildren(el, kids) {
  for (const c of kids.flat(Infinity)) {
    if (c === null || c === undefined || c === false || c === '') continue;
    el.append(c.nodeType ? c : document.createTextNode(String(c)));
  }
}

export function clear(el) {
  el.replaceChildren();
  return el;
}

// Inline SVG icons. SVG built through createElementNS — never a string, so an icon name can
// never smuggle markup. Names are internal constants, not server data.
const ICON_PATHS = {
  plus: 'M8 3v10M3 8h10',
  x: 'M4 4l8 8M12 4l-8 8',
  trash: 'M3 5h10M6.5 5V3.5h3V5M5 5l.7 8h4.6L11 5',
  copy: 'M6 6h7v7H6zM3 10V3h7',
  sync: 'M13 8a5 5 0 1 1-1.4-3.5M13.2 2.6v2.6h-2.6',
  up: 'M8 13V3M4 7l4-4 4 4',
  down: 'M8 3v10M4 9l4 4 4-4',
  check: 'M3 8.5l3.5 3.5L13 4.5',
  pencil: 'M4 12l.8-2.8 6.4-6.4a1.4 1.4 0 0 1 2 2L6.8 11.2 4 12z',
  grip: null, // drawn with circles below
};

export function icon(name) {
  const NS = 'http://www.w3.org/2000/svg';
  const svg = document.createElementNS(NS, 'svg');
  svg.setAttribute('viewBox', '0 0 16 16');
  svg.setAttribute('class', 'i');
  svg.setAttribute('aria-hidden', 'true');
  if (name === 'grip') {
    for (const [cx, cy] of [[5.5, 4], [10.5, 4], [5.5, 8], [10.5, 8], [5.5, 12], [10.5, 12]]) {
      const c = document.createElementNS(NS, 'circle');
      c.setAttribute('cx', String(cx));
      c.setAttribute('cy', String(cy));
      c.setAttribute('r', '1.15');
      c.setAttribute('fill', 'currentColor');
      svg.append(c);
    }
    return svg;
  }
  const p = document.createElementNS(NS, 'path');
  p.setAttribute('d', ICON_PATHS[name] || '');
  p.setAttribute('fill', 'none');
  p.setAttribute('stroke', 'currentColor');
  p.setAttribute('stroke-width', '1.5');
  p.setAttribute('stroke-linecap', 'round');
  p.setAttribute('stroke-linejoin', 'round');
  svg.append(p);
  return svg;
}

export function spinner() {
  const s = icon('sync');
  s.classList.add('spin');
  const span = h('span', { class: 'icon-btn', 'aria-hidden': 'true' }, s);
  span.style.display = 'inline-flex';
  return span;
}

export function chip(text, cls = 'chip-neutral', title = null) {
  return h('span', { class: `chip ${cls}`, title, text });
}

/* ---------- the help affordance (I2) ---------- */

// The bubble is a CSS ::after on the dot, with position:fixed. Fixed-position descendants escape
// the overflow boxes (#main, .drawer-body, .dialog-body, .table-wrap) that would clip an absolute
// popup; app.css has no transformed ancestor that would establish a nearer containing block.
// JavaScript measures the text in an invisible, style-matched node and sets viewport coordinates
// on the dot as --tip-top / --tip-left. The pseudo-element owns visibility and content: hover and
// :focus-visible both reveal attr(data-tip), with no extra event-state styling. Clamping the
// coordinates and max-width keep the bubble readable at narrow widths and in either direction.
//
// The text is duplicated on purpose: `data-tip` feeds the CSS bubble, while a `.sr-only` span is
// what the control's aria-describedby points at, so a screen reader hears the explanation with the
// setting and never as a detached "?" button.
let measureEl = null;
let tipOwner = null;
let helpEventsReady = false;

function ensureHelpEvents() {
  if (helpEventsReady) return;
  helpEventsReady = true;
  // Delegated from the document, so a .help-dot built anywhere gets the same behaviour, and a
  // re-render that throws the dot away leaves nothing to clean up here.
  const dotFor = (e) => (e.target instanceof Element ? e.target.closest('.help-dot') : null);
  document.addEventListener('pointerover', (e) => { const d = dotFor(e); if (d) positionTip(d); });
  document.addEventListener('focusin', (e) => { const d = dotFor(e); if (d) positionTip(d); });
  document.addEventListener('focusout', (e) => {
    const d = e.target instanceof Element ? e.target.closest('.help-dot') : null;
    if (d && !d.matches(':hover')) tipOwner = null;
  });
  document.addEventListener('pointerout', (e) => {
    const d = dotFor(e);
    // A focused dot keeps its coordinates after the pointer leaves, for keyboard users.
    if (d && !d.matches(':focus-visible')) tipOwner = null;
  });
  // Keep a visible bubble attached to its dot during scroll/resize rather than letting it drift.
  document.addEventListener('scroll', () => { if (tipOwner) positionTip(tipOwner); }, true);
  window.addEventListener('resize', () => { if (tipOwner) positionTip(tipOwner); });
}

function positionTip(dot) {
  const text = dot.getAttribute('data-tip');
  if (!text) return;
  if (!measureEl) {
    measureEl = h('div', { class: 'tip-measure', 'aria-hidden': 'true' });
    document.body.append(measureEl);
  }
  measureEl.textContent = text;
  measureEl.style.direction = getComputedStyle(dot).direction;
  const { width: w, height: hgt } = measureEl.getBoundingClientRect();
  const r = dot.getBoundingClientRect();
  const vw = document.documentElement.clientWidth;
  const vh = document.documentElement.clientHeight;
  const gap = 6;
  const margin = 8;
  // Below the dot unless that would push the bubble out of the viewport, then above it, then
  // pinned inside the viewport — a dot in the last 40px of a phone screen still shows its text.
  let top = r.bottom + gap;
  if (top + hgt > vh - margin) {
    const above = r.top - gap - hgt;
    top = above >= margin ? above : Math.max(margin, vh - margin - hgt);
  }
  // Grows with the reading direction, from the dot's inline start edge: in RTL the bubble's right
  // edge lines up with the dot's right edge instead of starting at its left. Clamped either way.
  const rtl = getComputedStyle(dot).direction === 'rtl';
  let left = rtl ? r.right - w : r.left;
  if (left + w > vw - margin) left = vw - margin - w;
  if (left < margin) left = margin;
  dot.style.setProperty('--tip-top', `${Math.round(top)}px`);
  dot.style.setProperty('--tip-left', `${Math.round(left)}px`);
  dot.dataset.tipSide = top < r.top ? 'above' : 'below';
  tipOwner = dot;
  ensureHelpEvents();
}

/** Merge a description id into the control's aria-describedby, keeping any id already there. */
function describeInput(input, helpId) {
  const prev = input.getAttribute('aria-describedby');
  input.setAttribute('aria-describedby', prev ? `${prev} ${helpId}` : helpId);
}

/**
 * The single place the "?" affordance is built — `field()` and `toggle()` both come through here,
 * so the tooltip, the id wiring and the screen-reader text can never drift apart between them.
 *
 * It returns the pieces rather than a finished box because the two callers place the dot
 * differently (beside a block label, beside a row label); whoever calls `describeInput()` owns
 * pointing the control at `desc`. Returns null when there is no help text.
 */
function helpAffordance(labelText, help) {
  if (!help) return null;
  ensureHelpEvents();
  const id = `help-${++uid}`;
  const btn = h('button', {
    class: 'help-dot',
    type: 'button',
    'aria-label': `Help: ${labelText}`,
    'aria-describedby': id,
    'data-tip': help,
  }, '?');
  const desc = h('span', { class: 'sr-only', id }, help);
  return { btn, desc, id };
}

/**
 * Labelled form field: <div class=field><label for>..</label><input>…</div>, plus the optional
 * "?" help dot (I2) and a hint under the control.
 *
 * The dot is a `<button>`, not a `<span title>`: a tooltip that only shows on mouse hover is
 * unreachable by keyboard, and the whole point of the text is that the setting was not
 * self-explanatory. It is a sibling of the `<label>` rather than a child: `<button>` is labelable
 * content, and a `<label>` may not contain labelable descendants other than its own control.
 */
export function field(labelText, inputEl, { hint = null, id = null, help = null } = {}) {
  const inputId = id ?? `f-${++uid}`;
  inputEl.id = inputId;
  const label = h('label', { for: inputId }, labelText);
  const aff = helpAffordance(labelText, help);
  const box = h('div', { class: 'field' }, aff ? h('div', { class: 'label-line' }, label, aff.btn) : label, inputEl);
  if (aff) {
    describeInput(inputEl, aff.id);
    box.append(aff.desc);
  }
  if (hint) box.append(h('p', { class: 'hint' }, hint));
  return box;
}

/**
 * Checkbox row whose visible text is a real `<label for>` — clicking the words ticks the box (I2).
 *
 * Returns `{ input, row }`: `input` is what the view reads/writes `.checked` on, `row` is the
 * element it appends (the row already owns the label, the help dot and the optional hint).
 * `onChange` is attached here so the caller keeps no second reference to the input just for it.
 *
 * The help dot sits beside the label, not inside it, for the same reason as in `field()`: the
 * label is a click target, and a `<button>` is both labelable content and a tap target of its own.
 * It keeps the same `data-tip` + `.sr-only` + aria-describedby wiring as `field()`.
 */
export function toggle(labelText, {
  checked = false,
  id = null,
  help = null,
  hint = null,
  disabled = false,
  onChange = null,
} = {}) {
  const inputId = id ?? `t-${++uid}`;
  const input = h('input', { type: 'checkbox', id: inputId });
  input.checked = Boolean(checked);
  if (disabled) input.disabled = true;
  if (onChange) input.addEventListener('change', onChange);
  const aff = helpAffordance(labelText, help);
  const row = h('div', { class: 'check-row' }, input, h('label', { for: inputId }, labelText));
  if (aff) row.className += ' has-help';
  if (aff) {
    describeInput(input, aff.id);
    row.append(aff.btn, aff.desc);
  }
  if (hint) row.append(h('p', { class: 'hint' }, hint));
  return { input, row };
}

/**
 * Copy `text` to the clipboard and report honestly whether it worked.
 *
 * The Clipboard API needs a secure context and can be refused by permission policy; when it is
 * unavailable the fallback is the old selection trick. Returning a boolean rather than throwing
 * is deliberate: every caller here is a UI affordance whose only job is to tell the operator
 * "copied" or "copy it yourself", and neither outcome is an error worth a dialog.
 */
export async function copyText(text) {
  const value = String(text ?? '');
  if (!value) return false;
  try {
    await navigator.clipboard.writeText(value);
    return true;
  } catch {
    // Fallback: a hidden textarea, selected and copied with the legacy command.
    try {
      const ta = document.createElement('textarea');
      ta.value = value;
      ta.setAttribute('readonly', '');
      ta.style.position = 'fixed';
      ta.style.top = '-1000px';
      ta.style.opacity = '0';
      document.body.append(ta);
      ta.select();
      const ok = document.execCommand('copy');
      ta.remove();
      return ok;
    } catch {
      return false;
    }
  }
}

/**
 * A button that copies `text` when pressed, flashing "Copied" on success (I13/Quick Connect).
 *
 * `label` is the resting text; the button is `aria-live`-free on purpose — the toast or the
 * button's own text change is the feedback, and a live region that fires on every copy would be
 * noise for a screen-reader user.
 */
export function copyButton(text, { label = 'Copy', title = null, className = 'btn btn-sm' } = {}) {
  const btn = h('button', { class: className, type: 'button', title }, label);
  btn.addEventListener('click', async () => {
    const ok = await copyText(text);
    btn.textContent = ok ? 'Copied' : 'Copy failed';
    btn.classList.toggle('copy-ok', ok);
    setTimeout(() => {
      btn.textContent = label;
      btn.classList.remove('copy-ok');
    }, 1600);
  });
  return btn;
}

/** Copy a value read at click time, for text that can change while the button is on screen. */
export function copyFrom(read, { label = 'Copy', title = null, className = 'btn btn-sm' } = {}) {
  const btn = h('button', { class: className, type: 'button', title }, label);
  btn.addEventListener('click', async () => {
    const ok = await copyText(read());
    btn.textContent = ok ? 'Copied' : 'Copy failed';
    btn.classList.toggle('copy-ok', ok);
    setTimeout(() => { btn.textContent = label; btn.classList.remove('copy-ok'); }, 1600);
  });
  return btn;
}

/**
 * Render an ApiError as a red block: the human message, then EVERY validation problem in
 * `details` as a list. The backend deliberately returns all problems at once — collapsing them
 * into one line would throw away the feature. All text lands via textContent.
 */
export function errorBlock(err) {
  const box = h('div', { class: 'form-errors', role: 'alert' });
  box.append(h('strong', {}, err?.message || 'The request failed.'));
  const items = err?.details ?? [];
  if (items.length) box.append(detailsList(items));
  return box;
}

/** <ul> of validation problems. Server-supplied strings go through text nodes only. */
export function detailsList(items) {
  const ul = h('ul');
  for (const item of items) {
    const { message, field: f } = problemParts(item);
    ul.append(h('li', {}, f ? `${f}: ${message}` : message));
  }
  return ul;
}

export function emptyState(title, hint = null) {
  return h('div', { class: 'empty-state' }, h('div', {}, title), hint ? h('p', {}, hint) : null);
}

/** Two-column definition list for read-only config facts. */
export function kv(pairs) {
  const dl = h('dl', { class: 'kv' });
  for (const [k, v] of pairs) {
    dl.append(h('dt', {}, k), h('dd', { class: typeof v === 'string' && looksMono(v) ? 'mono' : '' }, v));
  }
  return dl;
}

function looksMono(s) {
  return /^[A-Za-z0-9._:/-]{6,}$/.test(s);
}

// ---------------------------------------------------------------------------
// Validation-problem → field mapping.
// ---------------------------------------------------------------------------

/** Normalise one detail entry (string or {field,message}-ish object) to {field, message}. */
export function problemParts(item) {
  if (typeof item === 'string') return { field: null, message: item };
  if (item && typeof item === 'object') {
    const f = item.field ?? item.path ?? item.param ?? item.key ?? null;
    const m = item.message ?? item.detail ?? item.error ?? (typeof item.toString === 'function' && item.toString !== Object.prototype.toString ? String(item) : 'invalid value');
    return { field: f == null ? null : String(f), message: String(m) };
  }
  return { field: null, message: String(item) };
}

/**
 * True when a problem's field name refers to the input marked with data-field.
 * Exact match, or leaf match: the backend reports deep paths ("hedge.window_ms",
 * "steps[2].model") while forms mark leaf names, so the last path segment matches.
 * First match wins; a mis-mapped problem is still displayed (also in the full list), never lost.
 */
export function fieldMatches(dataField, problemField) {
  if (!problemField) return false;
  if (dataField === problemField) return true;
  const leaf = String(problemField).split(/[.[\]]/).filter(Boolean).pop();
  return leaf != null && dataField === leaf;
}

/**
 * Pin validation problems next to their inputs and list everything in the form's error block.
 * WHY the full list is repeated even when problems were mapped to fields: completeness beats
 * cleverness — a wrong guess in fieldMatches must never hide a problem from the operator.
 * Returns the error block (also appended to `scope`) so callers can scroll to it.
 */
export function applyFieldErrors(scope, err) {
  clearFieldErrors(scope);
  const box = errorBlock(err);
  for (const item of err?.details ?? []) {
    const { field: f, message } = problemParts(item);
    if (!f) continue;
    const input = [...scope.querySelectorAll('[data-field]')].find((el) => fieldMatches(el.dataset.field, f));
    if (!input) continue;
    input.setAttribute('aria-invalid', 'true');
    const p = h('p', { class: 'field-error' }, message);
    (input.closest('.field') ?? input.parentElement ?? input).after(p);
  }
  scope.prepend(box);
  return box;
}

export function clearFieldErrors(scope) {
  scope.querySelectorAll('.field-error').forEach((el) => el.remove());
  scope.querySelectorAll('[aria-invalid]').forEach((el) => el.removeAttribute('aria-invalid'));
  scope.querySelectorAll('.form-errors').forEach((el) => el.remove());
}

/** Standard button row at the bottom of a form. */
export function formActions(...buttons) {
  return h('div', { class: 'dialog-actions' }, ...buttons);
}
