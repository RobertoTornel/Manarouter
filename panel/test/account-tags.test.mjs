// Pins the tags editor in the provider accounts table.
//
// A tag is the operator's only way to say "this account belongs to Alice", and it is load-bearing:
// a combo step may name a tag instead of an account id, and the tag is what makes an account egress
// through that person's machine. What is pinned here:
//
//   * the account table has a Tags column, so tags are visible while scanning accounts;
//   * an account's tags render as text (never markup), and an untagged one says so;
//   * the inline editor round-trips through PUT /api/providers/{id}/accounts/{account_id} with the
//     rest of the account echoed back untouched;
//   * the field is parsed the way the CLI parses it: comma separated, trimmed, de-duplicated;
//   * an empty field clears the tags to an empty array.
//
// No jsdom (the panel has no dependencies): the same element stub as delete-account.test.mjs.
//
// Run: node --test panel/test/account-tags.test.mjs

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
    select() {},
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


// ------------------------------------------------------------------ tests

function taggedProvider(tags) {
  return {
    id: 'acme',
    label: 'acme label',
    dialect: 'open-ai',
    enabled: true,
    accounts: [{ id: 'acme-main', label: 'main', enabled: true, tags }],
    models: [{ id: 'model-a' }],
  };
}

/** Mount the view, open the provider's drawer, and hand back the account table and the tag editor. */
async function mountTags(tags, onRequest = null) {
  const calls = [];
  globalThis.fetch = async (url, init = {}) => {
    calls.push({ url: String(url), method: init.method ?? 'GET', body: init.body });
    if (onRequest) return onRequest({ url: String(url), method: init.method ?? 'GET' });
    return { ok: true, status: 200, json: async () => ({}) };
  };

  const { renderProviders } = await import('../view-providers.js');
  const toasts = { success: [], error: [], warning: [] };
  const mount = makeEl('div');
  const ctx = {
    loadProviders: async () => [taggedProvider(tags)],
    onHealth: (cb) => { cb({ snapshot: { digest: '1111111111111111' } }); return () => {}; },
    toast: {
      success: (m) => toasts.success.push(m),
      error: (m) => toasts.error.push(m),
      warning: (m) => toasts.warning.push(m),
    },
    confirm: async () => true,
  };

  await renderProviders(mount, ctx);
  await settle();
  document.body.appendChild(mount);

  const row = findOne(mount, 'TBODY')?.children
    ?.find((tr) => tr?.getAttribute?.('data-provider-id') === 'acme');
  assert.ok(row, 'row for acme should exist');
  row.querySelector('.provider-open').listeners.click[0]({ stopPropagation() {} });
  await flush();

  const body = queryAll(mount, '.drawer-body')[0];
  assert.ok(body, 'the provider detail drawer should be open');
  return { body, calls, toasts, mount };
}

const tagEdits = (calls) => calls.filter((call) => call.method === 'PUT' && call.url.includes('/accounts/'));

test('the account table carries a Tags column', async () => {
  const { body } = await mountTags([]);
  const heads = queryAll(body, 'th').map((th) => textOf(th).trim());
  assert.ok(heads.includes('Tags'), `expected a Tags column, got ${JSON.stringify(heads)}`);
});

test('tags render as text, and an untagged account says so', async () => {
  const { body } = await mountTags(['alice', 'work']);
  assert.match(textOf(body), /alice/);
  assert.match(textOf(body), /work/);

  const empty = await mountTags([]);
  assert.match(textOf(empty.body), /no tags/);
});

test('saving from the inline editor sends the parsed tags and the account unchanged', async () => {
  const { body, calls, toasts } = await mountTags(['alice']);
  const edit = findByAriaLabel(body, 'Edit tags for account acme-main');
  assert.ok(edit, 'the account row should carry its tag editor');

  edit.listeners.click[0]();
  const input = queryAll(body, 'input').find((el) => String(el.getAttribute('aria-label')).startsWith('Tags for account'));
  assert.ok(input, 'the editor should open a text field');
  assert.equal(input.value, 'alice', 'the editor starts from the tags the account has');

  input.value = '  Alice , work,, alice ';
  const save = buttonText(input.parent, 'Save');
  save.listeners.click[0]();
  await flush();

  const put = tagEdits(calls);
  assert.equal(put.length, 1, 'one PUT, not one per tag');
  assert.equal(put[0].url, '/api/providers/acme/accounts/acme-main');
  const sent = JSON.parse(put[0].body);
  assert.deepEqual(sent.tags, ['Alice', 'work'], 'trimmed, de-duplicated case-insensitively, order kept');
  assert.equal(sent.label, 'main', 'the rest of the account is echoed back untouched');
  assert.equal(sent.id, 'acme-main');
  assert.deepEqual(toasts.error, []);
  assert.deepEqual(toasts.success, ['Tags saved.']);
});

test('an empty field clears the tags', async () => {
  const { body, calls, toasts } = await mountTags(['alice', 'work']);
  findByAriaLabel(body, 'Edit tags for account acme-main').listeners.click[0]();
  const input = queryAll(body, 'input').find((el) => String(el.getAttribute('aria-label')).startsWith('Tags for account'));
  input.value = '   ';
  buttonText(input.parent, 'Save').listeners.click[0]();
  await flush();

  const put = tagEdits(calls);
  assert.equal(put.length, 1);
  assert.deepEqual(JSON.parse(put[0].body).tags, [], 'clearing is an empty list, not a missing key');
  assert.deepEqual(toasts.success, ['Tags cleared.']);
});

test('a failed save keeps the editor open and reports the server message', async () => {
  const { body, toasts } = await mountTags(['alice'], () => ({
    ok: false,
    status: 400,
    json: async () => ({ error: { code: 'config_parse', message: 'the account could not be read' } }),
  }));
  findByAriaLabel(body, 'Edit tags for account acme-main').listeners.click[0]();
  const input = queryAll(body, 'input').find((el) => String(el.getAttribute('aria-label')).startsWith('Tags for account'));
  input.value = 'bob';
  buttonText(input.parent, 'Save').listeners.click[0]();
  await flush();

  assert.equal(toasts.error.length, 1, 'the failure must be reported');
  assert.match(String(toasts.error[0]), /could not save the account tags/i);
  assert.ok(queryAll(body, 'input').some((el) => el.value === 'bob'), 'the typed text is not lost');
});
