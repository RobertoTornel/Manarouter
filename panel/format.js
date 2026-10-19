// format.js — pure, DOM-free helpers. Nothing here touches `document` or `fetch`, so every
// function can be tested with plain node (see panel/README.md, "Testing the pure functions").

// ---------- time ----------

/** 14520 → "4 h 2 m". Two most significant non-zero units; 0 → "0 s". */
export function formatUptime(secs) {
  if (!Number.isFinite(secs) || secs < 0) return '—';
  const s = Math.floor(secs);
  const d = Math.floor(s / 86400);
  const hh = Math.floor((s % 86400) / 3600);
  const mm = Math.floor((s % 3600) / 60);
  const ss = s % 60;
  const parts = [];
  if (d) parts.push(`${d} d`);
  if (hh) parts.push(`${hh} h`);
  if (mm) parts.push(`${mm} m`);
  if (!d && !hh && (ss || parts.length === 0)) parts.push(`${ss} s`);
  return parts.slice(0, 2).join(' ') || '0 s';
}

/** ISO string | epoch ms | null → "3 m ago" | "just now" | "never". */
export function formatAge(dateish, now = Date.now()) {
  if (dateish === null || dateish === undefined || dateish === '') return 'never';
  const t = typeof dateish === 'number' ? dateish : Date.parse(String(dateish));
  if (!Number.isFinite(t)) return 'unknown';
  const delta = Math.max(0, Math.round((now - t) / 1000));
  if (delta < 45) return 'just now';
  return `${formatUptime(delta)} ago`;
}

/** 3600 → "1 h"; 90 → "1 m 30 s". Used for quota window lengths. */
export function formatDuration(secs) {
  return formatUptime(secs);
}

/** 1234 → "1.2 k", 4500000 → "4.5 M". For request/token limits. */
export function formatCompact(n) {
  if (!Number.isFinite(n)) return '—';
  if (n < 1000) return String(n);
  if (n < 1_000_000) return `${trim1(n / 1000)} k`;
  if (n < 1_000_000_000) return `${trim1(n / 1_000_000)} M`;
  return `${trim1(n / 1_000_000_000)} G`;
}

function trim1(x) {
  return (Math.round(x * 10) / 10).toString();
}

/** "a1b2c3d4e5f6…" → "a1b2c3d4…5f6a1". Keeps digests scannable; full value in the title tooltip. */
export function truncateMiddle(s, head = 8, tail = 6) {
  const str = String(s ?? '');
  if (str.length <= head + tail + 1) return str;
  return `${str.slice(0, head)}…${str.slice(-tail)}`;
}

// ---------- lists & shapes ----------

/**
 * The panel must work whether an endpoint returns a bare array or an object wrapper
 * ({ providers: [...] }). Defensive on purpose: the backend is being built concurrently.
 */
export function extractList(data, ...keys) {
  if (Array.isArray(data)) return data;
  if (data && typeof data === 'object') {
    for (const k of keys) {
      if (Array.isArray(data[k])) return data[k];
    }
  }
  return [];
}

// ---------- semantic colours ----------
// Mapping tables so the chip decision is data, not scattered ternaries.
// GREEN = genuinely healthy · AMBER = warning/degraded/pending · RED = error/dead only ·
// neutral grey for intentional states (disabled is a choice, not a problem).

const HEALTH_GREEN = new Set(['healthy', 'ok', 'active', 'up', 'reachable', 'ready', 'connected']);
const HEALTH_AMBER = new Set(['degraded', 'warning', 'warn', 'rate-limited', 'rate_limited', 'ratelimited', 'cooling', 'cooldown', 'backoff', 'pending', 'starting', 'restarting', 'throttled']);
const HEALTH_RED = new Set(['dead', 'error', 'unhealthy', 'failed', 'down', 'unreachable', 'auth-failed', 'auth_failed']);

/** Known health word → chip class; anything unknown stays neutral grey. */
export function healthChipClass(value) {
  const v = String(value ?? '').toLowerCase();
  if (HEALTH_GREEN.has(v)) return 'chip-green';
  if (HEALTH_AMBER.has(v)) return 'chip-amber';
  if (HEALTH_RED.has(v)) return 'chip-red';
  return 'chip-neutral';
}

/** One account's observed/configured state → {key, label, cls}. */
export function accountState(account) {
  if (!account || typeof account !== 'object') return { key: 'unknown', label: 'unknown', cls: 'chip-neutral' };
  const raw = account.state ?? account.status ?? null;
  if (typeof raw === 'string' && raw) {
    return { key: raw, label: raw, cls: healthChipClass(raw) };
  }
  // No observed state from the backend: fall back to the configured enabled flag.
  if (account.enabled === false) return { key: 'disabled', label: 'disabled', cls: 'chip-neutral' };
  return { key: 'enabled', label: 'enabled', cls: 'chip-green' };
}

/**
 * Provider-level health for the Providers table.
 *
 * Order of evidence: (1) a server-provided `health`/`status` string wins — it observes what the
 * panel cannot; (2) otherwise derive from configuration. Derivation rules mirror
 * docs/01-architecture.md's health semantics: a provider with every account disabled is an
 * intentional state, so it is neutral grey — deliberately NOT red, NOT amber, NOT green.
 * `no accounts` is likewise unconfigured, not broken. Only real failure words turn red.
 */
export function deriveProviderHealth(provider) {
  if (!provider || typeof provider !== 'object') return { key: 'unknown', label: 'unknown', cls: 'chip-neutral' };
  const server = provider.health ?? provider.health_state ?? provider.status;
  if (typeof server === 'string' && server) {
    return { key: server, label: server, cls: healthChipClass(server) };
  }
  if (provider.enabled === false) return { key: 'disabled', label: 'disabled', cls: 'chip-neutral' };
  const accounts = Array.isArray(provider.accounts) ? provider.accounts : [];
  if (accounts.length === 0) return { key: 'unconfigured', label: 'no accounts', cls: 'chip-neutral' };
  const enabled = accounts.filter((a) => a && a.enabled !== false);
  if (enabled.length === 0) return { key: 'accounts-disabled', label: 'accounts disabled', cls: 'chip-neutral' };
  // Observed per-account states (when the backend surfaces them) can degrade the picture.
  const states = enabled.map((a) => accountState(a).key);
  const isBad = (s) => HEALTH_RED.has(s.toLowerCase());
  const isWarn = (s) => HEALTH_AMBER.has(s.toLowerCase());
  if (states.every(isBad)) return { key: 'dead', label: 'dead', cls: 'chip-red' };
  if (states.some(isBad) || states.some(isWarn)) return { key: 'degraded', label: 'degraded', cls: 'chip-amber' };
  return { key: 'healthy', label: 'healthy', cls: 'chip-green' };
}

// ---------- validation problems ----------

/** Group raw `details` into { byField: {field: [messages]}, general: [strings] }. Pure. */
export function groupProblems(details) {
  const byField = {};
  const general = [];
  for (const item of details ?? []) {
    if (typeof item === 'string') { general.push(item); continue; }
    if (item && typeof item === 'object') {
      const f = item.field ?? item.path ?? item.param ?? item.key ?? null;
      const m = item.message ?? item.detail ?? item.error ?? 'invalid value';
      if (f != null) {
        (byField[String(f)] ??= []).push(String(m));
      } else {
        general.push(String(m));
      }
      continue;
    }
    general.push(String(item));
  }
  return { byField, general };
}

// ---------- ports / restart banner ----------

/** True when any configured port differs from the port the process is actually listening on. */
export function portsDirty(ports) {
  if (!ports || typeof ports !== 'object') return false;
  return ['relay', 'control'].some((k) => {
    const p = ports[k];
    return p && typeof p === 'object' && p.port != null && p.running_port != null && Number(p.port) !== Number(p.running_port);
  });
}

/**
 * Human list of what a port change will move, for the banner and the settings preview.
 * Empty array = nothing pending.
 */
export function describePortChange(ports) {
  if (!ports || typeof ports !== 'object') return [];
  const out = [];
  const rel = ports.relay ?? {};
  const ctl = ports.control ?? {};
  if (rel.port != null && rel.running_port != null && Number(rel.port) !== Number(rel.running_port)) {
    out.push(`relay will listen on ${rel.host ?? '·'}:${rel.port} instead of ${rel.host ?? '·'}:${rel.running_port}`);
  }
  if (ctl.port != null && ctl.running_port != null && Number(ctl.port) !== Number(ctl.running_port)) {
    out.push(`control plane will listen on ${ctl.host ?? '·'}:${ctl.port} instead of ${ctl.host ?? '·'}:${ctl.running_port} — this panel moves with it`);
  }
  return out;
}

/**
 * Where a reachability probe for the relay should be aimed. Listen hosts like "0.0.0.0" or "::"
 * mean "all interfaces"; a browser cannot dial those, so probe loopback instead. Returns null
 * when there is no port to probe (the caller then shows an honest unknown state).
 */
export function relayProbeUrl(relay) {
  if (!relay || typeof relay !== 'object') return null;
  const port = relay.running_port ?? relay.port;
  if (port == null || port === '') return null;
  let host = relay.host ?? '';
  if (host === '' || host === '0.0.0.0' || host === '::' || host === '[::]') host = '127.0.0.1';
  if (host.includes(':')) host = `[${host}]`; // bare IPv6 literal
  return `http://${host}:${port}/`;
}

// ---------- combo mechanics ----------
// The step list is the operator's priority order and is NEVER sorted by the panel. Reordering is
// done exclusively by moveStep (drag or buttons), which moves one element and leaves the rest of
// the order untouched — that property is why it is a pure function here and not a sort call.

/** New array with the element at `from` relocated to index `to`. Nothing is ever sorted. */
export function moveStep(arr, from, to) {
  const n = arr.length;
  if (!Number.isInteger(from) || !Number.isInteger(to)) return arr.slice();
  if (from < 0 || to < 0 || from >= n || to >= n || from === to) return arr.slice();
  const out = arr.slice();
  const [item] = out.splice(from, 1);
  out.splice(to, 0, item);
  return out;
}

/** Where does index `i` land after moveStep(from → to)? Participants and stable-step references
 *  are positional, so they must be remapped when a step moves — forgetting this would silently
 *  re-point the hedged race at the wrong models. */
export function remapIndex(i, from, to) {
  if (!Number.isInteger(i)) return i;
  if (i === from) return to;
  if (from < to && i > from && i <= to) return i - 1;
  if (to < from && i >= to && i < from) return i + 1;
  return i;
}

export function remapParticipants(list, from, to) {
  return [...new Set((list ?? []).map((i) => remapIndex(i, from, to)))].sort((a, b) => a - b);
}

/** Step display label; accepts plain strings defensively (backend may simplify). */
export function stepLabel(step) {
  if (step == null) return '—';
  if (typeof step === 'string') return step;
  if (typeof step === 'object') {
    const p = step.provider ?? step.provider_id ?? '';
    const m = step.model ?? step.model_id ?? step.combo ?? '';
    return [p, m].filter(Boolean).join('/') || (step.id ?? '—');
  }
  return String(step);
}

/**
 * hedge.on_winner_fail is an externally tagged enum: {"rerun-excluding":{"max_rounds":2}} |
 * {"pick-highest-passed":null} | {"stable-fallback":{"step":1}}. Tolerate the bare-string form
 * too so the panel keeps working if the backend simplifies it.
 */
export function parseWinnerFail(v) {
  if (typeof v === 'string') {
    return { mode: v, maxRounds: v === 'rerun-excluding' ? 2 : null, step: null };
  }
  if (v && typeof v === 'object') {
    const mode = Object.keys(v)[0];
    const inner = v[mode];
    if (mode === 'rerun-excluding') return { mode, maxRounds: Number(inner?.max_rounds ?? 2), step: null };
    if (mode === 'stable-fallback') return { mode, maxRounds: null, step: Number(inner?.step ?? 0) };
    return { mode, maxRounds: null, step: null };
  }
  return { mode: 'rerun-excluding', maxRounds: 2, step: null };
}

/** Inverse of parseWinnerFail — canonical wire shape. */
export function buildWinnerFail(mode, { maxRounds = 2, step = 0 } = {}) {
  switch (mode) {
    case 'rerun-excluding': return { 'rerun-excluding': { max_rounds: Math.max(1, Math.floor(Number(maxRounds) || 1)) } };
    case 'stable-fallback': return { 'stable-fallback': { step: Math.max(0, Math.floor(Number(step) || 0)) } };
    case 'pick-highest-passed': return { 'pick-highest-passed': null };
    case 'pick-highest-passed-different-provider': return { 'pick-highest-passed-different-provider': null };
    default: return { 'rerun-excluding': { max_rounds: 2 } };
  }
}

/** True when a step is a hedged-race participant (participants = explicit step indices). */
export function isParticipant(participants, idx) {
  return (participants ?? []).includes(idx);
}

export function toggleParticipant(participants, idx) {
  const set = new Set(participants ?? []);
  if (set.has(idx)) set.delete(idx); else set.add(idx);
  return [...set].sort((a, b) => a - b);
}

// ---------- keys / allow-lists ----------

export function linesToList(text) {
  return String(text ?? '')
    .split('\n')
    .map((l) => l.trim())
    .filter(Boolean);
}

export function listToLines(list) {
  return (list ?? []).join('\n');
}

/** One QuotaWindow (config-declared) → "50 req · 200 k tok / 1 h". */
export function formatWindowLimits(qw) {
  if (!qw || typeof qw !== 'object') return '—';
  const bits = [];
  if (qw.limit_requests) bits.push(`${formatCompact(qw.limit_requests)} req`);
  if (qw.limit_tokens) bits.push(`${formatCompact(qw.limit_tokens)} tok`);
  if (qw.limit_input_tokens) bits.push(`${formatCompact(qw.limit_input_tokens)} in`);
  if (qw.limit_output_tokens) bits.push(`${formatCompact(qw.limit_output_tokens)} out`);
  if (!bits.length) bits.push('reset tracking only');
  const kind = typeof qw.kind === 'string' ? qw.kind : '';
  const secs = qw.seconds != null ? ` / ${formatDuration(qw.seconds)}` : '';
  return `${bits.join(' · ')}${secs}${qw.reset_only ? ' (reset only)' : ''}${kind ? ` — ${kind}` : ''}`;
}

// ---------- quota rendering (Feature 1) ----------

/** The three window families the panel shows, in the order it shows them. */
export const QUOTA_FAMILIES = ['5h', '1w', '30d'];

/**
 * How many chunks of `chunks` a remaining fraction paints, and whether the last one is half.
 *
 * Rounds HALF UP on the count rather than to the nearest cell: a window with 1% left must still
 * paint one cell, because a bar that renders completely empty while quota remains reads as
 * "exhausted" and would stop an operator using the last of it. The half-cell exists so a small
 * remainder is still distinguishable from a large one.
 *
 * Returns `null` — not zero — when no fraction was reported, which is the caller's signal to
 * render `N/A` instead of an empty bar.
 */
export function chunkFill(fraction, chunks) {
  if (fraction == null || !Number.isFinite(fraction)) return null;
  const total = Math.max(1, Math.floor(Number(chunks) || 1));
  const f = Math.min(1, Math.max(0, fraction));
  if (f <= 0) return { full: 0, half: false };
  const exact = f * total;
  const full = Math.floor(exact);
  const rest = exact - full;
  if (full === 0) return { full: 0, half: true };
  // The leftover shows as a half cell once it reaches half a cell. So 40% of 20 (exactly 8) is
  // eight cells, 42.5% (8.5) is eight and a half, and anything below half a cell rounds down
  // rather than painting a sliver as a whole cell.
  return { full, half: rest >= 0.5 };
}

/** `96.4%` — one decimal, because whole percents hide a 0.4% that matters near exhaustion. */
export function formatPercent(pct) {
  if (pct == null || !Number.isFinite(pct)) return 'N/A';
  return `${pct.toFixed(1)}%`;
}

/** `in 2 h 15 m`, `in 40 s`, or `reset time unknown`. Never invents a time. */
export function formatResetIn(resetAtMs, now = Date.now()) {
  if (resetAtMs == null || !Number.isFinite(resetAtMs)) return 'reset time unknown';
  const secs = Math.round((resetAtMs - now) / 1000);
  if (secs <= 0) return 'resets now';
  return `resets in ${formatDuration(secs)}`;
}

/**
 * A cached quota snapshot's age, honestly phrased. `never fetched` is a different state from
 * `fetched just now`, and a missing timestamp stays unknown rather than being replaced with now.
 */
export function formatQuotaAge(fetchedAt, now = Date.now()) {
  if (fetchedAt === null || fetchedAt === undefined || fetchedAt === '') return 'Never fetched';
  const age = formatAge(fetchedAt, now);
  if (age === 'unknown') return 'Fetch time unknown';
  return `Fetched ${age}`;
}

/**
 * Return an honest rendering model for one provider quota window.
 *
 * `fraction_reported` is authoritative: its false/missing state means the numeric zeroes are
 * sentinels, not a confirmed empty quota. The caller can render `N/A` without guessing.
 */
export function formatQuotaUsage(quota, { mode = 'chunks', chunks = 20 } = {}) {
  if (quota?.fraction_reported !== true) return { kind: 'unavailable' };
  if (quota?.unlimited === true) return { kind: 'unlimited' };

  let percent;
  if (Number.isFinite(quota.remaining_percentage) && quota.total > 0) {
    percent = Math.min(100, Math.max(0, quota.remaining_percentage));
  } else if (Number.isFinite(quota.remaining) && Number.isFinite(quota.total) && quota.total > 0) {
    percent = Math.min(100, Math.max(0, (quota.remaining / quota.total) * 100));
  } else {
    return { kind: 'unavailable' };
  }

  const remainingFraction = percent / 100;
  if (mode === 'percentage') {
    return {
      kind: 'percentage',
      label: formatPercent(percent),
      remainingPercent: percent,
      usedPercent: 100 - percent,
    };
  }

  const total = Math.max(1, Math.floor(Number(chunks) || 1));
  const fill = chunkFill(remainingFraction, total);
  const used = Math.max(0, total - fill.full - (fill.half ? 1 : 0));
  return {
    kind: 'chunks',
    total,
    remaining: fill.full,
    half: fill.half,
    used,
    remainingPercent: percent,
    usedPercent: 100 - percent,
  };
}
