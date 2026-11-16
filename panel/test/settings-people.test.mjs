// Pins the Settings → People card, which is how a tag becomes an egress route.
//
// The card exists because a tag is only a note until something says which machine it means, and an
// operator who gets that wrong sends a person's traffic out of the wrong address with no error to
// show for it. What is pinned here is
//
//   * the join: a person's row says which accounts currently carry their tag, because "routes
//     nothing" is the failure that otherwise looks like a correctly filled-in form;
//   * saving writes the whole authored document back (PUT /api/config/raw) with `people` patched
//     and every other key untouched — the card must not be able to drop a secret reference by
//     round-tripping a redacted view;
//   * a person with no tag or no proxy is refused in the card, with no request sent;
//   * two people sharing a tag are refused in the card, naming the tag.
//
// No jsdom (the panel has no dependencies): the same element stub as delete-account.test.mjs.
//
// Run: node --test panel/test/settings-people.test.mjs

import test from 'node:test';
import assert from 'node:assert/strict';

/** Settings appends plain strings as well as elements; the stub stores them as text nodes. */
function asNode(value) {
  if (value == null || typeof value === 'object') return value == null ? null : value;
  return { nodeType: 3, textContent: String(value), parent: null, children: [] };
}

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
    append(...kids) {
      for (const raw of kids) {
        const kid = asNode(raw);
        if (kid) kid.parent = el;
        el.children.push(kid);
      }
    },
    appendChild(raw) {
      const kid = asNode(raw);
      kid.parent = el;
      el.children.push(kid);
      return kid;
    },
    prepend(...kids) {
      for (const raw of kids) {
        const kid = asNode(raw);
        if (kid) kid.parent = el;
        el.children.unshift(kid);
      }
    },
    replaceChildren(...kids) {
      el.children = kids.map(asNode).filter((kid) => kid != null);
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
  // A real <select>'s value is the selected option's; the stub left it undefined, so a card that
  // reads it back before saving (as the people card does) would see a blank where the browser
  // shows a proxy. Computed on read, exactly as the browser does.
  if (el.tagName === 'SELECT') {
    Object.defineProperty(el, 'value', {
      get() {
        if (el.explicitValue !== undefined) return el.explicitValue;
        const options = (el.children ?? []).filter((kid) => kid?.tagName === 'OPTION');
        const chosen = options.find((kid) => Object.hasOwn(kid.attrs ?? {}, 'selected')) ?? options[0];
        return chosen ? String(chosen.attrs?.value ?? '') : '';
      },
      set(next) { el.explicitValue = String(next); },
      configurable: true,
    });
  }
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
// The view polls or timers in other cards would otherwise hold the process open; nothing asserted
// here depends on them.
globalThis.setInterval = () => 0;

/** The first element under `node` whose aria-label is exactly `label`. */
function byAriaLabel(node, label, out = null) {
  if (!node || node.nodeType !== 1) return out;
  if (String(node.getAttribute?.('aria-label') ?? '') === label) return node;
  for (const kid of node.children ?? []) {
    out = byAriaLabel(kid, label, out);
    if (out) return out;
  }
  return out;
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

/** Mount Settings against one authored configuration and record every write. */
async function mountSettings(config) {
  const writes = [];
  globalThis.fetch = async (url, init = {}) => {
    const path = String(url);
    const method = init.method ?? 'GET';
    if (path === '/api/config/raw' && method === 'GET') {
      return { ok: true, status: 200, text: async () => JSON.stringify(config) };
    }
    if (path === '/api/config/raw' && method === 'PUT') {
      writes.push(JSON.parse(init.body));
      return { ok: true, status: 200, json: async () => ({ config }) };
    }
    if (path === '/api/config/validate') {
      return { ok: true, status: 200, json: async () => ({ problems: [] }) };
    }
    if (path === '/api/ports') {
      return {
        ok: true,
        status: 200,
        json: async () => ({
          relay: { host: '127.0.0.1', port: 20900, running_port: 20900 },
          control: { host: '127.0.0.1', port: 20901, running_port: 20901 },
        }),
      };
    }
    return { ok: true, status: 200, json: async () => ({}) };
  };

  const { renderSettings } = await import('../view-settings.js');
  const toasts = { success: [], error: [], warning: [] };
  const mount = makeEl('div');
  const ctx = {
    state: {},
    toast: {
      success: (m) => toasts.success.push(m),
      error: (m) => toasts.error.push(m),
      warning: (m) => toasts.warning.push(m),
    },
    loadProviders: async () => [],
    refreshHealth: async () => {},
    refreshPorts: async () => {},
    confirm: async () => true,
    restartFlow: async () => {},
  };
  await renderSettings(mount, ctx);
  await flush();
  document.body.appendChild(mount);

  const heading = queryAll(mount, 'H2').find((el) => textOf(el) === 'People and their machines');
  assert.ok(heading, 'the People card should be on the page');
  const card = heading.parent;
  return { card, writes, toasts };
}

const CONFIG = {
  proxies: [{ id: 'ts', kind: 'socks5h', url: 'socks5h://100.64.0.7:1080' }],
  providers: [{
    id: 'acme',
    accounts: [{ id: 'acct-a', label: 'Alpha', tags: ['alice'] }],
  }],
  people: [],
};

// ------------------------------------------------------------------ tests

test('a person’s row names the accounts that carry their tag', async () => {
  const { card } = await mountSettings({ ...CONFIG, people: [{ tag: 'alice', proxy: 'ts' }] });
  assert.match(textOf(card), /Tagged accounts: Alpha \(acme\)/);

  const tag = byAriaLabel(card, 'Person 1 tag');
  tag.value = 'nobody';
  tag.listeners.input[0]();
  await flush();
  assert.match(textOf(card), /No account carries this tag yet/);
});

test('saving a person patches people and leaves the rest of the file alone', async () => {
  const { card, writes, toasts } = await mountSettings(CONFIG);
  buttonText(card, 'Add person').listeners.click[0]();
  await flush();

  const tag = byAriaLabel(card, 'Person 1 tag');
  tag.value = 'alice';
  tag.listeners.input[0]();
  buttonText(card, 'Save people').listeners.click[0]();
  await flush();

  assert.equal(writes.length, 1, 'one save, one write');
  assert.deepEqual(writes[0].people, [{ tag: 'alice', label: null, proxy: 'ts' }],
    'the single existing proxy is the one proposed, and nothing else was invented');
  assert.deepEqual(writes[0].proxies, CONFIG.proxies, 'the proxy list survived the round trip');
  assert.deepEqual(writes[0].providers, CONFIG.providers, 'the accounts survived the round trip');
  assert.deepEqual(toasts.success, ['People saved.']);
});

test('a person with no proxy is refused before any request', async () => {
  const { card, writes } = await mountSettings({ proxies: [], providers: [], people: [] });
  buttonText(card, 'Add person').listeners.click[0]();
  await flush();

  byAriaLabel(card, 'Person 1 tag').value = 'alice';
  buttonText(card, 'Save people').listeners.click[0]();
  await flush();

  assert.equal(writes.length, 0, 'a list the server would reject is not sent');
  assert.match(textOf(card), /Each person needs a tag and a proxy/);
});

test('two people cannot share a tag', async () => {
  const { card, writes } = await mountSettings(CONFIG);
  for (let i = 0; i < 2; i += 1) {
    buttonText(card, 'Add person').listeners.click[0]();
    await flush();
  }
  for (const ariaLabel of ['Person 1 tag', 'Person 2 tag']) {
    const tag = byAriaLabel(card, ariaLabel);
    tag.value = 'Alice';
    tag.listeners.input[0]();
  }
  buttonText(card, 'Save people').listeners.click[0]();
  await flush();

  assert.equal(writes.length, 0, 'the card says which row, so the server never has to');
  assert.match(textOf(card), /Two people share the tag "Alice"/);
});
