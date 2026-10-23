// Contract checks for the "paste the redirect instead" fallback in panel/view-providers.js.
//
// There is no jsdom in this panel and the existing view tests do not load view modules, so this
// file installs the same kind of ~40-line element stub as dom.test.mjs and drives the exported
// component directly. What matters here is the wire contract, not layout:
//
//   * the pasted text is sent verbatim to POST /api/oauth/sessions/{id}/redirect as
//     { provider_id, url }, and nothing else is posted (in particular, no second /complete),
//   * the submit button is disabled while that POST is in flight and re-enabled on failure,
//   * the server's message reaches the form's error block and a retry is possible,
//   * the block starts hidden and the label is wired to the input with a real <label for>.
//
// Run: node --test panel/test/oauth-redirect.test.mjs

import test from 'node:test';
import assert from 'node:assert/strict';

function makeEl(tag) {
  return {
    nodeType: 1,
    tagName: String(tag).toUpperCase(),
    attrs: {},
    children: [],
    listeners: {},
    className: '',
    id: '',
    hidden: false,
    disabled: false,
    value: '',
    textContent: '',
    setAttribute(k, v) {
      this.attrs[k] = String(v);
      if (k === 'class') this.className = String(v);
      if (k === 'id') this.id = String(v);
      if (k === 'hidden') this.hidden = true;
    },
    getAttribute(k) {
      return Object.hasOwn(this.attrs, k) ? this.attrs[k] : null;
    },
    addEventListener(type, fn) {
      (this.listeners[type] ||= []).push(fn);
    },
    append(...kids) {
      for (const kid of kids) this.children.push(kid);
    },
    replaceChildren(...kids) {
      this.children = [];
      this.append(...kids);
    },
    focus() {
      document.activeElement = this;
    },
  };
}

// Installed before view-providers.js is imported: h() and field() touch `document` when called.
globalThis.document = {
  createElement: makeEl,
  createTextNode: (text) => ({ nodeType: 3, textContent: String(text) }),
  body: makeEl('body'),
  documentElement: makeEl('html'),
  activeElement: null,
  addEventListener() {},
};
globalThis.window = { addEventListener() {} };

const { buildRedirectPaste } = await import('../view-providers.js');
const { api } = await import('../api.js');

function walk(node, out = []) {
  out.push(node);
  for (const kid of node.children ?? []) if (kid.nodeType === 1) walk(kid, out);
  return out;
}

const textOf = (node) =>
  node.nodeType === 3 ? node.textContent : (node.children ?? []).map(textOf).join('');

function makeCtx() {
  const warnings = [];
  const errors = [];
  return {
    warnings,
    errors,
    toast: {
      warning: (message) => warnings.push(message),
      error: (message, err) => errors.push({ message, err }),
    },
  };
}

test('a pasted redirect URL is POSTed verbatim to the session redirect endpoint', async () => {
  const calls = [];
  const original = api.post;
  api.post = async (path, body) => {
    calls.push({ path, body });
    return {};
  };
  try {
    const ctx = makeCtx();
    const session = { id: 'sess-1' };
    const ui = buildRedirectPaste({ id: 'codex-work' }, ctx, session);

    assert.equal(ui.block.hidden, true, 'the fallback stays hidden until browser sign-in starts');

    // Accessibility: the visible words are a <label for> pointing at the input.
    const all = walk(ui.block);
    const label = all.find((n) => n.tagName === 'LABEL');
    const input = all.find((n) => n.tagName === 'INPUT');
    assert.ok(label && input);
    assert.equal(label.getAttribute('for'), input.id);
    assert.match(textOf(label), /paste the URL you were redirected to/i);

    ui.show();
    assert.equal(ui.block.hidden, false);

    const url = 'http://127.0.0.1:1455/auth/callback?code=a%2Fb&state=xyz';
    ui.input.value = url;
    const pending = ui.send();
    assert.equal(ui.submit.disabled, true, 'the button is disabled while the POST is in flight');
    await pending;

    assert.deepEqual(calls, [{
      path: '/api/oauth/sessions/sess-1/redirect',
      body: { provider_id: 'codex-work', url },
    }], 'one redirect POST, sent verbatim, and no second /complete');

    ui.hide();
    assert.equal(ui.block.hidden, true, 'the block is hidden again once the flow ends');
    assert.equal(ui.input.value, '', 'the one-time code is dropped from the DOM');
  } finally {
    api.post = original;
  }
});

test('a rejected paste shows the server message and re-enables the button for a retry', async () => {
  const calls = [];
  const original = api.post;
  let fail = true;
  api.post = async (path, body) => {
    calls.push({ path, body });
    if (fail) throw { message: 'The state did not match this sign-in.', details: [] };
    return {};
  };
  try {
    const ctx = makeCtx();
    const ui = buildRedirectPaste({ id: 'antigravity' }, ctx, { id: 'sess-2' });
    ui.show();

    ui.input.value = 'just-the-code';
    await ui.send();
    assert.equal(ui.submit.disabled, false, 'the button is usable again after a rejection');
    assert.match(textOf(ui.block), /The state did not match this sign-in\./);
    assert.equal(ctx.errors.length, 1, 'the server message is also toasted');
    assert.equal(calls[0].body.url, 'just-the-code', 'a bare code is sent as pasted');

    fail = false;
    ui.input.value = 'http://127.0.0.1:1455/auth/callback?code=ok&state=xyz';
    await ui.send();
    assert.equal(calls.length, 2, 'the retry reaches the same endpoint');
    assert.deepEqual(calls[1].body, {
      provider_id: 'antigravity',
      url: 'http://127.0.0.1:1455/auth/callback?code=ok&state=xyz',
    });
  } finally {
    api.post = original;
  }
});

test('an empty paste does nothing, and Enter in the input submits', async () => {
  const calls = [];
  const original = api.post;
  api.post = async (path, body) => {
    calls.push({ path, body });
    return {};
  };
  try {
    const ctx = makeCtx();
    const ui = buildRedirectPaste({ id: 'codex-work' }, ctx, { id: 'sess-3' });
    ui.show();

    await ui.send();
    assert.equal(calls.length, 0);
    assert.equal(ctx.warnings.length, 1);

    ui.input.value = 'http://127.0.0.1:1455/auth/callback?code=ok&state=xyz';
    for (const fn of ui.input.listeners.keydown ?? []) {
      await fn({ key: 'Enter', preventDefault() {} });
    }
    assert.equal(calls.length, 1, 'Enter submits without needing the button');
    assert.equal(calls[0].path, '/api/oauth/sessions/sess-3/redirect');
  } finally {
    api.post = original;
  }
});

test('a sent block ignores further submits until the flow ends', async () => {
  const calls = [];
  const original = api.post;
  api.post = async (path, body) => {
    calls.push({ path, body });
    return {};
  };
  try {
    const ui = buildRedirectPaste({ id: 'codex-work' }, makeCtx(), { id: 'sess-4' });
    ui.show();
    ui.input.value = 'code';
    await Promise.all([ui.send(), ui.send()]);
    assert.equal(calls.length, 1, 'the disabled button doubles as the in-flight guard');
    // After the flow completes the caller hides the block; a fresh run resets it.
    ui.hide();
    ui.show();
    assert.equal(ui.submit.disabled, false);
  } finally {
    api.post = original;
  }
});
