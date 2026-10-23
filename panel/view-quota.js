// view-quota.js — cached upstream quota windows, refreshed only on operator action (or opt-in load).
//
// Provider snapshots and observed gateway traffic are different things. This view only renders
// UsageQuota snapshots from GET /api/quota; it never turns the historical usage aggregate into a
// provider allowance.

import { api } from './api.js';
import { h, emptyState, errorBlock, toggle } from './dom.js';
import {
  QUOTA_FAMILIES, formatQuotaAge, formatQuotaUsage, formatPercent, formatResetIn,
} from './format.js';

const DEFAULT_DISPLAY = Object.freeze({ mode: 'chunks', chunks: 20, reset_on_load: false });

export async function renderQuota(mount, ctx) {
  let providers = [];
  let config = null;
  let display = { ...DEFAULT_DISPLAY };
  let quotaError = null;
  let configError = null;
  let usage = null;
  let usageError = null;
  let records = new Map();
  let refreshAllBusy = false;
  const busyAccounts = new Set();
  const configFieldAvailable = { value: false };
  const localErrors = new Map();

  mount.replaceChildren(h('div', { class: 'card' }, 'Loading provider quota…'));
  const [providerResult, configResult, quotaResult, usageResult] = await Promise.allSettled([
    ctx.loadProviders(),
    api.get('/api/config'),
    api.get('/api/quota'),
    api.get('/api/usage?limit=20'),
  ]);

  if (usageResult.status === 'fulfilled') usage = usageResult.value ?? null;
  else usageError = `Could not load request usage: ${usageResult.reason?.message || 'request failed'}`;

  if (providerResult.status === 'fulfilled') providers = providerResult.value ?? [];
  else quotaError = `Could not load accounts: ${providerResult.reason?.message || 'request failed'}`;

  if (configResult.status === 'fulfilled') {
    config = configResult.value?.config ?? configResult.value;
    const saved = config?.quota?.display;
    if (saved && Object.hasOwn(saved, 'reset_on_load')) {
      configFieldAvailable.value = true;
      display = {
        mode: saved.mode === 'percentage' ? 'percentage' : 'chunks',
        chunks: finitePositiveInteger(saved.chunks, DEFAULT_DISPLAY.chunks),
        reset_on_load: saved.reset_on_load === true,
      };
    }
  } else {
    configError = configResult.reason?.message || 'Could not read panel configuration.';
  }

  if (quotaResult.status === 'fulfilled') {
    records = recordsFrom(quotaResult.value);
  } else {
    quotaError = [quotaError, `Could not load cached provider quota: ${quotaResult.reason?.message || 'request failed'}`]
      .filter(Boolean).join(' ');
  }

  let accountRows = buildAccountRows(providers, records);
  render();

  // The default is off. Only the documented config field can opt into a provider request on view load.
  if (display.reset_on_load) await refreshAll();

  function render() {
    const toolbar = h('div', { class: 'toolbar' });
    const refreshButton = h('button', {
      class: 'btn primary',
      type: 'button',
      disabled: refreshAllBusy || busyAccounts.size > 0,
      onclick: refreshAll,
    }, refreshAllBusy ? 'Refreshing…' : 'Refresh all');
    toolbar.append(refreshButton);

    if (configFieldAvailable.value) {
      const loadToggle = toggle('Refresh on entering Quota', {
        checked: display.reset_on_load,
        disabled: refreshAllBusy || busyAccounts.size > 0,
        hint: 'Off by default. When enabled, opening this page contacts each provider account.',
        onChange: (event) => saveRefreshOnLoad(event.currentTarget.checked),
      });
      toolbar.append(loadToggle.row);
    }

    const children = [toolbar];
    if (quotaError) children.push(errorBlock({ message: quotaError }));
    if (configError) children.push(errorBlock({ message: `Quota display settings unavailable: ${configError}` }));
    if (!configError && !configFieldAvailable.value) {
      children.push(h('p', { class: 'hint' },
        'Refresh-on-load is unavailable: the config API did not return quota.display.reset_on_load.'));
    }
    children.push(h('p', { class: 'quota-legend' },
      h('span', { class: 'swatch', 'aria-hidden': 'true' }), ' remaining', ' · gray = used'));

    children.push(renderUsage());

    if (!accountRows.length) {
      children.push(emptyState('No provider accounts found.',
        'Add an account before fetching provider quota.'));
    } else {
      for (const row of accountRows) children.push(renderAccount(row));
    }
    mount.replaceChildren(...children);
  }

  // The request ledger: what the client keys spent, with the token count reported by the provider
  // or estimated by the router. The estimate is computed in the panel process, so this page never
  // has to ask the relay for a number it does not have.
  function renderUsage() {
    const card = h('section', { class: 'card' });
    card.append(h('h2', {}, 'Recent request usage'));
    if (usageError) {
      card.append(errorBlock({ message: usageError }));
      return card;
    }
    const rows = Array.isArray(usage?.rows) ? usage.rows : [];
    if (!rows.length) {
      card.append(h('p', { class: 'hint' },
        'No request usage recorded yet. A request that carries a client key is recorded once it settles.'));
      return card;
    }
    const table = h('table', { class: 'tbl' },
      h('thead', {}, h('tr', {},
        h('th', { scope: 'col' }, 'When'),
        h('th', { scope: 'col' }, 'Key'),
        h('th', { scope: 'col' }, 'Prompt'),
        h('th', { scope: 'col' }, 'Completion'),
        h('th', { scope: 'col' }, 'Cached'),
        h('th', { scope: 'col' }, 'Cache write'),
        h('th', { scope: 'col' }, 'tokens/s'),
        h('th', { scope: 'col' }, 'Source'))));
    const body = h('tbody');
    for (const row of rows) {
      body.append(h('tr', {},
        h('td', { class: 'mono' }, formatUsageTime(row?.occurred_at)),
        h('td', { class: 'mono' }, row?.client_key_id ?? '(unknown)'),
        h('td', { class: 'num' }, formatTokens(row?.prompt_tokens)),
        h('td', { class: 'num' }, formatTokens(row?.completion_tokens)),
        // The cache split and the rate are shown as em dashes when the provider did not report
        // them. A zero would claim the provider said "no cache", which is a different fact from
        // silence, and the distinction is the whole reason these columns are nullable.
        h('td', { class: 'num' }, formatTokens(row?.cached_tokens)),
        h('td', { class: 'num' }, formatTokens(row?.cache_write_tokens)),
        h('td', { class: 'num' }, formatRate(row?.tokens_per_second)),
        h('td', {}, row?.estimated ? 'estimated by the router' : 'reported by the provider')));
    }
    table.append(body);
    card.append(table);
    const totals = usage?.totals ?? {};
    card.append(h('p', { class: 'hint' },
      `${totals.requests ?? rows.length} request(s), `
      + `${formatTokens(totals.prompt_tokens)} prompt tokens, `
      + `${formatTokens(totals.completion_tokens)} completion tokens`
      + (totals.cached_tokens ? `, ${formatTokens(totals.cached_tokens)} from cache` : '')
      + (totals.cache_write_tokens ? `, ${formatTokens(totals.cache_write_tokens)} written to cache` : '')
      + (totals.estimated_requests ? `, ${totals.estimated_requests} estimated` : '')));
    return card;
  }

  function renderAccount(row) {
    const { provider, account, key } = row;
    const record = records.get(key);
    const label = account.label || account.identity || record?.label || account.id || record?.account || 'Account';
    const title = `${provider.id} · ${label}`;
    const card = h('section', { class: 'card quota-account' });
    const header = h('div', { class: 'toolbar' }, h('h2', {}, title));
    const refreshButton = h('button', {
      class: 'btn',
      type: 'button',
      disabled: refreshAllBusy || busyAccounts.has(key),
      onclick: () => refreshOne(row),
    }, busyAccounts.has(key) ? 'Refreshing…' : 'Refresh account');
    header.append(refreshButton);
    card.append(header);

    const source = record?.source || firstWindowSource(record?.windows);
    const details = displayDetails(record?.details, record?.windows);
    const meta = [formatQuotaAge(record?.fetched_at)];
    if (source) meta.push(`Source: ${source}`);
    if (details.length) meta.push(`Details: ${details.join(' · ')}`);
    card.append(h('p', { class: 'hint' }, meta.join(' · ')));

    const error = localErrors.get(key) || record?.error;
    if (error) card.append(errorBlock({ message: errorMessage(error) }));

    // The endpoint may include declared family placeholders before the first fetch; those are not
    // upstream observations. Only render usage fields once this account has a cached fetch time.
    const windows = record?.fetched_at != null && Array.isArray(record?.windows) ? record.windows : [];
    for (const family of QUOTA_FAMILIES) {
      const window = windows.find((candidate) => familyLabel(candidate?.family) === family);
      card.append(renderWindow(family, window));
    }
    for (const window of windows) {
      if (!QUOTA_FAMILIES.includes(familyLabel(window?.family))) {
        card.append(renderWindow(familyLabel(window?.family) || 'Other window', window));
      }
    }
    if (!windows.length && !error) {
      card.append(h('p', { class: 'hint' }, 'No provider quota snapshot cached. Use Refresh account to fetch one.'));
    }
    return card;
  }

  function renderWindow(label, window) {
    const row = h('div', { class: 'quota-window' });
    row.append(h('strong', {}, label));
    const info = h('div', { class: 'quota-window-usage' });
    const usage = formatQuotaUsage(window, display);
    if (usage.kind === 'unlimited') {
      info.append(h('span', { class: 'chunks-na' }, 'Unlimited'));
    } else if (usage.kind === 'unavailable') {
      info.append(h('span', { class: 'chunks-na' }, 'N/A'));
    } else if (usage.kind === 'percentage') {
      const bar = percentageBar(usage.remainingPercent, usage.usedPercent);
      info.append(bar, h('span', { class: 'quota-pct' },
        `${formatPercent(usage.remainingPercent)} remaining · ${formatPercent(usage.usedPercent)} used`));
    } else {
      const cells = h('div', { class: 'chunks', role: 'img',
        'aria-label': `${formatPercent(usage.remainingPercent)} remaining, ${formatPercent(usage.usedPercent)} used` });
      for (let index = 0; index < usage.total; index += 1) {
        const remainingCells = usage.remaining;
        const isHalf = usage.half && index === remainingCells;
        cells.append(h('span', { class: `chunk${index < remainingCells ? ' on' : isHalf ? ' half' : ''}` }));
      }
      info.append(cells, h('span', { class: 'quota-pct' },
        `${formatPercent(usage.remainingPercent)} remaining · ${formatPercent(usage.usedPercent)} used`));
    }
    row.append(info);
    const sub = [];
    if (window?.display_name) sub.push(window.display_name);
    if (window?.source) sub.push(`Source: ${window.source}`);
    if (window?.reset_at != null) sub.push(formatResetIn(window.reset_at));
    else sub.push('reset time unknown');
    if (window?.details?.length) {
      sub.push(...window.details.map((detail) => detail?.name
        ? `${detail.name}: ${detail.used ?? 'N/A'}`
        : String(detail)));
    }
    row.append(h('p', { class: 'hint' }, sub.join(' · ')));
    return row;
  }

  function percentageBar(remaining, used) {
    const bar = h('div', { class: 'quota-percentage-bar', role: 'img',
      'aria-label': `${formatPercent(remaining)} remaining, ${formatPercent(used)} used` });
    Object.assign(bar.style, {
      display: 'flex', width: 'min(100%, 320px)', height: '14px', margin: '4px 0',
      overflow: 'hidden', borderRadius: '3px', background: 'var(--bg3)',
      border: '1px solid var(--line2)', boxSizing: 'border-box',
    });
    const fill = h('span', { 'aria-hidden': 'true' });
    Object.assign(fill.style, {
      display: 'block', width: `${Math.min(100, Math.max(0, remaining))}%`,
      flex: 'none', background: 'var(--pink)',
    });
    bar.append(fill);
    return bar;
  }

  async function refreshOne(row) {
    const { provider, account, key } = row;
    if (refreshAllBusy || busyAccounts.has(key)) return;
    busyAccounts.add(key);
    localErrors.delete(key);
    render();
    try {
      const data = await api.post(`/api/providers/${encodeURIComponent(provider.id)}/accounts/${encodeURIComponent(account.id)}/quota/refresh`);
      mergeRecords(recordsFrom(data), key);
      accountRows = buildAccountRows(providers, records);
      if (!records.get(key)?.error) localErrors.delete(key);
    } catch (err) {
      localErrors.set(key, err?.message || 'Quota refresh failed.');
    } finally {
      busyAccounts.delete(key);
      render();
    }
  }

  async function refreshAll() {
    if (refreshAllBusy || busyAccounts.size > 0) return;
    refreshAllBusy = true;
    for (const key of records.keys()) localErrors.delete(key);
    render();
    try {
      const data = await api.post('/api/quota/refresh');
      mergeRecords(recordsFrom(data));
      accountRows = buildAccountRows(providers, records);
      quotaError = null;
    } catch (err) {
      quotaError = `Refresh all failed: ${err?.message || 'request failed'}`;
    } finally {
      refreshAllBusy = false;
      render();
    }
  }

  function mergeRecords(incoming, onlyKey = null) {
    for (const [key, next] of incoming) {
      if (onlyKey && key !== onlyKey) continue;
      const previous = records.get(key);
      // A per-account error must not erase its last good cached snapshot.
      if (next.error && !Array.isArray(next.windows) && previous) {
        records.set(key, { ...previous, ...next, windows: previous.windows });
      } else {
        records.set(key, next);
      }
      if (next.error) localErrors.set(key, errorMessage(next.error));
      else localErrors.delete(key);
    }
  }

  async function saveRefreshOnLoad(enabled) {
    if (!config || !configFieldAvailable.value) return;
    const previous = display.reset_on_load;
    display.reset_on_load = Boolean(enabled);
    render();
    try {
      const next = JSON.parse(JSON.stringify(config));
      next.quota ??= {};
      next.quota.display ??= {};
      next.quota.display.reset_on_load = display.reset_on_load;
      const saved = await api.put('/api/config', next);
      config = saved?.config ?? saved ?? next;
      display.reset_on_load = config?.quota?.display?.reset_on_load === true;
      configError = null;
    } catch (err) {
      display.reset_on_load = previous;
      configError = `Could not save refresh-on-load setting: ${err?.message || 'request failed'}`;
    }
    render();
  }
}

// A token count that is not known is not zero: it is shown as a dash.
function formatTokens(value) {
  return value == null ? '—' : String(value);
}

// A rate is reported to one decimal place; `null` means the provider gave no timing, so there is
// no rate to state and the cell says so rather than showing a zero.
function formatRate(value) {
  if (value == null || !Number.isFinite(Number(value))) return '—';
  return Number(value).toFixed(1);
}

// The ledger stores milliseconds since the epoch; the panel shows it in the local timezone.
function formatUsageTime(millis) {
  if (millis == null) return '—';
  const date = new Date(Number(millis));
  if (Number.isNaN(date.getTime())) return String(millis);
  return date.toLocaleString();
}

function recordsFrom(data) {
  const entries = Array.isArray(data) ? data
    : Array.isArray(data?.accounts) ? data.accounts
      : Array.isArray(data?.results) ? data.results
        : data?.account != null && (Array.isArray(data?.windows) || data?.error) ? [data]
          : [];
  const result = new Map();
  for (const entry of entries) {
    if (!entry || typeof entry !== 'object') continue;
    // The pre-F1 GET /api/quota returned historical gateway-usage totals. Those are not provider
    // quota snapshots, so do not surface them as if they were cached allowance data.
    if (!Array.isArray(entry.windows) && entry.error == null && !Object.hasOwn(entry, 'fetched_at')) continue;
    const provider = typeof entry.provider === 'string' ? entry.provider : entry.provider?.id;
    const account = typeof entry.account === 'string' ? entry.account : entry.account?.id;
    if (!provider || !account) continue;
    result.set(recordKey(provider, account), { ...entry, provider, account });
  }
  return result;
}

function buildAccountRows(providers, records) {
  const rows = [];
  const seen = new Set();
  for (const provider of providers) {
    for (const account of provider.accounts ?? []) {
      if (!account?.id) continue;
      const key = recordKey(provider.id, account.id);
      if (seen.has(key)) continue;
      seen.add(key);
      rows.push({ provider, account, key });
    }
  }
  for (const [key, record] of records) {
    if (seen.has(key)) continue;
    const provider = { id: record.provider || '(unknown provider)' };
    const account = {
      id: record.account,
      label: record.label || record.account,
    };
    rows.push({ provider, account, key });
  }
  return rows;
}

function recordKey(provider, account) {
  return `${provider}\u0000${account}`;
}

function familyLabel(family) {
  if (typeof family === 'string') return family;
  if (family && typeof family === 'object' && family.other) {
    const secs = family.other.secs;
    return Number.isFinite(secs) ? `other (${secs}s)` : 'other';
  }
  return '';
}

function displayDetails(accountDetails, windows) {
  const seen = new Set();
  const result = [];
  const details = [...(Array.isArray(accountDetails) ? accountDetails : [])];
  for (const window of windows ?? []) details.push(...(window?.details ?? []));
  for (const detail of details) {
    if (!detail || typeof detail !== 'object') continue;
    const text = detail.name ? `${detail.name}: ${detail.used ?? 'N/A'}` : '';
    if (text && !seen.has(text)) { seen.add(text); result.push(text); }
  }
  return result;
}

function firstWindowSource(windows) {
  return windows?.find((window) => window?.source)?.source || null;
}

function errorMessage(error) {
  if (typeof error === 'string') return error;
  return error?.message || error?.error?.message || 'Quota refresh failed for this account.';
}

function finitePositiveInteger(value, fallback) {
  const number = Number(value);
  return Number.isFinite(number) && number > 0 ? Math.min(200, Math.floor(number)) : fallback;
}
