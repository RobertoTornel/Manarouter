// view-overview.js — health summary, system inventory, relay reachability,
// routing presets, and live request dispatch telemetry.

import { api } from './api.js';
import { h, chip, spinner } from './dom.js';
import { formatUptime, formatAge, truncateMiddle, relayProbeUrl, extractList, formatCompact } from './format.js';
import { comboRoutePreview, recentCombos } from './combo-live.js';

export async function renderOverview(mount, ctx) {
  const alive = { on: true };
  const root = h('div', { class: 'overview-root' });
  mount.replaceChildren(root);

  // 1. System Inventory Telemetry (6 columns full width)
  const statGrid = h('div', { class: 'stat-grid stat-grid-resources' });

  // 2. Core Infrastructure Telemetry (Relay Data Plane & Conductor Control Plane)
  const coreGrid = h('div', { class: 'overview-core-grid' });
  const relayCard = h('section', { class: 'card overview-infra-card', 'aria-label': 'Data Plane Relay' });
  const conductorCard = h('section', { class: 'card overview-infra-card', 'aria-label': 'Control Plane Conductor' });
  coreGrid.append(relayCard, conductorCard);

  // 3. Routing Presets / Combos Grid
  const combosCard = h('section', { class: 'card overview-combos-card', 'aria-label': 'Routing Presets' });
  const combosHead = h('div', { class: 'overview-section-head' },
    h('div', { class: 'overview-section-title' },
      h('h2', {}, 'Routing Presets'),
      h('span', { class: 'count', 'data-combos-count': '' }, 'loading…')),
    h('span', { class: 'hint' }, 'Compiled failover and load balancing routes'));
  const combosGrid = h('div', { class: 'overview-combos-grid' });
  combosCard.append(combosHead, combosGrid);

  // 4. Live Request Dispatch Activity Stream
  const activityCard = h('section', { class: 'card overview-activity-card', 'aria-label': 'Recent Request Dispatches' });
  const activityHead = h('div', { class: 'overview-section-head' },
    h('div', { class: 'overview-section-title' },
      h('h2', {}, 'Recent Request Activity'),
      h('span', { class: 'count', 'data-activity-count': '' }, 'loading…')),
    h('span', { class: 'hint' }, 'Live in-memory ring buffer of gateway requests'));
  const activityTableWrap = h('div', { class: 'table-scroll overview-activity-scroll' });
  activityCard.append(activityHead, activityTableWrap);

  root.append(statGrid, coreGrid, combosCard, activityCard);

  let cachedProviders = [];
  let cachedCombos = [];
  let cachedActivity = [];
  let cachedKeys = [];
  let cachedAliases = [];
  let relayState = { status: 'probing', url: '', ms: null, err: null };

  function statBox(label, value, sub, route) {
    const box = h('button', {
      class: 'stat overview-stat-btn',
      type: 'button',
      title: `View ${label} (${value})`,
      onclick: () => ctx.navigate(route),
    },
      h('div', { class: 'k' }, label),
      h('div', { class: 'v mono' }, value),
      h('div', { class: 's' }, sub));
    return box;
  }

  function renderStats(health) {
    statGrid.replaceChildren();
    if (!health) {
      statGrid.append(h('div', { class: 'stat' },
        h('div', { class: 'k' }, 'CONTROL PLANE'),
        h('div', { class: 'v' }, 'UNREACHABLE'),
        h('div', { class: 's' }, 'Process socket not responding')));
      return;
    }

    const counts = health.counts ?? {};
    const provList = cachedProviders;
    const totalAccounts = counts.accounts
      ?? provList.reduce((acc, p) => acc + (p.accounts?.length ?? 0), 0);
    const totalModels = provList.reduce((acc, p) => {
      if (Array.isArray(p.models)) return acc + p.models.length;
      if (typeof p.model_count === 'number') return acc + p.model_count;
      return acc;
    }, 0);

    const modelDisplay = totalModels > 0 ? totalModels.toLocaleString() : '—';
    const comboDisplay = String(counts.combos ?? cachedCombos.length ?? 0);
    const provDisplay = String(counts.providers ?? provList.length ?? 0);
    const accDisplay = String(totalAccounts);
    const keyDisplay = String(counts.keys ?? cachedKeys.length ?? 0);
    const aliasDisplay = String(counts.aliases ?? cachedAliases.length ?? 0);

    statGrid.append(
      statBox('PROVIDERS', provDisplay, 'configured upstreams', '#/providers'),
      statBox('ACCOUNTS', accDisplay, 'credentials in vault', '#/providers'),
      statBox('MODELS', modelDisplay, 'catalogue models', '#/models'),
      statBox('COMBOS', comboDisplay, 'routing presets', '#/combos'),
      statBox('CLIENT KEYS', keyDisplay, 'caller credentials', '#/keys'),
      statBox('ALIASES', aliasDisplay, 'model rewrites', '#/aliases'),
    );
  }

  function renderConductor(health) {
    conductorCard.replaceChildren();
    const isHealthy = Boolean(health);
    const headRow = h('div', { class: 'infra-head' },
      h('div', { class: 'infra-title-wrap' },
        h('span', { class: 'dot ' + (isHealthy ? 'dot-green' : 'dot-red'), 'aria-hidden': 'true' }),
        h('h2', {}, 'Conductor // Control Plane')),
      isHealthy
        ? chip('HEALTHY // OPERATIONAL', 'chip-green')
        : chip('DOWN // UNREACHABLE', 'chip-red'));

    const ctrlPort = ctx.state.ports?.control?.port ?? 20901;
    const ctrlHost = ctx.state.ports?.control?.host ?? '0.0.0.0';
    const digest = health?.snapshot?.digest;
    const snapshotText = digest ? truncateMiddle(digest, 10, 8) : 'none';
    const snapshotAge = digest ? formatAge(health.snapshot?.built_at) : null;
    const uptimeText = health ? formatUptime(health.uptime_secs) : '—';
    const versionText = String(health?.version ?? '?');

    const kv = h('dl', { class: 'kv infra-kv' },
      h('div', { class: 'kv-row' },
        h('dt', {}, 'SOCKET'),
        h('dd', { class: 'mono' }, `${ctrlHost}:${ctrlPort}`)),
      h('div', { class: 'kv-row' },
        h('dt', {}, 'VERSION'),
        h('dd', { class: 'mono' }, `v${versionText}`)),
      h('div', { class: 'kv-row' },
        h('dt', {}, 'UPTIME'),
        h('dd', { class: 'mono', 'data-live': 'uptime' }, uptimeText)),
      h('div', { class: 'kv-row' },
        h('dt', {}, 'SNAPSHOT'),
        h('dd', { class: 'mono', title: digest ? `digest ${digest}` : '' },
          snapshotText,
          snapshotAge ? h('span', { class: 'kv-note' }, `built ${snapshotAge}`) : null)),
      h('div', { class: 'kv-row' },
        h('dt', {}, 'CONSOLE AUTH'),
        h('dd', {},
          health?.password_set
            ? chip('PASSWORD PROTECTED', 'chip-green')
            : chip('OPEN ACCESS // NO PASSWORD', 'chip-amber'))),
    );

    conductorCard.append(headRow, kv);
  }

  function renderRelay() {
    relayCard.replaceChildren();
    const isUp = relayState.status === 'up';
    const isProbing = relayState.status === 'probing';
    const relayPort = ctx.state.ports?.relay?.port ?? 20900;
    const relayHost = ctx.state.ports?.relay?.host ?? '0.0.0.0';

    const headRow = h('div', { class: 'infra-head' },
      h('div', { class: 'infra-title-wrap' },
        h('span', { class: 'dot ' + (isUp ? 'dot-green' : isProbing ? 'dot-idle' : 'dot-red'), 'aria-hidden': 'true' }),
        h('h2', {}, 'Relay // Data Plane')),
      isUp
        ? chip('ONLINE // REACHABLE', 'chip-green')
        : isProbing
          ? chip('PROBING SOCKET…', 'chip-neutral')
          : chip('OFFLINE // UNREACHABLE', 'chip-red'));

    const probeBtn = h('button', {
      class: 'btn btn-sm',
      type: 'button',
      disabled: isProbing,
      onclick: () => void probeRelay(),
    }, isProbing ? spinner() : null, isProbing ? ' Probing…' : 'Probe Socket');

    const statusNote = isUp
      ? h('span', { class: 'kv-ok mono' }, `Socket answered in ${relayState.ms ?? '<5'} ms`)
      : isProbing
        ? h('span', { class: 'kv-note mono' }, 'Testing socket…')
        : h('span', { class: 'kv-err mono' }, 'No response on loopback');

    const kv = h('dl', { class: 'kv infra-kv' },
      h('div', { class: 'kv-row' },
        h('dt', {}, 'LISTEN SOCKET'),
        h('dd', { class: 'mono' }, `${relayHost}:${relayPort}`)),
      h('div', { class: 'kv-row' },
        h('dt', {}, 'INTERFACES'),
        h('dd', { class: 'mono' }, 'OpenAI (/v1) + Anthropic (/v1/messages)')),
      h('div', { class: 'kv-row' },
        h('dt', {}, 'PROBE STATUS'),
        h('dd', {}, statusNote, h('span', { class: 'kv-actions' }, probeBtn))),
      h('div', { class: 'kv-row' },
        h('dt', {}, 'PROBE TARGET'),
        h('dd', { class: 'mono kv-url' }, relayState.url || `http://127.0.0.1:${relayPort}/healthz`)),
      h('div', { class: 'kv-row' },
        h('dt', {}, 'FAILOVER POLICY'),
        h('dd', {}, chip('AUTOMATIC RESET-AWARE', 'chip-neutral'))),
    );

    relayCard.append(headRow, kv);
  }

  function renderCombos() {
    combosGrid.replaceChildren();
    const combos = cachedCombos;
    const activityPayload = { entries: cachedActivity };
    const recents = recentCombos(activityPayload, { now: Date.now() });
    const recentsMap = new Map(recents.map((r) => [r.combo, r]));

    const countSpan = combosHead.querySelector('[data-combos-count]');
    const activeCount = recents.length;
    if (countSpan) {
      countSpan.textContent = `${combos.length} presets // ${activeCount} active in 5m`;
    }

    if (!combos.length) {
      combosGrid.append(h('div', { class: 'overview-empty' }, 'No routing combos configured.'));
      return;
    }

    for (const c of combos) {
      const recent = recentsMap.get(c.name);
      const preview = comboRoutePreview(c, recent);
      const isLive = preview.live;
      const isActive = Boolean(recent);
      const stepCount = c.steps?.length ?? 0;
      const strategyName = c.strategy === 'reset-aware-priority' ? 'RESET-AWARE' : (c.strategy ?? 'priority').toUpperCase();

      const tile = h('button', {
        class: 'overview-combo-tile' + (isActive ? ' is-active' : ''),
        type: 'button',
        title: `Open combo ${c.name}`,
        onclick: () => ctx.navigate(`#/combos/${encodeURIComponent(c.name)}`),
      },
        h('div', { class: 'combo-tile-top' },
          h('div', { class: 'combo-tile-name-wrap' },
            h('span', { class: 'dot ' + (isLive ? 'dot-green' : isActive ? 'dot-idle' : 'dot-neutral'), 'aria-hidden': 'true' }),
            h('span', { class: 'combo-tile-name' }, c.name)),
          chip(strategyName, 'chip-neutral')),
        h('div', { class: 'combo-tile-route mono', title: preview.title }, preview.route),
        h('div', { class: 'combo-tile-foot' },
          h('span', { class: 'combo-tile-steps' }, `${stepCount} step${stepCount === 1 ? '' : 's'}`),
          isLive
            ? chip('LIVE NOW', 'chip-pink')
            : isActive
              ? chip(`${recent.count} CALLS // ${formatAge(recent.lastStartedAt)}`, 'chip-green')
              : chip('STANDBY // IDLE', 'chip-neutral')));

      combosGrid.append(tile);
    }
  }

  function renderActivity() {
    activityTableWrap.replaceChildren();
    const entries = cachedActivity;
    const countSpan = activityHead.querySelector('[data-activity-count]');
    if (countSpan) {
      countSpan.textContent = `${entries.length} dispatches in memory ring`;
    }

    if (!entries.length) {
      activityTableWrap.append(h('div', { class: 'overview-empty' },
        'No recent request activity recorded in relay ring buffer.'));
      return;
    }

    const tbl = h('table', { class: 'tbl overview-activity-tbl' },
      h('thead', {},
        h('tr', {},
          h('th', {}, 'TIMESTAMP'),
          h('th', {}, 'COMBO'),
          h('th', {}, 'REQUESTED MODEL'),
          h('th', {}, 'DISPATCHED STEP'),
          h('th', { class: 'num' }, 'LATENCY'),
          h('th', { class: 'num' }, 'STATUS'))),
      h('tbody', {}));

    const tbody = tbl.querySelector('tbody');
    const now = Date.now();

    for (const item of entries.slice(0, 20)) {
      const isRunning = item.finished_at == null && item.outcome == null;
      let durationStr = '—';
      if (item.started_at && item.finished_at) {
        const ms = item.finished_at - item.started_at;
        durationStr = ms < 1000 ? `${ms} ms` : `${(ms / 1000).toFixed(2)} s`;
      } else if (isRunning) {
        durationStr = 'running…';
      }

      const outcomeChip = isRunning
        ? chip('IN FLIGHT', 'chip-pink')
        : item.outcome === 'ok'
          ? chip('200 OK', 'chip-green')
          : chip(String(item.outcome ?? 'FAILED').toUpperCase(), 'chip-red');

      const tr = h('tr', {},
        h('td', { class: 'mono cell-dim' }, formatAge(item.started_at, now)),
        h('td', {},
          h('a', {
            href: `#/combos/${encodeURIComponent(item.combo)}`,
            class: 'overview-table-combo mono',
            title: `View combo ${item.combo}`,
          }, item.combo)),
        h('td', { class: 'mono' }, item.requested || '—'),
        h('td', { class: 'mono cell-highlight', title: item.step }, item.step || '—'),
        h('td', { class: 'num mono' }, durationStr),
        h('td', { class: 'num' }, outcomeChip));

      tbody.append(tr);
    }

    activityTableWrap.append(tbl);
  }

  async function probeRelay() {
    const url = relayProbeUrl(ctx.state.ports?.relay, globalThis.location?.hostname, globalThis.location?.protocol);
    relayState = { status: 'probing', url: url || '', ms: null, err: null };
    renderRelay();

    if (!url) {
      relayState = { status: 'down', url: '', ms: null, err: 'No listen address' };
      if (alive.on) renderRelay();
      return;
    }

    const start = performance.now();
    const controller = new AbortController();
    const bail = setTimeout(() => controller.abort(), 3000);
    let ok = false;
    try {
      await fetch(url, { mode: 'no-cors', cache: 'no-store', signal: controller.signal });
      ok = true;
    } catch (err) {
      ok = false;
      relayState.err = String(err?.message ?? 'Fetch failed');
    } finally {
      clearTimeout(bail);
    }

    if (!alive.on) return;
    const elapsed = Math.round(performance.now() - start);
    relayState = { status: ok ? 'up' : 'down', url, ms: elapsed, err: ok ? null : relayState.err };
    renderRelay();
  }

  async function refreshData() {
    if (!alive.on || !mount.isConnected) return;
    try {
      const [provs, combs, act, keys, aliases] = await Promise.all([
        ctx.loadProviders().catch(() => cachedProviders),
        api.get('/api/combos').then((d) => extractList(d, 'combos')).catch(() => cachedCombos),
        api.get('/api/activity').then((d) => extractList(d, 'entries')).catch(() => cachedActivity),
        api.get('/api/keys').then((d) => extractList(d, 'keys', 'client_keys')).catch(() => cachedKeys),
        api.get('/api/aliases').then((d) => extractList(d, 'aliases')).catch(() => cachedAliases),
      ]);

      if (!alive.on) return;
      cachedProviders = Array.isArray(provs) ? provs : cachedProviders;
      cachedCombos = Array.isArray(combs) ? combs : cachedCombos;
      cachedActivity = Array.isArray(act) ? act : cachedActivity;
      cachedKeys = Array.isArray(keys) ? keys : cachedKeys;
      cachedAliases = Array.isArray(aliases) ? aliases : cachedAliases;

      renderStats(ctx.state.health);
      renderConductor(ctx.state.health);
      renderCombos();
      renderActivity();
    } catch {
      // Background poll failure is quiet
    }
  }

  ctx.onHealth((health) => {
    if (!alive.on) return;
    renderStats(health);
    renderConductor(health);
    void refreshData();
  });

  // Initial load
  await Promise.all([ctx.loadProviders().catch(() => []), ctx.refreshPorts()]);
  await refreshData();
  void probeRelay();
}
