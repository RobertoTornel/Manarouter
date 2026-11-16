// Pins the per-step account picker in the combo editor.
//
// A combo step may name *some* of a provider's accounts instead of all of them, by id or by tag.
// Nothing ticked means every account, and that default must look deliberate rather than broken:
// what is pinned here is
//
//   * the picker lists exactly the step provider's accounts, and says "every account" when empty;
//   * ticking one account and saving sends a step whose accounts is that one id, and nothing else
//     about the step changes;
//   * saving with nothing ticked writes no accounts key at all (one spelling of the default);
//   * a step that already selects by tag keeps the tag (it is a person, not an account id);
//   * the collapsed row carries the count, so a restricted step is visible without opening it.
//
// No jsdom (the panel has no dependencies): the same element stub as delete-account.test.mjs.
//
// Run: node --test panel/test/combo-step-accounts.test.mjs

import test from 'node:test';
import assert from 'node:assert/strict';

function makeEl(tag) {
  const el = {
    nodeType: 1,
    tagName: String(tag).toUpperCase(),
    attrs: {},
    children: [],
    dataset: {},
    listeners: {},
    className: '',
    id: '',
    textContent: '',
    value: undefined,
    style: {},
    checked: false,
    disabled: false,
    hidden: false,
    isConnected: true,
    classList: {
      add() {},
      remove() {},
      toggle() {},
      contains(c) { return String(el.className).split(/\s+/).includes(c); },
    },
    get firstElementChild() { return el.children[0] ?? null; },
    setAttribute(k, v) {
      el.attrs[k] = String(v);
      if (k === 'class') el.className = String(v);
      if (k === 'id') el.id = String(v);
      if (k === 'value') el.value = String(v);
    },
    getAttribute(k) { return Object.hasOwn(el.attrs, k) ? el.attrs[k] : null; },
    removeAttribute(k) { delete el.attrs[k]; },
    addEventListener(type, fn) { (el.listeners[type] ||= []).push(fn); },
    removeEventListener() {},
    append(...kids) { for (const kid of kids) { if (kid) kid.parent = el; el.children.push(kid); } },
    appendChild(kid) { kid.parent = el; el.children.push(kid); return kid; },
    prepend(...kids) { for (const kid of kids) { if (kid) kid.parent = el; el.children.unshift(kid); } },
    replaceChildren(...kids) {
      el.children = kids.filter((kid) => kid != null);
      for (const kid of el.children) if (kid && typeof kid === 'object') kid.parent = el;
    },
    after(node) {
      const parent = el.parent;
      if (!parent) return;
      const at = parent.children.indexOf(el);
      parent.children.splice(at + 1, 0, node);
      if (node && typeof node === 'object') node.parent = parent;
    },
    remove() {
      const parent = el.parent;
      if (!parent) return;
      const at = parent.children.indexOf(el);
      if (at !== -1) parent.children.splice(at, 1);
      el.parent = null;
    },
    focus() {},
    scrollIntoView() {},
    contains(node) {
      if (node === el) return true;
      return el.children.some((kid) => kid && kid.nodeType === 1 && typeof kid.contains === 'function' && kid.contains(node));
    },
    querySelector(sel) { return queryAll(el, sel)[0] ?? null; },
    querySelectorAll(sel) { return queryAll(el, sel); },
  };
  return el;
}

function selectorMatches(node, selector) {
  if (!node || node.nodeType !== 1) return false;
  const sel = selector.trim();
  if (sel.includes(' > ') || sel.includes(',')) return false; // handled by queryAll / not used
  // Attribute selector, with or without a value: `tr[data-provider-id="acme"]`.
  const attrMatch = /^([\w-]+)?\[([\w-]+)(?:="([^"]*)")?\]$/.exec(sel);
  if (attrMatch) {
    const tag = attrMatch[1] ?? '';
    const attrKey = attrMatch[2];
    if (tag && node.tagName !== tag.toUpperCase()) return false;
    if (attrMatch[3] === undefined) return node.getAttribute?.(attrKey) != null;
    const actual = attrKey === 'class' ? node.className : node.getAttribute?.(attrKey);
    return String(actual ?? '') === attrMatch[3];
  }
  if (sel.startsWith('.')) return String(node.className ?? '').split(/\s+/).includes(sel.slice(1));
  if (/^[\w-]+$/.test(sel)) return node.tagName === sel.toUpperCase();
  return false;
}

/** Descendants only (never the node itself), like Element.querySelectorAll. */
function queryAll(node, selector) {
  const out = [];
  const gtIndex = selector.indexOf(' > ');
  if (gtIndex !== -1) {
    const left = selector.slice(0, gtIndex);
    const right = selector.slice(gtIndex + 3);
    const leftMatches = [];
    const find = (n) => {
      if (!n || n.nodeType !== 1) return;
      if (selectorMatches(n, left)) leftMatches.push(n);
      for (const kid of n.children) find(kid);
    };
    for (const kid of node.children) find(kid);
    for (const lm of leftMatches) {
      for (const kid of lm.children ?? []) if (kid && selectorMatches(kid, right)) out.push(kid);
    }
    return out;
  }
  const walk = (n) => {
    for (const kid of n.children) {
      if (kid && selectorMatches(kid, selector)) out.push(kid);
      if (kid && kid.nodeType === 1) walk(kid);
    }
  };
  walk(node);
  return out;
}

// ------------------------------------------------------------------ globals

globalThis.document = {
  createElement: makeEl,
  createElementNS: (_ns, tag) => makeEl(tag),
  createTextNode: (text) => ({ nodeType: 3, textContent: String(text) }),
  body: makeEl('body'),
  documentElement: makeEl('html'),
  activeElement: null,
  addEventListener() {},
  querySelector(sel) { return queryAll(this.body, sel)[0] ?? null; },
  querySelectorAll(sel) { return queryAll(this.body, sel); },
};
globalThis.CSS = { escape: (value) => String(value).replace(/["\\]/g, '\\$&') };
globalThis.window = { addEventListener() {}, open() {} };
globalThis.requestAnimationFrame = (cb) => { cb(0); return 0; };
globalThis.localStorage = { getItem: () => null, setItem() {}, removeItem() {} };

const settle = () => new Promise((resolve) => setTimeout(resolve, 0));
const flush = async (times = 12) => { for (let i = 0; i < times; i += 1) await settle(); };

// ------------------------------------------------------------------ helpers

/** All text under a node, since the stub does not aggregate textContent. */
function textOf(node) {
  if (!node || typeof node !== 'object') return String(node ?? '');
  if (node.nodeType === 3) return String(node.textContent ?? '');
  let out = typeof node.textContent === 'string' ? node.textContent : '';
  for (const kid of node.children ?? []) out += textOf(kid);
  return out;
}

function findOne(node, tag, out = null) {
  if (!node || node.nodeType !== 1) return out;
  if (node.tagName === tag) return node;
  for (const kid of node.children) {
    out = findOne(kid, tag, out);
    if (out) return out;
  }
  return out;
}

/** The first BUTTON whose aria-label starts with `label`. */
function findByAriaLabel(node, label) {
  if (!node || node.nodeType !== 1) return null;
  if (node.tagName === 'BUTTON' && String(node.getAttribute('aria-label') ?? '').startsWith(label)) return node;
  for (const kid of node.children) {
    const hit = findByAriaLabel(kid, label);
    if (hit) return hit;
  }
  return null;
}
// The view polls /api/activity every 5 s for as long as its mount is in the document, and a test
// never unmounts. That poll is a browser concern and no assertion here depends on it, so it is
// neutralised instead of left holding the test process open.
globalThis.setInterval = () => 0;

/** The first BUTTON under `node` whose visible text is exactly `text`. */
function buttonText(node, text) {
  if (!node || node.nodeType !== 1) return null;
  if (node.tagName === 'BUTTON' && textOf(node).trim() === text) return node;
  for (const kid of node.children ?? []) {
    const hit = buttonText(kid, text);
    if (hit) return hit;
  }
  return null;
}

// ------------------------------------------------------------------ fixtures

const PROVIDER = {
  id: 'acme',
  label: 'acme',
  dialect: 'open-ai',
  enabled: true,
  models: [{ id: 'model-a' }],
  accounts: [
    { id: 'acct-a', label: 'Alpha', enabled: true, tags: ['alice'] },
    { id: 'acct-b', label: 'Beta', enabled: true, tags: [] },
  ],
};

const COMBO = {
  id: 'combo-1',
  name: 'deepseek',
  label: 'DeepSeek',
  enabled: true,
  steps: [{ provider: 'acme', model: 'model-a' }],
  strategy: 'priority',
  midstream_policy: 'restart',
  max_retries: 1,
  retry_delay_ms: 0,
  affinity: true,
  hedge: { enabled: false, participants: [], count: 3, window_ms: 800 },
  sticky_success_window_secs: 1800,
};

/**
 * Mount the combo view on one combo, open its step editor, and hand back the row plus every request
 * the panel made, so a test can drive the picker and inspect what was saved.
 */
async function mountStep(step = { provider: 'acme', model: 'model-a' }) {
  const calls = [];
  const combo = { ...COMBO, steps: [step] };
  globalThis.fetch = async (url, init = {}) => {
    const method = init.method ?? 'GET';
    calls.push({ url: String(url), method, body: init.body });
    if (method === 'PUT') return { ok: true, status: 200, json: async () => ({ combo }) };
    if (String(url) === '/api/combos') return { ok: true, status: 200, json: async () => ({ combos: [combo] }) };
    if (String(url) === '/api/activity') return { ok: true, status: 200, json: async () => ({ entries: [] }) };
    return { ok: true, status: 200, json: async () => ({}) };
  };

  const { renderCombos } = await import('../view-combos.js');
  const toasts = { success: [], error: [], warning: [] };
  const mount = makeEl('div');
  const ctx = {
    leaveGuard: null,
    state: { providers: [PROVIDER] },
    loadProviders: async () => [PROVIDER],
    navigate: async () => {},
    confirm: async () => true,
    toast: {
      success: (m) => toasts.success.push(m),
      error: (m) => toasts.error.push(m),
      warning: (m) => toasts.warning.push(m),
    },
  };

  await renderCombos(mount, ctx, 'combo-1');
  await flush();
  document.body.appendChild(mount);

  const openEditor = buttonText(mount, 'Edit combo');
  assert.ok(openEditor, 'the summary should offer Edit combo');
  openEditor.listeners.click[0]();
  await flush();

  const body = queryAll(mount, '.drawer-body')[0];
  assert.ok(body, 'the combo editor drawer should be open');
  const editStep = findByAriaLabel(body, 'Edit step 1');
  assert.ok(editStep, 'the first step should carry its Edit button');
  editStep.listeners.click[0]();
  await flush();

  const wrap = queryAll(body, '.step-edit-wrap')[0];
  assert.ok(wrap, 'the step editor should be open');
  return { mount, body, row: wrap, calls, toasts };
}

function accountBoxes(row) {
  return queryAll(row, 'input').filter((el) => String(el.getAttribute('aria-label') ?? '').startsWith('Use account'));
}

function savedSteps(calls) {
  const put = calls.filter((call) => call.method === 'PUT');
  assert.equal(put.length, 1, 'one save, one PUT');
  return JSON.parse(put[0].body).steps;
}

// ------------------------------------------------------------------ tests

test('the picker lists the step provider’s accounts and starts on every account', async () => {
  const { row } = await mountStep();
  const boxes = accountBoxes(row);
  assert.equal(boxes.length, 2, 'both accounts of the step provider are offered');
  assert.deepEqual(boxes.map((box) => Boolean(box.checked)), [false, false], 'nothing is ticked by default');
  assert.match(textOf(row), /every account/, 'the default is stated, not left as an empty box');
  assert.match(textOf(row), /Nothing ticked means every account/);
});

test('ticking one account saves that id as the step’s selection', async () => {
  const { row, body, calls } = await mountStep();
  const boxes = accountBoxes(row);
  const alpha = boxes.find((box) => String(box.getAttribute('aria-label')).includes('Alpha'));
  alpha.checked = true;
  alpha.listeners.change[0]();
  await flush();

  buttonText(row, 'Done').listeners.click[0]();
  await flush();
  const collapsed = findOne(body, 'OL').children[0];
  assert.match(textOf(collapsed), /1 of 2 accounts/, 'the restriction is visible in the row');

  buttonText(body, 'Save combo').listeners.click[0]();
  await flush();

  const steps = savedSteps(calls);
  assert.deepEqual(steps[0].accounts, ['acct-a']);
  assert.equal(steps[0].provider, 'acme', 'the step is otherwise unchanged');
  assert.equal(steps[0].model, 'model-a');
  assert.deepEqual(calls.filter((call) => call.method === 'PUT').map((call) => call.url), ['/api/combos/combo-1']);
});

test('nothing ticked writes no accounts key at all', async () => {
  const { row, body, calls } = await mountStep();
  // Open the picker, untick nothing, and leave: the saved document keeps one spelling of "all".
  buttonText(row, 'Done').listeners.click[0]();
  await flush();
  buttonText(body, 'Save combo').listeners.click[0]();
  await flush();

  const steps = savedSteps(calls);
  assert.equal(Object.hasOwn(steps[0], 'accounts'), false, 'the default is the absence of a selection');
  const collapsed = findOne(body, 'OL').children[0];
  assert.doesNotMatch(textOf(collapsed), /accounts/);
});

test('a selection written as a tag keeps the tag', async () => {
  const { row, body, calls } = await mountStep({ provider: 'acme', model: 'model-a', accounts: ['alice'] });
  const boxes = accountBoxes(row);
  assert.equal(boxes.filter((box) => box.checked).length, 1, 'the tag ticks the account carrying it');

  buttonText(row, 'Done').listeners.click[0]();
  await flush();
  buttonText(body, 'Save combo').listeners.click[0]();
  await flush();

  assert.deepEqual(savedSteps(calls)[0].accounts, ['alice'], 'a person stays a person');
});

test('a selection matching nothing is flagged rather than silently widened', async () => {
  const { row, body } = await mountStep({ provider: 'acme', model: 'model-a', accounts: ['teammate-2'] });
  assert.equal(accountBoxes(row).filter((box) => box.checked).length, 0);
  assert.match(textOf(row), /No account of this provider matches "teammate-2"/);

  buttonText(row, 'Done').listeners.click[0]();
  await flush();
  const collapsedChip = textOf(findOne(body, 'OL').children[0]);
  assert.match(collapsedChip, /0 of 2 accounts/, 'the row shows the hole rather than widening to all');
});
