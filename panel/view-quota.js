// view-quota.js — per-account usage and declared quota windows.
//
// Two distinct data sources, kept honest:
//  * DECLARED windows come from the configuration (Account.quota in mr-core) — real data,
//    rendered as soon as providers load.
//  * OBSERVED usage would need a quota endpoint. The API contract has none yet, so the panel
//    tries GET /api/quota once and, on any failure, says exactly that — no fake numbers, ever.

import { api } from './api.js';
import { h, chip, emptyState, errorBlock } from './dom.js';
import { formatWindowLimits, formatDuration } from './format.js';

export async function renderQuota(mount, ctx) {
  const declaredBox = h('div');
  const usageBox = h('div');
  mount.replaceChildren(
    h('div', { class: 'card' }, h('h2', {}, 'Declared windows (from configuration)'), declaredBox),
    h('div', { class: 'card' }, h('h2', {}, 'Observed usage'), usageBox),
  );

  // --- declared windows -------------------------------------------------

  try {
    const providers = await ctx.loadProviders();
    const rows = [];
    for (const p of providers ?? []) {
      for (const a of p.accounts ?? []) {
        for (const qw of a.quota ?? []) {
          rows.push({ provider: p.id, account: a, qw });
        }
        // Provider-level windows apply to the whole provider.
        for (const qw of p.quota ?? []) {
          rows.push({ provider: p.id, account: null, qw });
        }
      }
    }
    if (!rows.length) {
      declaredBox.append(emptyState('No quota windows declared.',
        'Accounts can carry rolling or fixed windows (request/token limits with reset periods) in the configuration.'));
    } else {
      const tbody = h('tbody');
      for (const { provider, account, qw } of rows) {
        tbody.append(h('tr', {},
          h('td', { class: 'mono' }, provider),
          h('td', { class: 'mono' }, account ? (account.label ?? account.id ?? '—') : 'provider-wide'),
          h('td', {}, qw.name ?? '—'),
          h('td', {}, chip(typeof qw.kind === 'string' ? qw.kind : 'window', 'chip-neutral', 'window kind')),
          h('td', { class: 'num' }, qw.seconds != null ? formatDuration(qw.seconds) : '—'),
          h('td', {}, formatWindowLimits(qw))));
      }
      declaredBox.append(h('div', { class: 'table-scroll' },
        h('table', { class: 'tbl' },
          h('thead', {}, h('tr', {},
            h('th', { scope: 'col' }, 'Provider'),
            h('th', { scope: 'col' }, 'Account'),
            h('th', { scope: 'col' }, 'Name'),
            h('th', { scope: 'col' }, 'Kind'),
            h('th', { scope: 'col' }, 'Length'),
            h('th', { scope: 'col' }, 'Limits'))),
          tbody)));
    }
  } catch (err) {
    declaredBox.replaceChildren(errorBlock(err));
  }

  // --- observed usage ---------------------------------------------------

  try {
    const usage = await api.get('/api/quota');
    usageBox.append(renderUsage(usage));
  } catch (err) {
    // Honest empty state: the endpoint does not exist in this build. Say so, plainly.
    usageBox.replaceChildren(h('div', { class: 'empty-state', style: 'text-align:left;' },
      h('div', {}, h('strong', {}, 'Observed usage is not available yet.')),
      h('p', {}, 'The control plane does not expose a quota endpoint in this build (GET /api/quota answered ',
        h('span', { class: 'mono' }, String(err.status ?? 'network error')), ').'),
      h('p', {}, 'Per-account usage is tracked in the state database and will appear here once the endpoint ships. ',
        'The windows above are the limits the configuration declares — that part is real.'),
    ));
  }
}

/**
 * Best-effort rendering of an unknown-but-successful usage payload: an array of flat objects is
 * turned into a table (columns = union of keys). Everything goes through text nodes, so even
 * unexpected server shapes cannot inject markup. A non-array payload renders as key/value pairs.
 */
function renderUsage(usage) {
  const rows = Array.isArray(usage) ? usage : (usage && typeof usage === 'object' ? [usage] : []);
  if (!rows.length) return emptyState('The quota endpoint answered, but returned no rows.');
  const cols = [...new Set(rows.flatMap((r) => (r && typeof r === 'object' ? Object.keys(r) : ['value'])))].slice(0, 10);
  const tbody = h('tbody');
  for (const r of rows.slice(0, 200)) {
    tbody.append(h('tr', {}, ...cols.map((c) => {
      const v = r?.[c];
      return h('td', { class: typeof v === 'string' && /^[\w.:/-]{6,}$/.test(v) ? 'mono' : '' },
        v === null || v === undefined ? '—' : typeof v === 'object' ? JSON.stringify(v) : String(v));
    })));
  }
  return h('div', { class: 'table-scroll' },
    h('table', { class: 'tbl' },
      h('thead', {}, h('tr', {}, ...cols.map((c) => h('th', { scope: 'col' }, c)))),
      tbody));
}
