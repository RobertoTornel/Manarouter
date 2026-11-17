// Contract checks for the pasted-code ("paste") sign-in in panel/view-providers.js.
//
// A platform whose redirect is a page on the provider's own origin has nothing that can reach
// this machine: the server binds no port, the person copies `code#state` off that page, and the
// flow is start → paste → complete. That makes it a wire contract with four things worth pinning:
//
//   * Connect POSTs /api/oauth/{platform}/paste with the provider id and opens the returned URL,
//   * the code box appears immediately, because it is the instruction, and it asks for a code,
//   * /complete is called once and resolves when /redirect accepts the paste — the paste does not
//     call /complete itself,
//   * /authorize and /device-code are never tried for a platform that declares only `paste`.
//
// There is no jsdom in this panel, so this file installs the same ~40-line element stub as
// dom.test.mjs and drives the exported form directly.
//
// Run: node --test panel/test/oauth-paste.test.mjs

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
    placeholder: '',
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

const opened = [];
globalThis.document = {
  createElement: makeEl,
  createTextNode: (text) => ({ nodeType: 3, textContent: String(text) }),
  body: makeEl('body'),
  documentElement: makeEl('html'),
  activeElement: null,
  addEventListener() {},
};
globalThis.window = {
  addEventListener() {},
  open(url, target) {
    opened.push({ url, target });
    return { closed: false };
  },
};

const { buildOAuthAccountForm } = await import('../view-providers.js');
const { api } = await import('../api.js');

function walk(node, out = []) {
  out.push(node);
  for (const kid of node.children ?? []) if (kid.nodeType === 1) walk(kid, out);
  return out;
}

const textOf = (node) =>
  node.nodeType === 3 ? node.textContent : (node.children ?? []).map(textOf).join('');

function makeCtx() {
  const successes = [];
  const errors = [];
  return {
    successes,
    errors,
    toast: {
      success: (message) => successes.push(message),
      warning() {},
      error: (message, err) => errors.push({ message, err }),
    },
  };
}

let releaseComplete = null;

function stubApi({ platforms, paste }) {
  const calls = [];
  const original = { post: api.post, get: api.get };
  api.get = async (path) => {
    calls.push({ method: 'GET', path });
    if (path === '/api/oauth') return { platforms };
    throw new Error(`unexpected GET ${path}`);
  };
  api.post = async (path, body) => {
    calls.push({ method: 'POST', path, body });
    if (path.endsWith('/paste')) return paste;
    if (path.endsWith('/complete')) {
      // The real endpoint blocks until the session is finished. The test resolves it only after
      // the paste has been accepted, so the sequence cannot pass by accident.
      return new Promise((resolve) => { releaseComplete = resolve; });
    }
    throw new Error(`unexpected POST ${path}`);
  };
  return {
    calls,
    restore() {
      api.post = original.post;
      api.get = original.get;
    },
  };
}

const claude = { id: 'claude-max', auth: { type: 'oauth', platform: 'claude' } };
const platform = { id: 'claude', label: 'Claude', flow: 'paste', flows: ['paste'] };

async function until(predicate, what) {
  for (let i = 0; i < 500; i += 1) {
    if (predicate()) return;
    await new Promise((resolve) => setImmediate(resolve));
  }
  throw new Error(`timed out waiting for ${what}`);
}

test('connect starts a paste session, opens the code page and finishes on the pasted code', async () => {
  opened.length = 0;
  releaseComplete = null;
  const stub = stubApi({
    platforms: [platform],
    paste: {
      session_id: 'sess-p1',
      authorization_url: 'https://claude.ai/oauth/authorize?code=true&state=s1&code_challenge=cc',
    },
  });
  try {
    const ctx = makeCtx();
    const form = buildOAuthAccountForm(claude, ctx, null);
    const button = walk(form).find((n) => n.tagName === 'BUTTON' && n.className.includes('btn-primary'));
    assert.ok(button, 'the drawer offers a Connect button');

    // Discovery relabels the button before it is pressed: a browser would never open here.
    await until(() => button.textContent === 'Connect and paste the code', 'the paste relabel');
    for (const fn of button.listeners.click ?? []) await fn({ preventDefault() {} });

    await until(() => stub.calls.some((c) => c.path.endsWith('/paste')), 'the paste start');

    const start = stub.calls.find((c) => c.path === '/api/oauth/claude/paste');
    assert.deepEqual(start, {
      method: 'POST',
      path: '/api/oauth/claude/paste',
      body: { provider_id: 'claude-max' },
    }, 'the start call names the platform in the path and the provider in the body');
    assert.deepEqual(opened, [{
      url: 'https://claude.ai/oauth/authorize?code=true&state=s1&code_challenge=cc',
      target: '_blank',
    }], 'the authorize page is opened, and nothing else is');

    // The box is the instruction: it is on screen while /complete is still pending.
    const codeInput = walk(form).find((n) => n.tagName === 'INPUT');
    assert.ok(codeInput && !codeInput.parentElement?.hidden);
    assert.equal(codeInput.placeholder, 'code#state');
    const submit = walk(form).find((n) => n.tagName === 'BUTTON' && textOf(n) === 'Use this code');
    assert.ok(submit, 'the paste action is labelled for a code');

    // The paste goes to /redirect, exactly as pasted, and does not call /complete itself.
    codeInput.value = 'the-code#the-state';
    for (const fn of submit.listeners.click ?? []) await fn({ preventDefault() {} });
    await until(() => stub.calls.some((c) => c.path.endsWith('/redirect')), 'the redirect post');
    assert.deepEqual(stub.calls.find((c) => c.path === '/api/oauth/sessions/sess-p1/redirect'), {
      method: 'POST',
      path: '/api/oauth/sessions/sess-p1/redirect',
      body: { provider_id: 'claude-max', url: 'the-code#the-state' },
    });

    assert.equal(
      stub.calls.filter((c) => c.path.endsWith('/complete')).length,
      1,
      'exactly one /complete was waiting, and the paste did not add another',
    );
    assert.equal(stub.calls.some((c) => c.path.endsWith('/authorize')), false);
    assert.equal(stub.calls.some((c) => c.path.endsWith('/device-code')), false);

    // The server accepts the paste by finishing the session, which resolves the waiting /complete.
    releaseComplete({ account: { label: 'Claude Max' } });
    await until(() => ctx.successes.length > 0, 'the flow to report success');
    assert.deepEqual(ctx.successes, ['Connected Claude Max.']);
    await until(() => walk(form).find((n) => n.tagName === 'INPUT').value === '', 'the code to be dropped');
  } finally {
    stub.restore();
  }
});
