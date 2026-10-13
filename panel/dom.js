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

/** Labelled form field: <div class=field><label for>..</label><input>…</div>. */
export function field(labelText, inputEl, { hint = null, id = null } = {}) {
  const inputId = id ?? `f-${++uid}`;
  inputEl.id = inputId;
  const box = h('div', { class: 'field' }, h('label', { for: inputId }, labelText), inputEl);
  if (hint) box.append(h('p', { class: 'hint' }, hint));
  return box;
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
