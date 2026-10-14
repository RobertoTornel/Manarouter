// app.js — panel entry point: session gate, hash router, health polling, the persistent
// restart-pending banner, and the shared context (ctx) handed to every view.
//
// Structure: views live in ./view-*.js, each exporting `render(mount, ctx, param)`. ctx carries
// shared services (toast, confirm dialogs, cached data loaders, the restart flow) so views stay
// free of cross-imports.

import { api, ApiError, onUnauthorized } from './api.js';
import { h, clear, errorBlock, spinner } from './dom.js';
import { createPoller } from './poll.js';
import { createToasts } from './toast.js';
import { confirmDialog, secretKeyDialog } from './dialog.js';
import {
  extractList, portsDirty, describePortChange, truncateMiddle, formatUptime,
} from './format.js';

import { renderLogin } from './view-login.js';
import { renderOverview } from './view-overview.js';
import { renderProviders } from './view-providers.js';
import { renderCombos } from './view-combos.js';
import { renderKeys } from './view-keys.js';
import { renderAliases } from './view-aliases.js';
import { renderModels } from './view-models.js';
import { renderQuota } from './view-quota.js';
import { renderSettings } from './view-settings.js';
import { renderDocs } from './view-docs.js';

// ---------------------------------------------------------------------------
// Shared state & services
// ---------------------------------------------------------------------------

const state = {
  session: null,      // { authenticated, password_required }
  health: null,       // GET /api/health payload
  ports: null,        // GET /api/ports payload
  providers: null,    // cached GET /api/providers (models view, combo pickers, quota view)
  restarting: false,
};

const toast = createToasts(document.getElementById('toasts'));

// Load-once provider cache with in-flight de-duplication: several views want the same list and
// providers are the heaviest collection the control plane serves.
async function loadProviders(force = false) {
  if (!force && state.providers) return state.providers;
  if (state.providersPromise && !force) return state.providersPromise;
  state.providersPromise = api.get('/api/providers')
    .then((data) => {
      state.providers = extractList(data, 'providers');
      return state.providers;
    })
    .finally(() => { state.providersPromise = null; });
  return state.providersPromise;
}

// Live-update subscriptions. renderRoute() clears them before each view change, so a view's
// subscription dies with the view — no manual unsubscription bookkeeping, no leaks.
const healthSubs = new Set();
function emitHealth() {
  for (const cb of healthSubs) cb(state.health);
}

async function refreshHealth() {
  try {
    state.health = await api.get('/api/health');
  } catch {
    state.health = null; // rendered as "control plane unreachable", never as fake data
  }
  updateHealthPill();
  updateSidebarMeta();
  emitHealth();
}

async function refreshPorts() {
  try {
    state.ports = await api.get('/api/ports');
  } catch {
    state.ports = null;
  }
  updateBanner();
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/**
 * Shared restart flow (Settings view + the persistent banner button).
 *
 * WHY network errors are swallowed around POST /api/restart: restarting the control plane can
 * drop the very connection that carried the response — that is expected, not a failure. The
 * panel then polls /api/health until the process answers again (bounded to 30 s) and reports
 * honestly if it never comes back.
 */
async function restartFlow() {
  if (state.restarting) return; // a restart is already in flight — no double submit
  const ok = await confirmDialog(dialogRoot, {
    title: 'Restart Manarouter?',
    message: 'The relay and control plane will restart. In-flight requests may be dropped; the panel reconnects automatically.',
    confirmLabel: 'Restart',
    danger: false, // disruptive, not destructive — so the confirm button is pink, not red
  });
  if (!ok) return;

  state.restarting = true;
  updateHealthPill();
  updateBanner();
  try {
    await api.post('/api/restart');
  } catch (err) {
    if (!(err instanceof ApiError && (err.status === 0 || err.status >= 500))) {
      state.restarting = false;
      updateHealthPill();
      updateBanner();
      toast.error('Restart failed', err);
      return;
    }
    // Connection dropped or 5xx mid-restart: expected, keep waiting below.
  }

  let back = null;
  for (let i = 0; i < 30 && !back; i++) {
    await sleep(1000);
    try { back = await api.get('/api/health'); } catch { /* still coming up */ }
  }

  state.restarting = false;
  if (back) {
    state.health = back;
    toast.success('Restart complete.');
    await Promise.all([refreshPorts(), loadProviders(true), refreshHealth()]);
    renderRoute();
  } else {
    updateHealthPill();
    updateBanner();
    toast.warning('The control plane has not answered for 30 s. It may still be restarting — check the process log.');
  }
}

const ctx = {
  state,
  toast,
  confirm: (opts) => confirmDialog(dialogRoot, opts),
  secretKey: (opts) => secretKeyDialog(dialogRoot, opts),
  navigate: (hash) => { location.hash = hash; },
  loadProviders,
  refreshPorts,
  refreshHealth,
  restartFlow,
  onHealth: (cb) => {
    healthSubs.add(cb);
    if (state.health) cb(state.health); // immediate replay so late subscribers render once
    return () => healthSubs.delete(cb);
  },
};

// ---------------------------------------------------------------------------
// Login gate
// ---------------------------------------------------------------------------

const shell = document.getElementById('shell');
const loginRoot = document.getElementById('login-root');
const mainEl = document.getElementById('main');
const dialogRoot = document.getElementById('dialog-root');
const drawerRoot = document.getElementById('drawer-root');
const bannerSlot = document.getElementById('banner-slot');

function showLogin(notice = null) {
  poller.stop();
  shell.hidden = true;
  loginRoot.hidden = false;
  clear(loginRoot);
  if (notice) toast.info(notice);
  renderLogin(loginRoot, {
    toast,
    onSuccess: async () => {
      try {
        state.session = await api.get('/api/session');
      } catch {
        state.session = { authenticated: true, password_required: true };
      }
      await enterApp();
    },
  });
  const first = loginRoot.querySelector('input');
  if (first) first.focus();
}

async function enterApp() {
  loginRoot.hidden = true;
  clear(loginRoot);
  shell.hidden = false;
  if (!location.hash) location.hash = '#/overview'; // fires hashchange → first render
  else renderRoute();
  await Promise.all([refreshHealth(), refreshPorts()]);
  poller.start();
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

const ROUTES = [
  { id: 'overview', re: /^#\/overview$/, title: 'Overview', render: renderOverview },
  { id: 'providers', re: /^#\/providers$/, title: 'Providers', render: renderProviders },
  { id: 'combos', re: /^#\/combos(?:\/(.+))?$/, title: 'Combos', render: renderCombos },
  { id: 'keys', re: /^#\/keys$/, title: 'Keys', render: renderKeys },
  { id: 'aliases', re: /^#\/aliases$/, title: 'Aliases', render: renderAliases },
  { id: 'models', re: /^#\/models$/, title: 'Models', render: renderModels },
  { id: 'quota', re: /^#\/quota$/, title: 'Quota', render: renderQuota },
  { id: 'settings', re: /^#\/settings$/, title: 'Settings', render: renderSettings },
  { id: 'docs', re: /^#\/docs(?:\/(.+))?$/, title: 'Documentation', render: renderDocs },
];

function renderRoute() {
  const gate = !state.session?.authenticated && state.session?.password_required;
  if (gate) { showLogin(); return; }

  healthSubs.clear(); // previous view's live subscriptions die here
  clear(drawerRoot);  // drawers are per-view UI state

  const hash = location.hash || '#/overview';
  const route = ROUTES.find((r) => r.re.test(hash)) ?? ROUTES[0];
  const m = hash.match(route.re);
  const param = m && m[1] ? decodeURIComponent(m[1]) : null;

  document.title = `Manarouter — ${route.title}`;
  document.getElementById('topbar-title').textContent = route.title;
  for (const a of document.querySelectorAll('#nav-list a')) {
    if (a.dataset.route === route.id) a.setAttribute('aria-current', 'page');
    else a.removeAttribute('aria-current');
  }

  clear(mainEl);
  mainEl.append(h('div', { class: 'toolbar' }, spinner(), h('span', { class: 'count' }, 'Loading…')));
  route.render(mainEl, ctx, param).catch((err) => {
    clear(mainEl);
    const box = errorBlock(err);
    const retry = h('button', { class: 'btn', type: 'button' }, 'Retry');
    retry.addEventListener('click', () => renderRoute());
    box.append(h('div', { class: 'dialog-actions' }, retry));
    mainEl.append(box);
  });
}

// ---------------------------------------------------------------------------
// Global chrome: health pill, sidebar meta, restart-pending banner
// ---------------------------------------------------------------------------

function updateHealthPill() {
  const pill = document.getElementById('topbar-health');
  const dot = pill.querySelector('.dot');
  const text = pill.querySelector('.health-text');
  if (state.restarting) {
    dot.className = 'dot dot-amber';
    text.textContent = 'restarting…';
    pill.setAttribute('aria-label', 'Control plane is restarting');
    return;
  }
  if (!state.health) {
    dot.className = 'dot dot-red';
    text.textContent = 'control plane unreachable';
    pill.setAttribute('aria-label', 'Control plane unreachable — open overview');
    return;
  }
  dot.className = 'dot dot-green';
  text.textContent = `v${state.health.version ?? '?'} · up ${formatUptime(state.health.uptime_secs)}`;
  pill.setAttribute('aria-label', `Control plane healthy, version ${state.health.version ?? 'unknown'} — open overview`);
}

function updateSidebarMeta() {
  const el = document.getElementById('sidebar-meta');
  const snap = state.health?.snapshot;
  el.replaceChildren(
    state.health
      ? `v${state.health.version ?? '?'} · ${truncateMiddle(snap?.digest ?? '', 6, 4) || 'no snapshot'}`
      : 'disconnected',
  );
}

/**
 * The restart-pending banner. Amber by design: the saved port change is a warning that the
 * configuration is not live yet, not an error. It is rendered above <main> (outside the view)
 * so it persists across every view until the restart actually happens — there is no dismiss
 * button on purpose.
 */
function updateBanner() {
  clear(bannerSlot);
  const dirty = portsDirty(state.ports);
  const flagged = Boolean(state.ports?.restart_pending);
  if (!dirty && !flagged) return;

  const changes = describePortChange(state.ports);
  const box = h('div', { class: 'banner', role: 'status' });
  const text = h('div', { class: 'banner-text' });
  text.append(h('strong', {}, 'Pending restart: '));
  text.append(document.createTextNode(
    changes.length ? changes.join('; ') + '. ' : 'Saved port changes are not live yet. ',
  ));
  text.append(document.createTextNode('They apply on the next restart.'));
  box.append(text);

  if (state.restarting) {
    box.append(h('span', { class: 'count' }, 'restarting…'));
  } else {
    const btn = h('button', { class: 'btn btn-sm', type: 'button' }, 'Restart now');
    btn.addEventListener('click', () => restartFlow());
    box.append(btn);
  }
  bannerSlot.append(box);
}

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

const poller = createPoller({ tick: refreshHealth, intervalMs: 5000 });

// A 401 from any authenticated call lands here exactly once, centrally (see api.js).
onUnauthorized(() => {
  state.session = { authenticated: false, password_required: state.session?.password_required ?? true };
  showLogin('Your session has expired — please sign in again.');
});

document.getElementById('logout-btn').addEventListener('click', async () => {
  try { await api.post('/api/logout'); } catch { /* logging out of a dead session is fine */ }
  state.session = { authenticated: false, password_required: state.session?.password_required ?? false };
  if (state.session.password_required) showLogin();
  else { toast.info('Logged out.'); await enterApp(); }
});

document.getElementById('topbar-health').addEventListener('click', () => {
  location.hash = '#/overview';
});

window.addEventListener('hashchange', () => {
  if (shell.hidden) return; // login owns the screen; the gate renders after success
  renderRoute();
});

function showBootError(err) {
  // Never leave both roots hidden: even an unexpected error during login/app startup must
  // produce a visible, actionable screen rather than a blank dark page.
  poller.stop();
  shell.hidden = false;
  loginRoot.hidden = true;
  clear(mainEl);
  const box = errorBlock(err);
  const retry = h('button', { class: 'btn btn-primary', type: 'button' }, 'Retry');
  retry.addEventListener('click', () => location.reload());
  box.append(h('div', { class: 'dialog-actions' }, retry));
  mainEl.append(box);
  document.getElementById('topbar-title').textContent = 'Manarouter';
}

(async function boot() {
  try {
    state.session = await api.get('/api/session');
    if (state.session.password_required && !state.session.authenticated) showLogin();
    else await enterApp();
  } catch (err) {
    // Covers both a failed session request and errors later in login/app startup.
    showBootError(err);
  }
})();
