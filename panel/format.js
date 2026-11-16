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
  const byField = Object.create(null);
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
 *
 * The path is `/healthz`, the relay's unauthenticated liveness route, rather than the root: the
 * root has no handler, so probing it logged a 404 on every visit to the overview and answered a
 * different question ("is something listening") than the one the panel asks ("is the relay up").
 */
export function relayProbeUrl(relay, pageHost) {
  if (!relay || typeof relay !== 'object') return null;
  const port = relay.running_port ?? relay.port;
  if (port == null || port === '') return null;
  let host = relay.host ?? '';
  const wildcard = host === '' || host === '0.0.0.0' || host === '::' || host === '[::]';
  // The panel and the relay run on the same machine, so the address this page was opened from is
  // also an address for the relay. That matters when the panel is used from another device (a
  // tailnet address): probing 127.0.0.1 would test the *browser's* loopback, not the server's, and
  // report a healthy relay as unreachable.
  const loopback = host === '127.0.0.1' || host === 'localhost' || host === '[::1]' || host === '::1';
  const pageIsRemote = typeof pageHost === 'string' && pageHost !== '' &&
    pageHost !== '127.0.0.1' && pageHost !== 'localhost' && pageHost !== '[::1]';
  if (wildcard || (loopback && pageIsRemote)) host = pageIsRemote ? pageHost : '127.0.0.1';
  if (host.includes(':') && !(host.startsWith('[') && host.endsWith(']'))) host = `[${host}]`;
  return `http://${host}:${port}/healthz`;
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
/**
 * The reasoning-level spellings the backend understands, in rank order.
 *
 * Mirrors `LEVEL_SUFFIXES` in `crates/mr-core/src/snapshot.rs`. `none` is included because it is a
 * real suffix — `antigravity/claude-opus-4-6-thinking-none` is a model id — even though it means
 * "do not reason".
 */
export const REASONING_LEVELS = ['none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'ultra', 'max'];

/** Keep only canonical level spellings, in the canonical ascending order. */
function canonicalLevels(list) {
  if (!Array.isArray(list)) return [];
  const present = new Set(list.filter((level) => REASONING_LEVELS.includes(level)));
  return REASONING_LEVELS.filter((level) => present.has(level));
}

/**
 * The levels a provider's dialect can send, as the backend reports them.
 *
 * The backend computes this from the dialect's wire vocabulary, so it is the same list the
 * validator accepts and the compiler offers. The control plane annotates *model entries* rather
 * than the provider object (it serialises the provider from mr-core, and an unknown provider-level
 * key would survive a round trip through the config's `flatten` catch-all), so the list is read from
 * the model the picker is editing when the provider itself does not carry it. Falling back to the
 * whole ladder keeps the panel usable against an older control plane, where no list is reported.
 */
export function providerLevels(provider, model = null) {
  const reported = canonicalLevels(
    provider?.dialect_reasoning_levels ?? model?.dialect_reasoning_levels,
  );
  return reported.length ? reported : REASONING_LEVELS;
}

/** The levels the model entry itself declares, which is what a save writes. */
export function declaredLevels(model) {
  return canonicalLevels(model?.reasoning_levels);
}

/**
 * The levels this model offers at runtime, ascending.
 *
 * `effective_reasoning_levels` comes from the same mr-core derivation the compiler uses (declared
 * list, then provider catalogue, then the dialect). An empty array is an answer, not missing data:
 * it means the model advertises no reasoning at all, so it must not fall through to the provider's
 * vocabulary. Without the field, the old behaviour is preserved: declared levels, else the
 * provider's.
 */
export function modelLevels(model, provider) {
  if (Array.isArray(model?.effective_reasoning_levels)) {
    return canonicalLevels(model.effective_reasoning_levels);
  }
  const declared = declaredLevels(model);
  return declared.length ? declared : providerLevels(provider, model);
}

/** The provider a combo step names, or `null`. */
export function stepProvider(step, providers) {
  if (!step || typeof step !== 'object') return null;
  const providerId = step.provider ?? step.provider_id;
  if (!providerId) return null;
  return (providers ?? []).find((candidate) => candidate?.id === providerId) ?? null;
}

/**
 * The provider's model entry for a combo step, or `null`.
 *
 * A step names a provider and a model; the reasoning capabilities live on the provider's model
 * entry, not on the step, so every level question has to go through this join.
 */
export function stepModel(step, providers) {
  if (!step || typeof step !== 'object') return null;
  const modelId = step.model ?? step.model_id;
  if (!modelId) return null;
  const provider = stepProvider(step, providers);
  const models = Array.isArray(provider?.models) ? provider.models : [];
  return models.find((candidate) => candidate?.id === modelId) ?? null;
}

/**
 * The provider/model pair a live-health probe can call, or `null` when the step has no single
 * target to probe.
 *
 * A model step is one tiny completion against that model. A combo-target step is not: it expands
 * to the nested combo's whole step list at compile time, so probing `provider: ""` / `model: ""`
 * would send a request that names nothing. Those steps are checked by opening the combo they
 * route to, where each real model step is listed.
 */
export function probeTarget(step) {
  if (!step || typeof step !== 'object' || step.combo) return null;
  const provider = String(step.provider ?? step.provider_id ?? '').trim();
  const model = String(step.model ?? step.model_id ?? '').trim();
  return provider && model ? { provider, model } : null;
}

/** True when `name` already ends in a reasoning-level suffix, as in `coding-high`. */
export function endsInLevelName(name) {
  if (typeof name !== 'string' || !name) return false;
  const dash = name.lastIndexOf('-');
  if (dash <= 0 || dash === name.length - 1) return false;
  return REASONING_LEVELS.includes(name.slice(dash + 1).toLowerCase());
}

/**
 * The level suffixes a combo can be called with (`coding-high`, `coding-low`, …).
 *
 * A **union** of the levels the steps' models offer at runtime, not an intersection: a step whose
 * model does not offer a level still takes part in `<combo>-<level>`, because the relay substitutes
 * that model's highest available level. Derived levels are included, because they are names the
 * relay really resolves; a model that offers no levels contributes none.
 */
export function comboLevels(steps, providers) {
  const offered = new Set();
  for (const step of steps ?? []) {
    const levels = modelLevels(stepModel(step, providers), stepProvider(step, providers));
    for (const level of levels) offered.add(level);
  }
  return REASONING_LEVELS.filter((level) => offered.has(level));
}

export function stepLabel(step) {
  if (step == null) return '—';
  if (typeof step === 'string') return step;
  if (typeof step === 'object') {
    // A combo target has no provider and no model: it is a route to another combo, marked so it
    // cannot be mistaken for a model id that happens to share the name.
    if (step.combo) return `→ ${step.combo}`;
    const p = step.provider ?? step.provider_id ?? '';
    const m = step.model ?? step.model_id ?? '';
    return [p, m].filter(Boolean).join('/') || (step.id ?? '—');
  }
  return String(step);
}

/**
 * hedge.on_winner_fail is an internally tagged enum on the wire:
 * {"mode":"rerun-excluding","max_rounds":2} |
 * {"mode":"pick-highest-passed"} |
 * {"mode":"stable-fallback","step":1}.
 * Tolerate the bare-string form too so the panel keeps working if the backend simplifies it.
 * The legacy externally tagged shape {"rerun-excluding":{...}} is also accepted on read so a
 * panel saved against an older build still round-trips.
 */
export function parseWinnerFail(v) {
  if (typeof v === 'string') {
    return { mode: v, maxRounds: v === 'rerun-excluding' ? 2 : null, step: null };
  }
  if (v && typeof v === 'object') {
    // Internally tagged wire form: {"mode": "...", "max_rounds": N, "step": N}
    if (typeof v.mode === 'string') {
      const mode = v.mode;
      const maxRounds = mode === 'rerun-excluding' ? Number(v.max_rounds ?? 2) : null;
      const step = mode === 'stable-fallback' ? Number(v.step ?? 0) : null;
      return { mode, maxRounds: Number.isFinite(maxRounds) ? maxRounds : null, step: Number.isFinite(step) ? step : null };
    }
    // Legacy externally tagged form: {"rerun-excluding":{"max_rounds":2}} — accepted on read only.
    const mode = Object.keys(v)[0];
    const inner = v[mode];
    if (mode === 'rerun-excluding') return { mode, maxRounds: Number(inner?.max_rounds ?? 2), step: null };
    if (mode === 'stable-fallback') return { mode, maxRounds: null, step: Number(inner?.step ?? 0) };
    return { mode, maxRounds: null, step: null };
  }
  return { mode: 'pick-highest-passed', maxRounds: null, step: null };
}

/** Inverse of parseWinnerFail — canonical internally tagged wire shape. */
export function buildWinnerFail(mode, { maxRounds = 2, step = 0 } = {}) {
  const base = { mode };
  if (mode === 'rerun-excluding') base.max_rounds = Math.max(1, Math.floor(Number(maxRounds) || 1));
  if (mode === 'stable-fallback') base.step = Math.max(0, Math.floor(Number(step) || 0));
  return base;
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

// ---------- account tags and step account selection ----------

/**
 * The tag field → the list the configuration stores.
 *
 * Tags are a small free-text note, so the field accepts the natural separators (comma or newline)
 * and nothing is rejected for shape. Case-insensitive duplicates collapse: `Alice, alice` is one
 * tag because that is how the server compares them, and keeping both would make a step selector
 * look ambiguous when it is not.
 */
export function parseTagList(text) {
  const seen = new Set();
  const out = [];
  for (const raw of String(text ?? '').split(/[,\n]/)) {
    const tag = raw.trim();
    if (!tag) continue;
    const key = tag.toLowerCase();
    if (seen.has(key)) continue;
    seen.add(key);
    out.push(tag);
  }
  return out;
}

export function tagListText(list) {
  return (Array.isArray(list) ? list : []).join(', ');
}

/**
 * Does one step selector name this account? Mirrors `mr_core::model::account_selector_matches`:
 * a `tag:` prefix is optional, the id is checked first, and the comparison is case-insensitive.
 */
export function selectorNames(selector, account) {
  if (!account) return false;
  const trimmed = String(selector ?? '').trim();
  if (!trimmed) return false;
  const bare = trimmed.slice(0, 4).toLowerCase() === 'tag:' ? trimmed.slice(4).trim() : trimmed;
  if (!bare) return false;
  const needle = bare.toLowerCase();
  if (String(account.id ?? '').toLowerCase() === needle) return true;
  return (Array.isArray(account.tags) ? account.tags : [])
    .some((tag) => String(tag ?? '').trim().toLowerCase() === needle);
}

/**
 * The accounts a step's selectors name, and the selectors that name nothing.
 *
 * An empty selection is not "no accounts": it is the default, *every* account of the provider.
 * `missing` is what the panel must warn about, because the server rejects the whole combo when a
 * selector matches nothing and the operator needs to see which one.
 */
export function stepAccountSelection(selectors, accounts) {
  const list = (Array.isArray(selectors) ? selectors : []).map((s) => String(s ?? '').trim()).filter(Boolean);
  const pool = Array.isArray(accounts) ? accounts : [];
  if (!list.length) return { selected: pool.slice(), missing: [], all: true };
  const selected = pool.filter((account) => list.some((selector) => selectorNames(selector, account)));
  const missing = list.filter((selector) => !pool.some((account) => selectorNames(selector, account)));
  return { selected, missing, all: false };
}

/** "every account" or "alice, work" — a step's selection in a few words. */
export function describeAccountSelection(selectors, accounts, { max = 3 } = {}) {
  const { selected, missing, all } = stepAccountSelection(selectors, accounts);
  if (all) return 'every account';
  const names = selected.map((a) => a.label || a.identity || a.id);
  const shown = names.slice(0, max).join(', ');
  const more = names.length > max ? ` +${names.length - max}` : '';
  const gaps = missing.length ? ` (unknown: ${missing.join(', ')})` : '';
  return (names.length ? `${shown}${more}` : 'no account') + gaps;
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

/** Whole seconds until a reset instant, or `null` when that instant is unknown. */
function resetInSeconds(resetAtMs, now) {
  if (resetAtMs == null || !Number.isFinite(resetAtMs)) return null;
  return Math.round((resetAtMs - now) / 1000);
}

/** `resets in 2 h 15 m`, `resets in 40 s`, or `reset time unknown`. Never invents a time. */
export function formatResetIn(resetAtMs, now = Date.now()) {
  const secs = resetInSeconds(resetAtMs, now);
  if (secs == null) return 'reset time unknown';
  if (secs <= 0) return 'resets now';
  return `resets in ${formatDuration(secs)}`;
}

/**
 * The same countdown as a label that can stand beside a percentage: `Resets in: 2 h 15 m`,
 * `Resets now`, or `Reset time unknown`. The panel puts it on the right of a window, where a
 * lower-case sentence would read as a footnote; the facts are identical to formatResetIn.
 */
export function formatResetInLabel(resetAtMs, now = Date.now()) {
  const secs = resetInSeconds(resetAtMs, now);
  if (secs == null) return 'Reset time unknown';
  if (secs <= 0) return 'Resets now';
  return `Resets in: ${formatDuration(secs)}`;
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

/**
 * The colour policy for a quota window, in one place.
 *
 * The thresholds are on what is REMAINING: above 50% is healthy, 50% or less is degraded, 20% or
 * less is critical. Every chip, bar and account border asks this function, so one number can never
 * be painted two ways on one page. A window the provider never quantified is "unknown" — which is
 * not the same as exhausted, and must not be painted red.
 */
export const QUOTA_STATUS_HEALTHY_ABOVE = 50;
export const QUOTA_STATUS_CRITICAL_AT_OR_BELOW = 20;

/** One of ok, alert, critical or unknown for a remaining percentage. */
export function quotaStatusLevel(remainingPercent) {
  const pct = remainingPercent == null ? NaN : Number(remainingPercent);
  if (!Number.isFinite(pct)) return 'unknown';
  if (pct <= QUOTA_STATUS_CRITICAL_AT_OR_BELOW) return 'critical';
  if (pct <= QUOTA_STATUS_HEALTHY_ABOVE) return 'alert';
  return 'ok';
}

/**
 * The worst level among several windows: critical beats alert beats ok, and nothing known stays
 * unknown. An account with one exhausted window is an account in trouble, whatever its other
 * windows say — so this never averages.
 */
export function worstQuotaStatus(levels) {
  let worst = 'unknown';
  for (const level of levels ?? []) {
    if (level === 'critical') return 'critical';
    if (level === 'alert') worst = 'alert';
    else if (level === 'ok' && worst === 'unknown') worst = 'ok';
  }
  return worst;
}

/** `5h`, `1w`, `30d`, or `other (3600s)` for a family the panel has no short name for. A family
 * the provider left blank returns an empty string, so callers can fall back to display_name. */
export function quotaFamilyLabel(family) {
  if (typeof family === 'string') return family;
  if (family && typeof family === 'object' && family.other) {
    const secs = family.other.secs;
    return Number.isFinite(secs) ? `other (${secs}s)` : 'other';
  }
  return '';
}

/**
 * The windows worth drawing, in the panel's family order.
 *
 * A provider that quantifies one window gets one row; the families it never mentioned are not
 * padded with N/A rows, because a row that only says "unknown" is noise an operator has to read
 * past. A window with no reported fraction is dropped for the same reason: silence is not a
 * measurement, so there is nothing to paint. `unlimited` is kept — that is an answer.
 *
 * Availability is decided from `fraction_reported`, not from the display mode, so this filter is
 * the same whether the panel asked for bars or chunks.
 */
export function quotaWindows(windows) {
  const order = new Map(QUOTA_FAMILIES.map((family, index) => [family, index]));
  const unranked = QUOTA_FAMILIES.length;
  return (Array.isArray(windows) ? windows : [])
    .filter((window) => window && typeof window === 'object')
    .map((window, index) => ({ window, index, usage: formatQuotaUsage(window) }))
    .filter(({ usage }) => usage.kind !== 'unavailable')
    .sort((a, b) => {
      const rankA = order.get(quotaFamilyLabel(a.window.family)) ?? unranked;
      const rankB = order.get(quotaFamilyLabel(b.window.family)) ?? unranked;
      return rankA === rankB ? a.index - b.index : rankA - rankB;
    })
    .map(({ window }) => window);
}

/**
 * Whether a cached /api/quota record earns a card.
 *
 * Only a quantified window — including an unlimited one — is content. An error is shown on the
 * card it belongs to; it never creates a card by itself. A record with no window to draw (never
 * fetched, a provider that reports no fraction, or a first refresh that failed) has nothing to
 * say about allowance and stays off the page. After a later refresh fails, mergeQuotaRecord keeps
 * the last-good window, so the error still has a card to sit on.
 */
export function quotaAccountVisible(record) {
  return quotaWindows(record?.windows).length > 0;
}

/**
 * Merge one refresh result over the cached record.
 *
 * A refresh that fails after returning an empty window list has not learned that the allowance
 * disappeared — it learned that the provider would not answer. Keep the last-good windows and the
 * time they were fetched, and let the new error sit beside them. A failed refresh that did return
 * windows replaces the snapshot normally; so does any successful refresh.
 */
export function mergeQuotaRecord(previous, next) {
  if (next?.error && !quotaWindows(next.windows).length && quotaWindows(previous?.windows).length) {
    return {
      ...previous,
      ...next,
      windows: previous.windows,
      fetched_at: next.fetched_at ?? previous.fetched_at,
    };
  }
  return next;
}

// ---------- quota probing ----------
// A provider with no dedicated quota fetcher can still be asked: the router walks a list of
// conventional balance/quota paths on the account's own base URL and reads the first one that
// answers (see `crates/mr-core/src/quota/probe.rs`). The panel edits that list in Settings, and
// shows which candidate answered on the Quota page.

/** The fetcher id the walk stamps on a reading it produced, before the candidate path. */
const PROBE_SOURCE_PREFIX = 'openai-compatible';

/**
 * The candidates a deployment walks until someone saves a list of their own.
 *
 * Mirrors `DEFAULT_QUOTA_PROBE_PATHS` in `crates/mr-core/src/quota/probe.rs`, which is also the
 * table in `docs/08-quota-tracking.md`. Settings shows these so the card is never blank and an
 * operator can see — and reset to — what a refresh would really try. The router's copy stays
 * authoritative; a test in `panel/test/quota-probe.test.mjs` pins this one to the documented order.
 */
export const QUOTA_PROBE_DEFAULT_PATHS = [
  '/v1/balance',
  '/api/balance',
  '/api/v1/key',
  '/v1/key',
  '/v1/user/info',
  '/v1/dashboard/billing/subscription',
  '/v1/credits',
  '/v1/quota',
  '/v1/usage',
];

/** The one candidate that is a POST, tried after every path (`CHAT_COMPLETIONS_PATH`). */
export const QUOTA_PROBE_CHAT_PATH = '/v1/chat/completions';

/**
 * What is wrong with one candidate path, or `null` when nothing is.
 *
 * A candidate is a path on the account's own base URL, never a URL: the credential for that account
 * may not be sent anywhere else, so a host in the list is refused rather than fetched. This mirrors
 * `probe_path_problem` in `crates/mr-core/src/quota/probe.rs` so a typo is caught while it is typed
 * instead of at the first refresh. The router refuses the same entries at runtime — it has to, since
 * an account's own list never passes through config validation.
 */
export function probePathProblem(path) {
  const value = typeof path === 'string' ? path : '';
  if (!value.trim()) return 'is empty';
  if (value.includes('://') || value.startsWith('//')) {
    return 'names a host; a candidate is a path on the provider\'s own base URL, so it must start with a single "/"';
  }
  if (!value.startsWith('/')) return 'must start with "/" — a candidate is a path, not a URL';
  if (value.includes('#')) return 'carries a fragment, which is never sent to a server';
  // Exactly what the router refuses: a space, or a control character (`\u0000`–`\u001f`, `\u007f`).
  if (/[\u0000-\u001f\u007f ]/.test(value)) {
    return 'contains a space or control character';
  }
  return null;
}

/** The first problem in a candidate list, as `index` and reason; `null` when the list is usable. */
export function probePathListProblem(paths) {
  const list = Array.isArray(paths) ? paths : [];
  for (let index = 0; index < list.length; index += 1) {
    const problem = probePathProblem(list[index]);
    if (problem) return { index, problem };
  }
  return null;
}

/**
 * Where a reading came from, for the Quota page.
 *
 * `source` is the fetcher that produced the snapshot, and the walk appends the candidate that
 * answered: `openai-compatible /v1/balance`, or `... /v1/balance (headers)` when the number came
 * from response headers rather than the body. Anything else (a dedicated fetcher, or a reading
 * taken before the walk existed) has no candidate to name and returns an empty string, so the page
 * says nothing rather than guessing.
 */
export function probeSourceLabel(source) {
  if (typeof source !== 'string') return '';
  const match = new RegExp(`^${PROBE_SOURCE_PREFIX}\\s+(\\S+?)(\\s+\\(headers\\))?$`).exec(source.trim());
  if (!match) return '';
  const path = match[1];
  if (!path.startsWith('/')) return '';
  return match[2] ? `read from ${path} (response headers)` : `read from ${path}`;
}
