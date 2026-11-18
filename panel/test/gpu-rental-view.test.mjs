// Behavioural tests for the GPU rentals view (panel/view-gpu-rentals.js).
//
// What this file is here to pin, in the operator's terms rather than by grepping the source:
//
//   * the inventory renders the two tables the operator actually reads (rental accounts, deployments)
//     and the GPU catalogue for the selected platform;
//   * a credential is shown as a fingerprint and never as a value;
//   * `(Paid)` appears on exactly one card, and only for `free_plan: "unavailable"` — never for
//     `"unknown"`, which is a field nobody has looked at, not a price the platform stated;
//   * the plan limits are quoted with their source, because "10 GPU concurrency" is the number that
//     decides whether a deployment is possible;
//   * the generated app file is a text node inside <pre>, so the app text can never become markup;
//   * the panel says out loud that Modal publishes no deployment REST API;
//   * every entry of error.details reaches the screen;
//   * a hidden tab stops asking for usage numbers;
//   * the quant selector stays blank until a model repository is named (requirement 5).
//
// No jsdom (the panel has no dependencies): the same element stub as
// panel/test/providers-refresh.test.mjs, extended with real classList/querySelector handling because
// this view is inspected through `[data-field]`, `.field-error` and `.form-errors` in setup mode.
//
// Run: node --test panel/test/gpu-rental-view.test.mjs

import test from 'node:test';
import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';

const realSetTimeout = globalThis.setTimeout;
const realClearTimeout = globalThis.clearTimeout;

// ---------------------------------------------------------------------------
// DOM stub
// ---------------------------------------------------------------------------

function classesOf(el) {
  return String(el.className || '').split(/\s+/).filter(Boolean);
}

function setClasses(el, list) {
  el.className = list.join(' ');
}

function makeEl(tag) {
  const el = {
    nodeType: 1,
    tagName: String(tag).toUpperCase(),
    attrs: {},
    children: [],
    parentNode: null,
    dataset: {},
    listeners: {},
    className: '',
    id: '',
    // Real DOM: assigning textContent replaces every child. The plain property this stub used to
    // have hid the status line behind its initial text node, so the getter/setter below is
    // deliberate: a view that names the failing endpoint must be observably different from one
    // that does not.
    _text: '',
    value: undefined,
    style: {},
    checked: false,
    disabled: false,
    isConnected: true,
    get childNodes() { return el.children; },
    classList: {
      add(...names) {
        const cur = classesOf(el);
        for (const n of names) if (!cur.includes(n)) cur.push(n);
        setClasses(el, cur);
      },
      remove(...names) { setClasses(el, classesOf(el).filter((n) => !names.includes(n))); },
      contains(name) { return classesOf(el).includes(name); },
      toggle(name, force) {
        const has = classesOf(el).includes(name);
        const want = force === undefined ? !has : Boolean(force);
        if (want) el.classList.add(name); else el.classList.remove(name);
        return want;
      },
    },
    setAttribute(k, v) {
      el.attrs[k] = String(v);
      if (k === 'class') el.className = String(v);
      if (k === 'id') el.id = String(v);
      if (k === 'value') el.value = String(v);
    },
    getAttribute(k) { return Object.hasOwn(el.attrs, k) ? el.attrs[k] : null; },
    hasAttribute(k) { return Object.hasOwn(el.attrs, k); },
    removeAttribute(k) { delete el.attrs[k]; },
    addEventListener(type, fn) { (el.listeners[type] ||= []).push(fn); },
    removeEventListener(type, fn) {
      el.listeners[type] = (el.listeners[type] || []).filter((f) => f !== fn);
    },
    append(...kids) { el.insert(kids, el.children.length); },
    appendChild(kid) { el.insert([kid], el.children.length); return kid; },
    prepend(...kids) { el.insert(kids, 0); },
    replaceChildren(...kids) { el.children = []; el._text = ''; el.insert(kids, 0); },
    insert(kids, at) {
      const fresh = kids.filter((k) => k !== null && k !== undefined);
      for (const kid of fresh) {
        if (typeof kid === 'object') kid.parentNode = el;
      }
      el.children.splice(at, 0, ...fresh);
    },
    remove() {
      const parent = el.parentNode;
      if (!parent) return;
      parent.children = parent.children.filter((kid) => kid !== el);
      el.parentNode = null;
    },
    after(node) {
      const parent = el.parentNode;
      if (!parent) return;
      if (typeof node === 'object') node.parentNode = parent;
      parent.children.splice(parent.children.indexOf(el) + 1, 0, node);
    },
    focus() {},
    blur() {},
    contains(node) {
      if (node === el) return true;
      return el.children.some((kid) => kid && kid.nodeType === 1
        && typeof kid.contains === 'function' && kid.contains(node));
    },
    closest(sel) {
      let cur = el;
      while (cur) {
        if (matches(cur, sel)) return cur;
        cur = cur.parentNode;
      }
      return null;
    },
    querySelector(sel) { return descendants(el).find((n) => matches(n, sel)) ?? null; },
    querySelectorAll(sel) { return descendants(el).filter((n) => matches(n, sel)); },
  };
  Object.defineProperty(el, 'textContent', {
    configurable: true,
    get() {
      if (!el.children.length) return el._text;
      return el.children.map((kid) => (kid.nodeType === 3
        ? String(kid.data ?? '')
        : String(kid.textContent ?? ''))).join('');
    },
    set(v) {
      el._text = v === null || v === undefined ? '' : String(v);
      el.children = [];
    },
  });
  return el;
}

function descendants(root) {
  const out = [];
  const walk = (node) => {
    for (const kid of node.children ?? []) {
      if (!kid || kid.nodeType !== 1) continue;
      out.push(kid);
      walk(kid);
    }
  };
  walk(root);
  return out;
}

/** Just enough selector support: tag, .class, [attr], [attr="value"], and their combination. */
function matches(el, selector) {
  let sel = String(selector).trim();
  const attr = sel.match(/\[([^\]="]+)(?:="([^"]*)")?\]/);
  if (attr) {
    if (attr[2] === undefined) {
      if (!el.hasAttribute(attr[1])) return false;
    } else if (el.getAttribute(attr[1]) !== attr[2]) {
      return false;
    }
    sel = sel.replace(attr[0], '');
  }
  for (const cls of sel.match(/\.[A-Za-z0-9_-]+/g) ?? []) {
    if (!classesOf(el).includes(cls.slice(1))) return false;
  }
  const tag = sel.replace(/\.[A-Za-z0-9_-]+/g, '').trim();
  if (tag && tag !== '*' && el.tagName !== tag.toUpperCase()) return false;
  return true;
}

const winListeners = {};

globalThis.document = {
  createElement: makeEl,
  // The dialogs in this view append to #dialog-root when the shell provides one and to the body
  // otherwise. The stub has no shell, so every window in these tests lands on the body.
  getElementById: () => null,
  createElementNS: (_ns, tag) => makeEl(tag),
  createTextNode: (text) => ({ nodeType: 3, data: String(text), textContent: String(text) }),
  body: makeEl('body'),
  documentElement: makeEl('html'),
  activeElement: null,
  // Hidden by default: the poller then arms no timer at all, so no test leaves a live 15 s timer
  // behind. The polling test opts into 'visible' explicitly.
  visibilityState: 'hidden',
  addEventListener() {},
  removeEventListener() {},
};
globalThis.window = {
  addEventListener(type, fn) { (winListeners[type] ||= []).push(fn); },
  removeEventListener(type, fn) {
    winListeners[type] = (winListeners[type] || []).filter((f) => f !== fn);
  },
};
globalThis.localStorage = { getItem: () => null, setItem() {}, removeItem() {} };

const { api } = await import('../api.js');
const { render, quantGate } = await import('../view-gpu-rentals.js');

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

// A credential value that must never appear in the DOM, in full.
const TOKEN_SECRET = 'modal-tok-ABCDEFGH1234567890';
const TOKEN_FINGERPRINT = 'f8a1c2d3e4b5a6978877665544332211';

const BUNDLE_TEXT = [
  'import modal',
  '',
  'app = modal.App("qwen3-vllm")',
  '',
  '# <b>not markup</b> — the panel must render this as text',
  '@app.function(gpu="RTX-PRO-6000")',
  'def serve():',
  '    return ...',
].join('\n');

// The probe reply the setup model consumes. One in-repo quant is deliberate: applyProbe() then
// selects it, so the populated selector has an observable value rather than only a list.
const PROBE = {
  repo: 'Qwen/Qwen3-32B',
  revision: 'main',
  sha: 'f8a1c2d3e4b5a6978877665544332211',
  gated: { state: 'open', needs_token: false },
  arch: {
    num_hidden_layers: 64,
    num_key_value_heads: 8,
    head_dim: 128,
    max_position_embeddings: 262144,
  },
  params: { total: 27_000_000_000, source: 'config', agrees_with_name: true },
  quants: [{ id: 'fp8', label: 'FP8', in_repo: true, bytes_per_param: 1 }],
  warnings: [],
};

function modalPlatform() {
  return {
    id: 'modal',
    label: 'Modal',
    docs_url: 'https://modal.com/pricing',
    deployment: {
      programmatic: false,
      explain: 'Modal is serverless: you hand it an app file, not an HTTP deployment call.',
      cli: 'modal deploy modal_app.py',
      sdk: 'modal.App(...)',
    },
    max_concurrent_containers: { free: 10, source: 'observed' },
    plan_limits: [
      {
        id: 'starter', label: 'Starter', usd_per_month: 0, included_credits_usd: 30,
        gpu_concurrency: 10, seats: 3, source: 'observed', source_url: 'https://modal.com/pricing',
      },
      {
        id: 'team', label: 'Team', usd_per_month: 250, included_credits_usd: 100,
        gpu_concurrency: 50, seats: 5, source: 'observed', source_url: 'https://modal.com/pricing',
      },
    ],
    gpus: [
      {
        id: 'h100', label: 'H100', vram_gib: 80, vram_bytes: 85899345920,
        price_usd_per_hour: 3.95, price_usd_per_second: 0.0011,
        free_plan: 'unavailable', source: 'observed', checked_at: '2026-11-01T10:00:00Z',
        source_url: 'https://modal.com/pricing',
      },
      {
        id: 'rtx-pro-6000', label: 'RTX PRO 6000', vram_gib: 95, vram_bytes: 102005473280,
        price_usd_per_hour: 2.5, price_usd_per_second: 0.0007,
        free_plan: 'unknown', source: 'unknown',
      },
      {
        id: 'l40s', label: 'L40S', vram_gib: 46, vram_bytes: 49392123904,
        price_usd_per_hour: 1.95, price_usd_per_second: 0.0005,
        free_plan: 'available', source: 'observed', checked_at: '2026-11-01T10:00:00Z',
      },
    ],
  };
}

function catalog() {
  return {
    platforms: [modalPlatform()],
    // The live catalog records a width for every quant and cache dtype it lists; without one the
    // sizing cannot compute a weight or cache figure at all, which is a different verdict from
    // "this does not fit".
    quant_methods: [
      { id: 'fp8', label: 'FP8', bytes_per_param: 1, min_compute_cap: 8.9 },
      { id: 'awq', label: 'AWQ', bytes_per_param: 0.5, min_compute_cap: 8.9 },
    ],
    kv_cache_dtypes: [
      { id: 'auto', label: 'auto' },
      { id: 'fp8', label: 'fp8', bytes_per_element: 1, min_compute_cap: 8.9 },
    ],
  };
}

function rentals() {
  return {
    accounts: [{
      id: 'mana-modal',
      label: 'Mana Modal',
      platform: 'modal',
      // `plan` is the billing classification the backend records, not a plan-tier id.
      plan: 'paid',
      effective_plan: 'paid',
      plan_is_known: true,
      secret_ref: 'rental:mana-modal',
      credential_fingerprint: TOKEN_FINGERPRINT,
      has_credential: true,
      max_concurrent_containers: 10,
      usage: null,
      usage_unavailable: 'the GPU usage ledger aggregates every provider\'s records for a window '
        + 'and has no per-account filter, so this account\'s share of the totals cannot be attributed',
    }],
    // The conductor reports one aggregate for the whole instance, not a number per account.
    usage: {
      from_millis: 0,
      to_millis: 1762000000000,
      gpu_requests: 42,
      gpu_seconds: 3720,
      cost_usd: 12.5,
      scope: 'instance',
    },
    // A provider with a malformed rental block is dropped by the conductor and named here.
    errors: ['provider "broken-vllm": rental.gpu_count must be between 1 and 8'],
    deployments: [{
      provider_id: 'modal-vllm',
      account_id: 'mana-modal',
      enabled: true,
      deployment: {
        repo: 'Qwen/Qwen3-32B',
        gpu: 'rtx-pro-6000',
        gpu_count: 1,
        weight_quant: 'awq',
        kv_cache_dtype: 'fp8',
        max_model_len: 32768,
        max_num_seqs: 8,
        shutdown_cooldown_secs: 120,
        mode: 'batch',
      },
      sizing: {
        method: 'analytic',
        confidence: 'analytic',
        kv_bytes_per_token: 147456,
        kv_pool_tokens: 1777758,
        simultaneous_sessions_at_max_len: 4,
        assumptions: ['weights fit in the card at 4-bit'],
        warnings: [],
      },
      problems: [{ field: 'max_model_len', message: 'does not fit the chosen card' }],
      hardware_problems: [{ gpu: 'rtx-pro-6000', message: '96 GiB is below the KV pool floor' }],
      bundle: {
        filename: 'modal_app.py',
        text: BUNDLE_TEXT,
        command: 'modal deploy modal_app.py',
        notes: ['Run this from the repository root.'],
      },
    }],
  };
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function textOf(node) {
  if (node === null || node === undefined) return '';
  if (node.nodeType === 3) return String(node.data ?? node.textContent ?? '');
  const kids = node.children ?? [];
  if (kids.length) return kids.map(textOf).join(' ');
  return String(node.textContent ?? '');
}

const settle = async (rounds = 3) => {
  for (let i = 0; i < rounds; i++) {
    await new Promise((resolve) => realSetTimeout(resolve, 0));
  }
};

/** Mount the real view with a stub API and a stub ctx, and record every request. */
async function mountView({ param = null, handlers = {}, onPost = null } = {}) {
  const calls = [];
  const posts = [];
  const navigations = [];
  const originalGet = api.get;
  const originalPost = api.post;
  api.get = async (path) => {
    calls.push(path);
    if (!Object.hasOwn(handlers, path)) throw new Error(`test did not expect GET ${path}`);
    const reply = handlers[path];
    if (reply instanceof Error) throw reply;
    return reply;
  };
  api.post = async (path, body) => {
    posts.push({ path, body });
    if (typeof onPost !== 'function') throw new Error(`test did not expect POST ${path}`);
    const reply = await onPost(path, body);
    if (reply instanceof Error) throw reply;
    return reply;
  };
  const onError = [];
  const mount = makeEl('div');
  const ctx = {
    state: {},
    navigate(hash) { navigations.push(hash); },
    toast: { success() {}, warning() {}, error() {}, info() {} },
    confirm: async () => true,
    prompt: async () => null,
    onError(fn) { onError.push(fn); },
  };
  try {
    await render(mount, ctx, param);
  } finally {
    api.get = originalGet;
    api.post = originalPost;
  }
  return { mount, calls, posts, navigations, ctx };
}

function findButton(root, label) {
  const wanted = String(label).trim();
  const button = descendants(root).find((el) => el.tagName === 'BUTTON'
    && String(el.textContent ?? '').trim() === wanted);
  assert.ok(button, `expected a button labelled ${JSON.stringify(wanted)}`);
  return button;
}

// The account step is the flow's only write before the end, so its forward button names the write.
// Every other step, and the review step, keep their own label.
function advanceButton(root) {
  const labels = ['Save account', 'Save rental', 'Continue'];
  const button = descendants(root).find((el) => el.tagName === 'BUTTON'
    && labels.includes(String(el.textContent ?? '').trim()));
  assert.ok(button, 'expected a forward button on the step');
  return button;
}

function fire(el, type) {
  assert.ok(el, 'cannot dispatch an event on a missing element');
  const listeners = el.listeners?.[type] ?? [];
  assert.ok(listeners.length, `no ${type} listener on <${el.tagName?.toLowerCase()}>`);
  for (const fn of listeners) fn({ type, target: el, currentTarget: el });
}

function setInput(root, name, value) {
  const input = root.querySelector(`[data-field="${name}"]`);
  assert.ok(input, `expected a control for ${name}`);
  input.value = String(value);
  fire(input, 'input');
}

function setSelect(root, name, value) {
  const select = root.querySelector(`[data-field="${name}"]`);
  assert.ok(select, `expected a select for ${name}`);
  select.value = String(value);
  fire(select, 'change');
}

/** Hold the GET stub while a dialog loads: dialogs read their sources after mountView's own stub
 *  has been restored, so a test that opens one must keep the handlers alive itself. */
async function withGet(reply, fn) {
  const original = api.get;
  api.get = async (path) => {
    const value = typeof reply === 'function' ? reply(path) : Object.hasOwn(reply, path) ? reply[path] : undefined;
    if (value === undefined) throw new Error(`test did not expect GET ${path}`);
    if (value instanceof Error) throw value;
    return value;
  };
  try {
    await fn();
  } finally {
    api.get = original;
  }
}

async function withPost(reply, fn) {
  const original = api.post;
  const posts = [];
  api.post = async (path, body) => {
    posts.push({ path, body });
    return typeof reply === 'function' ? reply(path, body) : reply;
  };
  try {
    await fn(posts);
  } finally {
    api.post = original;
  }
}

/** render() deliberately keeps the draft at module scope across route renders, so a test that
 *  wants a clean flow must visit the inventory first. */
async function resetSetup() {
  await mountView({ handlers: standardHandlers() });
}

// A two-lane slice of GET /api/lanes: one host that answers requests, one that cannot and says so
// by quoting the platform. The real route returns five; two are enough to prove both branches.
const lanes = () => ({
  lanes: [
    {
      kind: 'modal',
      id: 'modal',
      display: 'Modal',
      shape: 'serving',
      inbound: 'public-endpoint',
      submission: 'cli',
      headline: 'Serverless GPUs behind proxy tokens.',
      can_serve_requests: true,
      can_run_batch: true,
      refusals: { synchronous: [], batch: [] },
      sources: ['https://modal.com/docs/guide/gpu.md', 'https://modal.com/pricing'],
    },
    {
      kind: 'kaggle',
      id: 'kaggle',
      display: 'Kaggle Notebooks',
      shape: 'batch',
      inbound: 'none',
      submission: 'kernel-push',
      headline: 'Batch only: a submitted kernel returns files, never a live response.',
      can_serve_requests: false,
      can_run_batch: true,
      refusals: {
        synchronous: [{
          code: 'lane_is_batch_only',
          message: 'Kaggle runs submitted kernels and returns files; it has no endpoint to answer a request.',
          clause: null,
          source: null,
        }],
        batch: [{
          code: 'multiple_accounts_forbidden',
          message: 'A second Kaggle account is prohibited by the Terms of Service.',
          clause: 'You also may not have, control, or operate under more than one active Kaggle account',
          source: 'https://www.kaggle.com/terms',
        }],
      },
      sources: ['https://www.kaggle.com/terms', 'https://www.kaggle.com/docs/notebooks'],
    },
  ],
  note: 'Policy facts read from each platform.'
});

// Every endpoint the inventory reads on mount, in the order it asks. Kaggle is the one lane left
// after Molab, Saturn Cloud and Cerebrium were removed from the panel.
const ENDPOINTS = [
  '/api/gpu/rentals',
  '/api/gpu/catalog',
  '/api/lanes',
  '/api/gpu/lanes/kaggle/status',
];

// The lane status shaped like the conductor's answer: readiness and per-account health.
const laneStatuses = () => ({
  '/api/gpu/lanes/kaggle/status': {
    ready: true,
    accounts: [{ account: 'kaggle-work', host: 'kaggle.com', health: 'healthy', failure_count: 0, warnings: [] }],
  },
});

const standardHandlers = () => ({
  '/api/gpu/rentals': rentals(),
  '/api/gpu/catalog': catalog(),
  '/api/lanes': lanes(),
  ...laneStatuses(),
});

// ---------------------------------------------------------------------------
// Inventory: what the operator reads
// ---------------------------------------------------------------------------

test('inventory renders the account table with a fingerprint and never the secret', async () => {
  const { mount, calls } = await mountView({ handlers: standardHandlers() });
  const text = textOf(mount);

  assert.deepEqual(calls, ENDPOINTS,
    'the view reads the inventory and every lane status on mount');
  assert.match(text, /Rental accounts/);
  assert.match(text, /Mana Modal/);
  assert.match(text, /mana-modal/, 'the slug is shown when it differs from the label');
  assert.match(text, /Starter/, 'the documented plan tier is quoted from the catalogue');
  assert.match(text, /paid/, 'the account billing classification is not mangled');
  assert.match(text, /GPU concurrency/);
  assert.match(text, /10 GPUs/, 'the starter concurrency figure is quoted');
  assert.match(text, /Instance usage/, 'the ledger total is shown, and labelled as instance-wide');
  assert.match(text, /Usage is reported as one instance-wide total: the sum of each rental's own account/);
  assert.match(text, /42/, 'live gpu_requests');
  assert.match(text, /1 h 2 m/, 'live gpu_seconds, formatted');
  assert.match(text, /\$12\.50/, 'live cost');
  assert.match(text, /The accounts are separate; only the reading beside the title above adds them up/,
    'the table says the total is a sum of separate accounts, not a shared ledger');

  assert.ok(!text.includes(TOKEN_SECRET), 'the credential value is never rendered');
  // format.js truncateMiddle() uses U+2026, the panel-wide ellipsis; asserting ASCII dots here
  // would be a second, contradictory convention invented by the test.
  assert.match(text, /f8a1c2d3e4…332211/, 'a middle-truncated fingerprint is shown instead');
  const credential = descendants(mount)
    .find((el) => String(el.getAttribute('title')).startsWith('Credential fingerprint'));
  assert.ok(credential, 'the fingerprint says what it is, so it cannot be mistaken for the value');
  assert.ok(!text.includes(TOKEN_FINGERPRINT), 'even the full fingerprint is middle-truncated');
});

test('a lane that names what is missing is quoted, and an empty lane says there is no attach route', async () => {
  const handlers = standardHandlers();
  handlers['/api/gpu/lanes/kaggle/status'] = {
    ready: false,
    detail: 'No kaggle CLI credential is configured on this host.',
    accounts: [],
    notes: 'ready means the CLI answers, not that a kernel has run.',
  };
  const { mount } = await mountView({ handlers });
  const text = textOf(mount);

  assert.match(text, /No kaggle CLI credential is configured/,
    'the lane\'s own instruction reaches the operator instead of a generic not-ready chip');
  assert.match(text, /ready means the CLI answers/,
    'the lane\'s note about what readiness does not prove is quoted');
  assert.match(text, /This lane runs through the kaggle CLI on the host and has no attach route/,
    'the empty card says where the credential comes from, since there is no form to fill in');
  assert.doesNotMatch(text, /saturn|molab|cerebrium/i,
    'the three removed hosts appear nowhere on the inventory, not even quoted from /api/lanes');
});

test('inventory renders deployments with their own card, mode and derived sizing', async () => {
  const { mount } = await mountView({ handlers: standardHandlers() });
  const text = textOf(mount);

  assert.match(text, /Your rentals/);
  assert.match(text, /modal-vllm/);
  assert.match(text, /Qwen\/Qwen3-32B/);
  assert.match(text, /32\.8 k ctx/);
  assert.match(text, /rtx-pro-6000/);
  assert.match(text, /batch/, 'the mode is a chip on this row, not a second card');
  assert.match(text, /awq/);
  assert.doesNotMatch(text, /→ h100/, 'batch is a mode, not a second GPU');
  assert.match(text, /fp8/);
  // Assert the two facts in their own cells rather than flattening the row: flattening inserts a
  // separator between `~4` and ` of 8`, so a whitespace-sensitive match on the row would be
  // asserting the test helper, not the view.
  const sessionsCell = descendants(mount).find((el) => el.tagName === 'TD'
    && String(el.getAttribute('title')).startsWith('Derived from the sizing estimate'));
  assert.ok(sessionsCell, 'the sessions cell states that the number is derived');
  const [sessionsNode, ceilingNode] = sessionsCell.children;
  assert.equal(textOf(sessionsNode).trim(), '~4',
    'simultaneous sessions at max length, marked as derived');
  // "~4 of 8" reads as four available against eight allowed; a bare "/ 8 seqs" left the reader to
  // work out which number was the ceiling.
  assert.equal(textOf(ceilingNode).trim(), 'of 8', 'shown against max_num_seqs');
  assert.match(text, /1\.8 M/, 'KV pool tokens (1777758) compacted');
  assert.match(text, /Deployment records that could not be read/);
  assert.match(text, /provider "broken-vllm": rental\.gpu_count must be between 1 and 8/);
  assert.match(String(sessionsCell.getAttribute('title')), /Derived from the sizing estimate/,
    'the derivation is stated on the cell, not implied');
  const problems = descendants(mount).find((el) => el.tagName === 'SPAN'
    && el.classList.contains('chip-red') && textOf(el) === '2 problems');
  assert.ok(problems, 'deployment and hardware problems are counted, not dropped');
  assert.match(String(problems.getAttribute('title')), /max_model_len: does not fit the chosen card/);
  assert.match(String(problems.getAttribute('title')), /rtx-pro-6000: 96 GiB is below the KV pool floor/);
  assert.match(text, /estimates, marked with ~/,
    'the table explains what the ~ means and which number is authoritative');

  // Only deployment rows say "enabled"; the lane cards use their own readiness wording, so this
  // count still means "exactly one deployment is enabled".
  const enabled = descendants(mount)
    .filter((el) => classesOf(el).includes('chip-green') && textOf(el).trim() === 'enabled');
  assert.equal(enabled.length, 1);
});

test('the (Paid) marker appears once, and only for free_plan unavailable', async () => {
  const { mount } = await mountView({ handlers: standardHandlers() });
  const text = textOf(mount);

  const paidChips = descendants(mount).filter((el) => classesOf(el).includes('chip-amber'));
  assert.equal(paidChips.length, 1, 'exactly one badge in the catalogue');
  assert.equal(textOf(paidChips[0]).trim(), '(Paid)');
  assert.match(String(paidChips[0].getAttribute('title')), /observed/,
    'the badge carries its provenance');
  assert.equal(text.split('(Paid)').length - 1, 1, 'the literal text appears exactly once');

  assert.match(text, /free tier unknown/, 'an unlooked-at field is plain text, not a badge');
  assert.ok(!text.includes('(Paid) unknown'), 'and it is never conflated with the paid marker');
  assert.equal(
    descendants(mount).filter((el) => classesOf(el).includes('hint')
      && textOf(el).trim() === 'free tier').length,
    1,
    'the available card says "free tier" in grey text',
  );
});

test('free_plan unknown is never rendered as (Paid)', async () => {
  const only = catalog();
  only.platforms[0].gpus = [
    { id: 'rtx-pro-6000', label: 'RTX PRO 6000', free_plan: 'unknown', source: 'unknown' },
  ];
  const { mount } = await mountView({
    handlers: { '/api/gpu/rentals': rentals(), '/api/gpu/catalog': only },
  });
  const text = textOf(mount);

  assert.ok(!text.includes('(Paid)'), 'nobody stated this card is paid, so the panel must not say so');
  assert.match(text, /free tier unknown/);
});

test('free_plan absent is reported as not reported, again without a badge', async () => {
  const only = catalog();
  only.platforms[0].gpus = [{ id: 'mystery', label: 'Mystery card' }];
  const { mount } = await mountView({
    handlers: { '/api/gpu/rentals': rentals(), '/api/gpu/catalog': only },
  });
  const text = textOf(mount);

  assert.ok(!text.includes('(Paid)'));
  assert.match(text, /free tier not reported/);
  assert.match(text, /source not reported/);
});

test('plan limits are quoted prominently with their source', async () => {
  const { mount } = await mountView({ handlers: standardHandlers() });
  const text = textOf(mount);

  assert.match(text, /Plan limits — Modal/);
  assert.match(text, /concurrency and included credits, not which cards exist/);
  assert.match(text, /\$0\.00\/mo/);
  assert.match(text, /\$30\.00/);
  assert.match(text, /10 GPUs/);
  assert.match(text, /\$250\.00\/mo/);
  assert.match(text, /\$100\.00/);
  assert.match(text, /50 GPUs/);

  const links = descendants(mount)
    .filter((el) => el.tagName === 'A' && el.getAttribute('href') === 'https://modal.com/pricing');
  assert.ok(links.length >= 2, 'the documented price and limit sources are linked');
  for (const link of links) {
    assert.equal(link.getAttribute('rel'), 'noopener noreferrer');
    assert.equal(link.getAttribute('target'), '_blank');
  }
  // 3 seats for Starter, 5 for Team: the seats column is a fact from the same source.
  const seats = descendants(mount)
    .filter((el) => el.tagName === 'TD' && classesOf(el).includes('num'))
    .map((el) => textOf(el).trim());
  assert.ok(seats.includes('3') && seats.includes('5'), `seats column rendered: ${seats.join(' | ')}`);
});

test('the bundle is a text node in <pre>, and the Modal honesty line is present', async () => {
  const { mount } = await mountView({ handlers: standardHandlers() });
  const text = textOf(mount);

  const pres = descendants(mount).filter((el) => el.tagName === 'PRE');
  assert.ok(pres.length >= 2, 'the app file and the command each get a pre');

  const app = pres.find((el) => el.getAttribute('aria-label') === 'modal_app.py');
  assert.ok(app, 'the generated app file is rendered');
  assert.equal(app.children.length, 1);
  assert.equal(app.children[0].nodeType, 3, 'the app text is a text node, never markup');
  assert.equal(app.children[0].data, BUNDLE_TEXT);
  assert.ok(!app.children.some((kid) => kid.nodeType === 1),
    'nothing in the generated text was parsed as an element');

  const command = pres.find((el) => el.getAttribute('aria-label') === 'deploy command');
  assert.ok(command, 'the exact deploy command is rendered');
  assert.equal(textOf(command).trim(), 'modal deploy modal_app.py');

  assert.ok(text.includes('import modal'), 'the app source is on screen, not hidden behind a link');
  assert.match(text, /publishes no deployment REST API/);
  assert.match(text, /this bundle is the deployment/);
  assert.match(text, /Run the command yourself/);

  const copyLabels = descendants(mount)
    .filter((el) => el.tagName === 'BUTTON')
    .map((el) => textOf(el).trim());
  assert.ok(copyLabels.includes('Copy modal_app.py'), 'the generated file can be copied');
  assert.ok(copyLabels.includes('Copy command'), 'the exact command can be copied without selecting it by hand');

});

test('every error.details entry reaches the screen', async () => {
  const failure = Object.assign(new Error('The GPU catalogue could not be read.'), {
    details: [
      { field: 'platforms[0].gpus[0]', message: 'card h100 has no price_usd_per_hour' },
      { field: 'plan_limits', message: 'plan limits carry no source' },
    ],
  });
  const { mount } = await mountView({
    handlers: { '/api/gpu/rentals': rentals(), '/api/gpu/catalog': failure },
  });
  const text = textOf(mount);

  assert.match(text, /The GPU catalogue could not be read\./);
  assert.match(text, /card h100 has no price_usd_per_hour/);
  assert.match(text, /plan limits carry no source/);
  assert.match(text, /GET \/api\/gpu\/catalog failed/,
    'the status line says which endpoint failed');
  const alerts = descendants(mount).filter((el) => el.getAttribute('role') === 'alert');
  assert.ok(alerts.length >= 1, 'the failure is announced, not only coloured');
  assert.match(text, /Your rentals/, 'the half that answered is still shown');
});

test('an empty response renders neutral empty states and a next step', async () => {
  const { mount } = await mountView({
    handlers: {
      '/api/gpu/rentals': { accounts: [], deployments: [] },
      '/api/gpu/catalog': { platforms: [] },
      '/api/lanes': { lanes: [] },
      '/api/gpu/lanes/kaggle/status': { ready: false, accounts: [] },
    },
  });
  const text = textOf(mount);

  assert.match(text, /No rental accounts yet\./);
  assert.match(text, /No GPU rentals yet\./);
  assert.match(text, /No platform catalogue to read\./);
  assert.match(text, /No lane catalogue to read\./);

  const states = descendants(mount).filter((el) => classesOf(el).includes('empty-state'));
  assert.equal(states.length, 5,
    'one per empty surface: plan limits, accounts, deployments, GPU catalogue, lanes');
  for (const state of states) {
    assert.ok(!classesOf(state).some((c) => c === 'chip-red' || c === 'chip-amber'),
      'an empty list is not an error and is not coloured like one');
  }
  assert.ok(!descendants(mount).some((el) => el.getAttribute('role') === 'alert'),
    'nothing failed, so nothing is announced as a failure');
  const buttons = descendants(mount).map((el) => textOf(el).trim());
  assert.ok(buttons.includes('Deploy model'), 'the operator is told what to do next');
  assert.ok(buttons.includes('Add Modal account'));
  assert.ok(buttons.includes('Add HuggingFace token'),
    'a token is an independent record, so the page offers it without waiting for a gated repo');
  assert.ok(!buttons.includes('New rental'),
    'the page deploys through a window; a link that navigated away would hide the three actions');
});

test('a hidden tab stops asking for usage numbers', async (t) => {
  const timers = new Map();
  let nextId = 1;
  globalThis.setTimeout = (fn, ms) => { const id = nextId++; timers.set(id, { fn, ms }); return id; };
  globalThis.clearTimeout = (id) => { timers.delete(id); };
  t.after(() => {
    globalThis.setTimeout = realSetTimeout;
    globalThis.clearTimeout = realClearTimeout;
    globalThis.document.visibilityState = 'hidden';
  });

  const calls = [];
  const originalGet = api.get;
  api.get = async (path) => { calls.push(path); return standardHandlers()[path]; };
  t.after(() => { api.get = originalGet; });

  const mount = makeEl('div');
  const ctx = {
    state: {},
    navigate() {},
    toast: { success() {}, warning() {}, error() {}, info() {} },
    confirm: async () => true,
  };

  globalThis.document.visibilityState = 'hidden';
  await render(mount, ctx, null);
  const onVisibility = (winListeners.visibilitychange ?? []).at(-1);
  assert.equal(typeof onVisibility, 'function', 'the view listens for visibilitychange');
  const show = (state) => { globalThis.document.visibilityState = state; onVisibility(); };

  await settle();
  assert.equal(calls.length, ENDPOINTS.length,
    'the mount reads the inventory and every lane status once');
  assert.equal(timers.size, 0, 'a hidden tab arms no timer at all');

  // Returning to the tab fires one immediate tick, and then the interval re-reads.
  show('visible');
  await settle();
  assert.equal(calls.length, ENDPOINTS.length,
    'the first tick is skipped: the mount just read the same data');
  assert.equal(timers.size, 1, 'a visible tab arms the next read');

  const flush = async () => {
    const due = [...timers.values()];
    timers.clear();
    for (const timer of due) await timer.fn();
    await settle();
  };
  await flush();
  assert.equal(calls.length, ENDPOINTS.length * 2,
    'the interval re-reads every endpoint while the tab is visible');
  assert.deepEqual(calls.slice(0, ENDPOINTS.length), calls.slice(ENDPOINTS.length),
    'the same endpoints are re-read, in the same order');

  show('hidden');
  await settle();
  assert.equal(timers.size, 0, 'hiding the tab cancels the pending read');
  await flush();
  assert.equal(calls.length, ENDPOINTS.length * 2, 'and nothing is fetched once it is hidden');
});

// ---------------------------------------------------------------------------
// Requirement 5, as a rule
// ---------------------------------------------------------------------------

test('the quant selector stays blank until the model repository is named', () => {
  const blank = { repo: '', deployment: { gpu: 'h100', weight_quant: 'awq' } };
  assert.equal(quantGate(blank).ready, false);
  assert.match(quantGate(blank).reason, /stays blank until the model repository/);

  const whitespace = { repo: '   ' };
  assert.equal(quantGate(whitespace).ready, false, 'whitespace is not a repository name');

  const named = { repo: 'Qwen/Qwen3-32B' };
  assert.equal(quantGate(named).ready, true);
  assert.equal(quantGate(named).reason, null);

  const nested = { model: { repo: 'Qwen/Qwen3-32B' } };
  assert.equal(quantGate(nested).ready, true,
    'emptyDraft() may keep the repo under a container; either layout must open the gate');
});

// ---------------------------------------------------------------------------
// Setup mode (rendered from panel/gpu-setup-model.js, written in parallel)
// ---------------------------------------------------------------------------

test('requirement 5 in the DOM: the quant selector is blank before a probe and populated after one', async (t) => {
  if (!hasSetupModel) return t.skip('panel/gpu-setup-model.js is not present');
  await resetSetup();

  const model = await mountView({ param: 'new/model', handlers: standardHandlers() });
  setInput(model.mount, 'repo', 'Qwen/Qwen3-32B');
  fire(findButton(model.mount, 'Continue'), 'click');
  await settle();
  assert.deepEqual(model.navigations, ['#/gpu-rentals/new/card'],
    'a named repository is enough for the model step; the probe belongs to the card step');

  const blankCard = await mountView({ param: 'new/card', handlers: standardHandlers() });
  const blank = blankCard.mount.querySelector('[data-field="weightQuant"]');
  assert.ok(blank, 'the card step still draws the quant field');
  assert.equal(blank.disabled, true, 'the control is disabled, not merely empty-looking');
  assert.equal(blank.getAttribute('disabled'), '', 'and carries the disabled attribute a browser honours');
  assert.equal(blank.children.length, 0, 'it is not populated with a placeholder/None option');
  assert.equal(blank.value, '');
  const blankRow = blank.closest('.gpu-field-off');
  assert.ok(blankRow, 'the field is marked as deliberately off');
  assert.match(textOf(blankRow), /stays blank until the repository has been probed/i);
  assert.ok(!textOf(blankRow).includes('AWQ'), 'no quant is invented for an unprobed repository');

  // The card is also part of the quant gate: a probe without a chosen card has nothing to filter
  // the repository quants against. Choose the H100 on the card step before probing.
  setSelect(blankCard.mount, 'gpu', 'h100');
  assert.equal(blankCard.mount.querySelector('[data-field="gpu"]').value, 'h100');

  // Probe the same draft from the model step, then return to the card.
  const reprobe = await mountView({ param: 'new/model', handlers: standardHandlers() });
  await withPost(PROBE, async (posts) => {
    fire(findButton(reprobe.mount, 'Probe repository'), 'click');
    await settle(6);
    assert.equal(posts.length, 1);
    assert.equal(posts[0].path, '/api/gpu/probe');
    assert.equal(posts[0].body.repo, 'Qwen/Qwen3-32B');
  });
  assert.match(textOf(reprobe.mount), /KV heads/);
  fire(findButton(reprobe.mount, 'Continue'), 'click');
  await settle();
  assert.equal(reprobe.navigations.at(-1), '#/gpu-rentals/new/card');

  const populatedCard = await mountView({ param: 'new/card', handlers: standardHandlers() });
  const quant = populatedCard.mount.querySelector('[data-field="weightQuant"]');
  assert.ok(quant);
  assert.equal(quant.disabled, false, 'a probe makes the selector usable');
  assert.equal(quant.hasAttribute('disabled'), false);
  assert.equal(quant.closest('.gpu-field-off'), null);
  const values = quant.children.map((option) => option.getAttribute('value'));
  assert.deepEqual(values, ['fp8'], 'the options are the repository quants the probe reported');
  assert.equal(quant.value, 'fp8', 'the single in-repo quant is selected rather than left ambiguous');
  assert.ok(!textOf(quant).includes('none'), 'no synthetic none option is added');

  // The catalogue also offers AWQ, which this repository does not ship. That is the §8 case: a
  // quant the card could run but the checkpoint does not contain. It is an optional select, and the
  // page shows one compact sentence rather than a panel per catalog quant.
  const requantSelect = populatedCard.mount.querySelector('[data-field="requantizeTo"]');
  assert.ok(requantSelect, 'the requantise escape hatch is its own optional select');
  assert.equal(requantSelect.value, '', 'it defaults to serving what the repository ships');
  const requantValues = requantSelect.children.map((option) => option.getAttribute('value'));
  assert.deepEqual(requantValues[0], '', 'the empty choice is first and says no job is being recorded');
  assert.ok(requantValues.includes('awq'), 'a quant the repository lacks is offered as a future job');
  const requantText = textOf(populatedCard.mount);
  assert.match(requantText, /The repository ships fp8\./);
  assert.match(requantText, /each is a separate multi-hour GPU job, not something this flow does/);
  assert.equal(populatedCard.mount.querySelectorAll('.gpu-requant').length, 0,
    'nothing is chosen, so no recipe panel is drawn');

  setSelect(populatedCard.mount, 'requantizeTo', 'awq');
  await settle(4);
  const requant = textOf(populatedCard.mount);
  assert.match(requant, /Requantisation: awq is not in the repository/);
  assert.match(requant, /llm-compressor/);
  assert.match(requant, /Estimated time/);
  assert.match(requant, /VRAM needed/);
  assert.match(requant, /not free and not instant/);
  assert.match(requant, /shows the recipe and the command/);
  assert.match(requant, /does not run the quantiser for you/);
  assert.equal(populatedCard.mount.querySelectorAll('.gpu-requant').length, 1,
    'exactly one recipe panel, for the one quant chosen');

  setSelect(populatedCard.mount, 'requantizeTo', '');
  await settle(4);
  assert.equal(populatedCard.mount.querySelectorAll('.gpu-requant').length, 0,
    'clearing the choice removes the one recipe panel');
  setSelect(populatedCard.mount, 'requantizeTo', 'awq');
  await settle(4);
  assert.equal(populatedCard.mount.querySelectorAll('.gpu-requant').length, 1,
    'choosing again draws exactly one panel, not a second copy');
});

test('an unprobed repository can be probed from the card step, where its absence blocks a field', async (t) => {
  if (!hasSetupModel) return t.skip('panel/gpu-setup-model.js is not present');
  await resetSetup();
  const model = await mountView({ param: 'new/model', handlers: standardHandlers() });
  setInput(model.mount, 'repo', 'Qwen/Qwen3-32B');
  fire(findButton(model.mount, 'Continue'), 'click');
  await settle();

  const card = await mountView({ param: 'new/card', handlers: standardHandlers() });
  const quant = card.mount.querySelector('[data-field="weightQuant"]');
  assert.equal(quant.disabled, true, 'the selector is still gated before the probe');
  assert.match(textOf(card.mount), /It is one button, just below\./,
    'the reason names where the button is instead of sending the operator back a step');
  await withPost(PROBE, async (posts) => {
    fire(findButton(card.mount, 'Probe repository'), 'click');
    await settle(6);
    assert.equal(posts.length, 1);
    assert.equal(posts[0].path, '/api/gpu/probe');
  });
  assert.equal(descendants(card.mount)
    .filter((el) => el.tagName === 'BUTTON' && /Probe/.test(String(el.textContent ?? ''))).length, 0,
  'the probe panel leaves the step once it has nothing left to resolve');
  // The probe alone does not legalise a quant: the card is the other half of the gate, so the next
  // sentence names that instead of still asking for a probe.
  assert.match(textOf(card.mount.querySelector('[data-field="weightQuant"]').closest('.gpu-field-off') ?? card.mount),
    /Choose a card on this step first/);
  const pre = card.mount.querySelector('[data-field="gpu"]');
  pre.value = 'h100';
  fire(pre, 'change');
  await settle(4);
  const gated = card.mount.querySelector('[data-field="weightQuant"]');
  const gpuEl = card.mount.querySelector('[data-field="gpu"]');
  assert.equal(gated.disabled, false, 'the probe and the card together unblock the selector in place');
  assert.deepEqual(gated.children.map((option) => option.getAttribute('value')), ['fp8']);
});

test('the account step names the write it performs, because the review step comes after it', async (t) => {
  if (!hasSetupModel) return t.skip('panel/gpu-setup-model.js is not present');
  await resetSetup();
  const { mount } = await mountView({ param: 'new/account', handlers: standardHandlers() });

  assert.equal(advanceButton(mount).textContent.trim(), 'Save account',
    'the button that writes a credential to the vault does not read as a plain Continue');
  assert.match(textOf(mount), /Writes this account to the vault now/);
  assert.match(textOf(mount), /The rental itself is saved at the end\./);
  assert.match(textOf(mount), /One account can host several rentals/,
    'an account is not owned by the rental that first named it');
  assert.match(textOf(mount), /Several rentals may name the same account\./);

  // Reuse is offered on the same step: naming a recorded account writes nothing, so the two paths
  // sit side by side and only the create path stores a credential.
  const reuse = descendants(mount).filter((el) => el.tagName === 'BUTTON'
    && String(el.textContent ?? '') === 'Use this account');
  assert.equal(reuse.length, 1, 'the recorded account is offered for this rental too');
  assert.match(textOf(mount), /writes nothing/,
    'the reuse path says it does not touch the credential');
});

test('validation pins every problem to its field and reports all of them at once', async (t) => {
  if (!hasSetupModel) return t.skip('panel/gpu-setup-model.js is not present');
  await resetSetup();
  const { mount } = await mountView({ param: 'new/account', handlers: standardHandlers() });

  fire(advanceButton(mount), 'click');
  await settle();

  const account = mount.querySelector('[data-field="accountId"]');
  assert.ok(account, 'the account id field is rendered');
  assert.equal(account.getAttribute('aria-invalid'), 'true', 'the failing control is marked invalid');
  const row = account.closest('.field');
  const parent = row?.parentNode;
  const next = parent?.children?.[parent.children.indexOf(row) + 1];
  assert.ok(next?.classList?.contains('field-error'),
    'the problem is printed immediately after its own field, not only in a summary');
  assert.match(textOf(next), /accountId/);

  const summary = mount.querySelector('.form-errors');
  assert.ok(summary, 'the step also has an error summary');
  const summaryText = textOf(summary);
  assert.match(summaryText, /accountId/);
  assert.match(summaryText, /tokenId/);
  assert.match(summaryText, /tokenSecret/);
  assert.equal(mount.querySelectorAll('.field-error').length >= 1, true);
});

test('the account step posts the vault entry before any model step, and clears no secret into the page', async (t) => {
  if (!hasSetupModel) return t.skip('panel/gpu-setup-model.js is not present');
  await resetSetup();
  const { mount, navigations } = await mountView({ param: 'new/account', handlers: standardHandlers() });

  const text = textOf(mount);
  assert.match(text, /\$0\/month/);
  assert.match(text, /\$250\/month/);
  assert.match(text, /10/);
  const sourceLinks = descendants(mount)
    .filter((el) => el.tagName === 'A' && el.getAttribute('href') === 'https://modal.com/pricing');
  assert.ok(sourceLinks.length >= 1, 'the plan limits carry a link to their published source');

  setInput(mount, 'accountId', 'mana-modal-2');
  setInput(mount, 'workspace', 'mana');
  setInput(mount, 'tokenId', 'wk-abc123');
  setInput(mount, 'tokenSecret', 'ws-super-secret-value');

  await withPost({ account: { id: 'mana-modal-2', secret_fingerprint: TOKEN_FINGERPRINT } }, async (posts) => {
    fire(advanceButton(mount), 'click');
    await settle(6);
    assert.equal(posts.length, 1, 'one account request, before the flow reaches the model step');
    assert.equal(posts[0].path, '/api/gpu/apply');
    assert.equal(posts[0].body.action, 'create-account');
    assert.equal(posts[0].body.account.id, 'mana-modal-2');
    assert.equal(posts[0].body.account.workspace, 'mana', 'the workspace is recorded as a plain field');
    assert.equal(posts[0].body.account.token_id, 'wk-abc123');
    assert.equal(posts[0].body.account.token_secret, 'ws-super-secret-value');
    assert.equal('hf_token' in posts[0].body.account, false,
      'a HuggingFace token is its own record: the account write does not carry one');
  });

  assert.equal(navigations.at(-1), '#/gpu-rentals/new/model');
  assert.match(textOf(mount), /Account stored/);
  assert.ok(!textOf(mount).includes('ws-super-secret-value'), 'the token value is never rendered back');
});

test('a recorded account is reused for another rental, and a duplicate id is refused before it posts', async (t) => {
  if (!hasSetupModel) return t.skip('panel/gpu-setup-model.js is not present');
  await resetSetup();
  const { mount, navigations } = await mountView({ param: 'new/account', handlers: standardHandlers() });

  const text = textOf(mount);
  assert.match(text, /Accounts already recorded for this platform/);
  assert.match(text, /An account can pay for several rentals: choosing one here names it and writes nothing/);
  const reuse = descendants(mount).filter((el) => el.tagName === 'BUTTON'
    && String(el.textContent ?? '') === 'Use this account');
  assert.equal(reuse.length, 1, 'one recorded account, one way to name it');

  // Naming the account is a draft change, not a write: the credential already in the vault is the
  // one this second rental bills through, and re-sending it would be a second copy of a secret.
  await withPost({ ignored: true }, async (posts) => {
    fire(reuse[0], 'click');
    await settle(4);
    assert.equal(posts.length, 0, 'reuse posts nothing');
    const accountRow = descendants(mount).find((el) => el.classList?.contains('gpu-account-row')
      && String(el.textContent ?? '').includes('mana-modal'));

    assert.ok(accountRow, 'the reused account appears in the selection');
    const chk = accountRow.querySelector('input[type="checkbox"]');
    assert.equal(chk && chk.checked, true, 'the reused account is included');
    assert.equal(advanceButton(mount).textContent.trim(), 'Continue',
      'nothing is written, so the button does not promise a write');
    assert.match(textOf(mount), /Reuses a recorded account: nothing is written/);
  });

  // Creating a second account under a recorded id would collide on that one vault entry, so the
  // panel refuses it before sending a credential that cannot be stored.
  await resetSetup();
  const second = await mountView({ param: 'new/account', handlers: standardHandlers() });
  setInput(second.mount, 'accountId', 'mana-modal');
  setInput(second.mount, 'workspace', 'mana');
  setInput(second.mount, 'tokenId', 'wk-abc123');
  setInput(second.mount, 'tokenSecret', 'ws-super-secret-value');
  await withPost({ error: { code: 'conflict' } }, async (posts) => {
    fire(advanceButton(second.mount), 'click');
    await settle(6);
    assert.equal(posts.length, 0,
      'the panel refuses the id before it sends a credential that would collide');
    assert.equal(second.navigations.includes('#/gpu-rentals/new/model'), false,
      'a refused account does not advance');
  });
  assert.match(textOf(second.mount), /is already recorded\. Reuse it instead of creating a second one/,
    'the refusal names the reuse path rather than a missing feature');
});

test('the page actions open windows rather than routes, and each says what it will write', async (t) => {
  if (!hasSetupModel) return t.skip('panel/gpu-setup-model.js is not present');
  await resetSetup();
  document.body.replaceChildren();
  const { mount, navigations } = await mountView({ handlers: standardHandlers() });
  const before = navigations.length;

  // Add a Modal account: the provider form's shape — labelled fields, hints, a secret written once.
  fire(findButton(mount, 'Add Modal account'), 'click');
  await settle(6);
  let text = textOf(document.body);
  assert.match(text, /Add a Modal account/);
  assert.match(text, /The credential is written to the vault once/);
  for (const name of ['id', 'workspace', 'tokenId', 'tokenSecret']) {
    assert.ok(document.body.querySelector(`[data-field="${name}"]`), `the account window asks for ${name}`);
  }
  assert.match(text, /Several deployments may name the same account/,
    'the window says out loud that an account is not spent by its first deployment');
  fire(findButton(document.body, 'Cancel'), 'click');

  // Add a HuggingFace token: its own record, not a property of an account.
  fire(findButton(mount, 'Add HuggingFace token'), 'click');
  await settle(6);
  text = textOf(document.body);
  assert.match(text, /HuggingFace token/);
  assert.match(text, /not tied to a Modal account/);
  assert.ok(document.body.querySelector('[data-field="id"]'));
  assert.ok(document.body.querySelector('[data-field="secret"]'));
  fire(findButton(document.body, 'Cancel'), 'click');

  assert.equal(navigations.length, before, 'no action navigates away from the inventory');
});

test('the account window writes one vault entry and never renders the secret back', async (t) => {
  if (!hasSetupModel) return t.skip('panel/gpu-setup-model.js is not present');
  await resetSetup();
  document.body.replaceChildren();
  const { mount } = await mountView({ handlers: standardHandlers() });
  fire(findButton(mount, 'Add Modal account'), 'click');
  await settle(6);

  const type = (name, value) => {
    const input = document.body.querySelector(`[data-field="${name}"]`);
    assert.ok(input, `expected a control for ${name}`);
    input.value = value;
    fire(input, 'input');
  };
  type('id', 'mana-modal-2');
  type('label', 'Modal — second workspace');
  type('workspace', 'mana-two');
  type('tokenId', 'wk-second-account');
  type('tokenSecret', TOKEN_SECRET);

  await withPost({
    account_id: 'mana-modal-2',
    vault: { secret_id: 'rental:mana-modal-2', fingerprint: 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' },
  }, async (posts) => {
    fire(findButton(document.body, 'Add account'), 'click');
    await settle(6);
    assert.equal(posts.length, 1);
    assert.equal(posts[0].path, '/api/gpu/apply');
    assert.equal(posts[0].body.action, 'create-account');
    assert.equal(posts[0].body.account.id, 'mana-modal-2');
    assert.equal(posts[0].body.account.label, 'Modal — second workspace');
    assert.equal(posts[0].body.account.workspace, 'mana-two');
    assert.equal(posts[0].body.account.token_secret, TOKEN_SECRET,
      'the secret travels once, to the vault, on the request that stores it');
  });

  const after = textOf(document.body);
  assert.equal(after.includes(TOKEN_SECRET), false, 'the window never renders the secret back');
  assert.match(after, /aaaaaaaaaaaaaaaa/);
  assert.match(after, /no response carries it/);
});

test('a HuggingFace token is written by its own action, with no account attached', async (t) => {
  if (!hasSetupModel) return t.skip('panel/gpu-setup-model.js is not present');
  await resetSetup();
  document.body.replaceChildren();
  const { mount } = await mountView({ handlers: standardHandlers() });
  fire(findButton(mount, 'Add HuggingFace token'), 'click');
  await settle(6);

  const type = (name, value) => {
    const input = document.body.querySelector(`[data-field="${name}"]`);
    assert.ok(input, `expected a control for ${name}`);
    input.value = value;
    fire(input, 'input');
  };
  type('id', 'hf-main');
  type('secret', TOKEN_SECRET);

  await withPost({ vault: { secret_id: 'hf:hf-main', fingerprint: 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb' } },
    async (posts) => {
      fire(findButton(document.body, 'Add token'), 'click');
      await settle(6);
      assert.equal(posts.length, 1);
      assert.equal(posts[0].body.action, 'create-hf-token');
      assert.deepEqual(posts[0].body.token, { id: 'hf-main', label: 'hf-main', secret: TOKEN_SECRET });
      assert.equal('account' in posts[0].body, false,
        'a token is not scoped to an account, so the token action names none');
    });
});

test('the deploy window shows every step at once and reuses the recorded account', async (t) => {
  if (!hasSetupModel) return t.skip('panel/gpu-setup-model.js is not present');
  await resetSetup();
  document.body.replaceChildren();
  const { mount } = await mountView({ handlers: standardHandlers() });

  await withGet(standardHandlers(), () => withPost({}, async (posts) => {
    fire(findButton(mount, 'Deploy model'), 'click');
    await settle(8);
    assert.equal(posts.length, 0, 'opening the window writes nothing');

    const title = textOf(document.body);
    assert.match(title, /Deploy a model/);
    // Every step is on the page: the window does not paginate the flow.
    for (const section of ['Account', 'Model', 'Card', 'Sizing']) {
      assert.ok(descendants(document.body).some((el) => el.tagName === 'SECTION'
        && /gpu-deploy-section/.test(el.className)),
      `the window is one scrolling form; it showed: ${textOf(document.body).slice(0, 400)}`);
      assert.match(title, new RegExp(section), `the ${section} step is in the window`);
    }
    // The account is chosen, never created here: creating one is the page's own action.
    assert.equal(document.body.querySelector('[data-field="accountMode"]'), null,
      'the window does not offer to create an account a second time');
    const accountRow = descendants(document.body).find((el) => el.classList?.contains('gpu-account-row')
      && String(el.textContent ?? '').includes('mana-modal'));
    assert.ok(accountRow, 'the recorded account is named');
    assert.equal(accountRow.querySelector('input[type="checkbox"]')?.checked, true,
      'the recorded account is included by default');

    // The fill policy is a choice of the deployment, and the timeout only exists when it is chosen.
    assert.ok(document.body.querySelector('[data-field="mode"]'), 'the window offers the mode axis');
    assert.equal(document.body.querySelector('[data-field="batchSize"]'), null,
      'streaming does not ask for a batch size');
  }));
  assert.ok(descendants(document.body).some((el) => /dialog-backdrop/.test(el.className)),
    'the window is still on the page when the assertions finish');
});

test('a gated deploy stores the pasted token before the deployment that names it', async (t) => {
  if (!hasSetupModel) return t.skip('panel/gpu-setup-model.js is not present');
  await resetSetup();
  document.body.replaceChildren();
  const { mount } = await mountView({ handlers: standardHandlers() });
  const gated = {
    ...PROBE,
    repo: 'orcarouter/Qwen3.8-27B-Uncensored-FP8',
    gated: { state: 'manual', needs_token: true },
  };
  await withGet(standardHandlers(), () => withPost((path, body) => {
    if (path === '/api/gpu/probe') return gated;
    if (path !== '/api/gpu/apply') throw new Error(`test did not expect POST ${path}`);
    if (body.action === 'create-hf-token') {
      return {
        token: { id: 'hub-main', label: 'hub-main', replaced: false },
        vault: { secret_ref: 'hf:hub-main', fingerprint: TOKEN_FINGERPRINT },
      };
    }
    return { provider_id: 'manarouter-rental-mana-modal-qwen3-8-27b-streaming-l40s', account_id: 'mana-modal' };
  }, async (posts) => {
    fire(findButton(mount, 'Deploy model'), 'click');
    await settle(8);
    setInput(document.body, 'repo', gated.repo);
    fire(findButton(document.body, 'Probe repository'), 'click');
    await settle(14);
    setSelect(document.body, 'gpu', 'rtx-pro-6000');
    setSelect(document.body, 'weightQuant', 'fp8');
    setSelect(document.body, 'kvCacheDtype', 'fp8');
    setInput(document.body, 'hfTokenNew', TOKEN_SECRET);
    setInput(document.body, 'hfTokenNewLabel', 'hub-main');
    await settle(4);

    const record = descendants(document.body)
      .find((el) => el.tagName === 'BUTTON' && String(el.textContent ?? '').trim() === 'Record deployment');
    assert.ok(record, 'a fitting deployment offers a way to record it');
    assert.equal(record.disabled, false, 'a fitting gated deployment can be recorded');

    fire(record, 'click');
    await settle(20);

    const writes = posts.map((post) => post.body).filter((body) => body.action);
    assert.deepEqual(writes.map((body) => body.action),
      ['create-hf-token', 'create-deployment'],
      'the token is written first: a deployment that named an unwritten token would point at nothing');
    const [token, deployment] = writes;
    assert.deepEqual(token.token, { id: 'hub-main', label: 'hub-main', secret: TOKEN_SECRET },
      'the pasted secret is written once, under the id the deployment will name');
    assert.equal('account' in token, false, 'a token is its own record, not a property of an account');
    assert.equal(deployment.account_id, 'mana-modal', 'the deployment still bills through its account');
    assert.equal(deployment.hf_token, 'hub-main',
      'the deployment names the token by id, and by the id that was just written');
    assert.equal(deployment.deployment.repo, gated.repo);

    const text = textOf(document.body).replace(/\s+/g, ' ');
    assert.match(text, /HuggingFace token hub-main/, 'the recorded window names the token it used');
    assert.match(text, /manarouter-hf/,
      'a gated deployment is told the container still needs the token mounted as a Modal secret');
  }));
});

test('an unestimated deployment is refused as unknown, not claimed unfit', async (t) => {
  if (!hasSetupModel) return t.skip('panel/gpu-setup-model.js is not present');
  await resetSetup();
  document.body.replaceChildren();
  const { mount } = await mountView({ handlers: standardHandlers() });
  // A config.json with no head_dim cannot yield a KV figure. "I cannot tell" must not be dressed
  // as "does not fit", and no suggestion may be printed: none of them could be proven to work.
  const blind = { ...PROBE, arch: { num_hidden_layers: 64, num_key_value_heads: 8 } };
  await withGet(standardHandlers(), () => withPost((path) => {
    if (path === '/api/gpu/probe') return blind;
    throw new Error(`test did not expect POST ${path}`);
  }, async (posts) => {
    fire(findButton(mount, 'Deploy model'), 'click');
    await settle(8);
    setInput(document.body, 'repo', blind.repo);
    fire(findButton(document.body, 'Probe repository'), 'click');
    await settle(14);
    setSelect(document.body, 'gpu', 'l40s');
    setSelect(document.body, 'weightQuant', 'fp8');
    await settle(4);
    const text = textOf(document.body).replace(/\s+/g, ' ');
    assert.match(text, /cannot be checked yet/, 'an unknown verdict says so instead of claiming a refusal');
    assert.doesNotMatch(text, /This does not fit on the chosen card/,
      'a verdict the panel could not compute is not reported as a measurement');
    const buttons = descendants(document.body)
      .filter((el) => el.tagName === 'BUTTON')
      .map((el) => String(el.textContent ?? '').trim());
    assert.equal(buttons.some((label) => /^Change GPU to |^Cap the context at /.test(label)), false,
      'no suggestion is printed when none can be proven sufficient');
    assert.equal(findButton(document.body, 'Record deployment').disabled, true,
      'an unestimated deployment is not recorded either');
    assert.equal(posts.length, 1, 'only the probe was a request');
  }));
});

test('a deployment that does not fit offers the changes that do, and records nothing', async (t) => {
  if (!hasSetupModel) return t.skip('panel/gpu-setup-model.js is not present');
  await resetSetup();
  document.body.replaceChildren();
  const { mount } = await mountView({ handlers: standardHandlers() });
  await withGet(standardHandlers(), () => withPost((path) => {
    if (path === '/api/gpu/probe') return PROBE;
    throw new Error(`test did not expect POST ${path}`);
  }, async (posts) => {
    fire(findButton(mount, 'Deploy model'), 'click');
    await settle(8);
    setInput(document.body, 'repo', PROBE.repo);
    fire(findButton(document.body, 'Probe repository'), 'click');
    await settle(14);
    setSelect(document.body, 'gpu', 'l40s');
    setSelect(document.body, 'weightQuant', 'fp8');
    setSelect(document.body, 'kvCacheDtype', 'fp8');
    await settle(4);

    const readout = () => textOf(document.body).replace(/\s+/g, ' ');
    assert.match(readout(), /This does not fit on the chosen card/,
      'a deployment that cannot be served is refused before it is recorded');
    const record = () => findButton(document.body, 'Record deployment');
    assert.equal(record().disabled, true, 'nothing is recorded while it does not fit');

    // Every suggestion below was produced by re-running the fit check with that one field changed,
    // so it is a change the panel has already proven sufficient — not a rule of thumb.
    const cardFix = descendants(document.body).find((el) => el.tagName === 'BUTTON'
      && /^Change GPU to \S+/.test(String(el.textContent ?? '').trim()) && !el.disabled);
    assert.ok(cardFix, 'a card that holds this deployment unchanged is offered first');
    fire(findButton(document.body, 'Cap the context at 32768 tokens'), 'click');
    await settle(6);
    assert.equal(posts.length, 1, 'taking a suggestion edits the draft; only the probe was a request');
    assert.match(readout(), /It fits/, 'the change the panel suggested is the change that fits');
    assert.equal(record().disabled, false, 'once it fits, it can be recorded');
  }));
});

const setupModelUrl = new URL('../gpu-setup-model.js', import.meta.url);
const hasSetupModel = existsSync(setupModelUrl);

test('setup mode either mounts the step flow or names the module it is missing', async (t) => {
  const { mount } = await mountView({
    param: 'new/platform',
    handlers: { '/api/gpu/catalog': catalog(), '/api/gpu/rentals': rentals() },
  });
  const text = textOf(mount);

  if (!hasSetupModel) {
    t.diagnostic('panel/gpu-setup-model.js does not exist yet (it is written by another agent), so '
      + 'the step flow itself cannot be exercised here. The view is asserted to report that gap '
      + 'rather than to render a form that would save nothing.');
    assert.match(text, /GPU setup flow is not available/);
    assert.match(text, /panel\/gpu-setup-model\.js/);
    assert.match(text, /Nothing was stubbed/);
    assert.ok(!text.includes('Rental accounts'), 'setup mode does not fall back to the inventory');
    return;
  }

  // The module is present: the platform step must be on screen with a step rail.
  assert.ok(descendants(mount).some((el) => classesOf(el).includes('gpu-steps')),
    'the step rail is rendered');
  assert.ok(text.length > 0);
  assert.match(text, /Modal/);
});

test('render() accepts a missing param without throwing', async () => {
  const { mount, calls } = await mountView({ handlers: standardHandlers() });
  assert.ok(mount.children.length > 0);
  assert.equal(calls.length, ENDPOINTS.length);
});

test('the lane table quotes the platform that refuses, and invents nothing where it does not', async () => {
  const { mount } = await mountView({ handlers: standardHandlers() });
  const text = textOf(mount);

  assert.match(text, /Platform lanes/);
  assert.match(text, /Modal/);
  assert.match(text, /Kaggle Notebooks/);
  assert.match(text, /usable/, 'a lane with no refusal says so instead of leaving the cell blank');
  // The refusal is the platform's sentence, attributed, and labelled with the traffic it blocks.
  assert.match(text, /You also may not have, control, or operate under more than one active Kaggle account/);
  const refusalLabels = descendants(mount)
    .filter((el) => classesOf(el).includes('gpu-lane-traffic'))
    .map((el) => el.getAttribute('title'));
  assert.ok(refusalLabels.includes('multiple_accounts_forbidden'),
    'the refusal code is kept, as a tooltip, so a log line can be matched to the screen');
  assert.ok(refusalLabels.includes('lane_is_batch_only'));
  assert.match(text, /kaggle\.com/, 'the source of a refusal is named next to it');

  // One link per distinct host: kaggle.com appears three times in the fixture and is rendered once.
  const laneRows = descendants(mount).filter((el) => el.tagName === 'TR');
  assert.ok(laneRows.length >= 2);
  const docsCell = descendants(mount).find((el) => classesOf(el).includes('gpu-lane-docs')
    && textOf(el).includes('kaggle.com'));
  assert.ok(docsCell, 'the lane carries its sources as a cell of links');
  const kaggleLinks = descendants(docsCell)
    .filter((el) => el.tagName === 'A' && /kaggle\.com/.test(el.getAttribute('href') ?? ''));
  assert.equal(kaggleLinks.length, 1,
    'two pages on one host collapse to one link, so the cell stays readable');
});

// ---------------------------------------------------------------------------
// The lane status card: a lane that is down must not blank the inventory
// ---------------------------------------------------------------------------

test('a lane whose status route fails is named, and the inventory still renders', async () => {
  const { mount } = await mountView({
    handlers: { ...laneStatuses(), '/api/gpu/lanes/kaggle/status': new Error('connect refused') },
  });
  const text = textOf(mount);

  assert.match(text, /GET \/api\/gpu\/lanes\/kaggle\/status failed/,
    'the failed route is named, with its own path');
  assert.match(text, /connect refused/, 'the reason is quoted, not swallowed');
  assert.match(text, /Rental accounts/, 'the page above the lane card is unaffected');
  assert.match(text, /Your rentals/);
  assert.match(text, /kaggle \(endpoint unavailable\)/,
    'the card says which lane it is even when the lane could not answer');
});

test('the plan preview posts the schedule and shows each wave with the deployments it holds',
  async () => {
    const { mount } = await mountView({ handlers: standardHandlers() });
    const plan = {
      schedule: 'sequential',
      max_live: 1,
      note: 'Sequential launches one deployment at a time.',
      waves: [{ cards: 1, deployments: [0] }, { cards: 1, deployments: [1] }],
      problems: [],
      malformed: [],
      deployments: [
        { index: 0, provider: 'modal', repo: 'org/qwen3-27b', gpu: 'rtx-pro-6000', gpu_count: 1, account: 'modal-1' },
        { index: 1, provider: 'modal', repo: 'org/llama-8b', gpu: 'l40s', gpu_count: 1, account: 'modal-1' },
      ],
    };
    await withPost(plan, async (posts) => {
      fire(findButton(mount, 'Plan launch'), 'click');
      await settle();

      assert.equal(posts.length, 1, 'one request per press');
      assert.equal(posts[0].path, '/api/gpu/launch-plan');
      assert.equal(posts[0].body.schedule, 'sequential', 'the chosen schedule is what is sent');

      const text = textOf(mount);
      assert.match(text, /Launch waves/, 'the answer is rendered as waves');
      assert.match(text, /org\/qwen3-27b/, 'a wave names the deployment it holds, not only its index');
      assert.match(text, /org\/llama-8b/);
      assert.match(text, /does not start/i, 'the preview says it starts nothing');
    });
  });
