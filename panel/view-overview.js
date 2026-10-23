// view-overview.js — health summary, counts, snapshot, relay reachability.
//
// Live updates arrive through ctx.onHealth (the 5 s poll in app.js). The relay probe is
// deliberately NOT on the poll loop: probing every tick would send requests to a relay nobody
// is looking at — the exact waste the poll scheduler exists to avoid. It runs once on open and
// on demand via the "Probe again" button.

import { h, chip, spinner } from './dom.js';
import { formatUptime, formatAge, truncateMiddle, relayProbeUrl } from './format.js';

export async function renderOverview(mount, ctx) {
  const alive = { on: true };
  const root = h('div');
  mount.replaceChildren(root);

  const statGrid = h('div', { class: 'stat-grid' });
  const relayBox = h('div');
  root.append(statGrid, relayBox);

  function stat(k, valueEl, sub = null) {
    const v = h('div', { class: 'v' }, valueEl);
    const box = h('div', { class: 'stat' }, h('div', { class: 'k' }, k), v);
    if (sub) box.append(h('div', { class: 's' }, sub));
    return box;
  }

  function renderStats(health) {
    statGrid.replaceChildren();
    if (!health) {
      statGrid.append(h('div', { class: 'stat' },
        h('div', { class: 'k' }, 'control plane'),
        h('div', { class: 'v' }, 'unreachable'),
        h('div', { class: 's' }, 'no data is being invented — check the process log')));
      return;
    }
    const counts = health.counts ?? {};
    const digest = health.snapshot?.digest;
    statGrid.append(
      stat('version', h('span', { class: 'mono' }, String(health.version ?? '?'))),
      stat('uptime', h('span', { 'data-live': 'uptime' }, formatUptime(health.uptime_secs))),
      stat('snapshot', h('span', {
        class: 'mono',
        title: digest ? `digest ${digest}, built ${health.snapshot?.built_at ?? 'unknown'}` : 'no snapshot',
      }, digest ? truncateMiddle(digest) : 'none'),
        `built ${formatAge(health.snapshot?.built_at)}`),
      stat('providers', String(counts.providers ?? 0)),
      stat('combos', String(counts.combos ?? 0)),
      stat('aliases', String(counts.aliases ?? 0)),
      stat('client keys', String(counts.keys ?? 0)),
      stat('panel password',
        health.password_set ? chip('set', 'chip-green') : chip('not set', 'chip-amber'),
        health.password_set ? 'sessions require the password' : 'control plane has no password yet'),
    );
  }

  // --- relay reachability -------------------------------------------------

  async function probeRelay() {
    const url = relayProbeUrl(ctx.state.ports?.relay);
    relayBox.replaceChildren(h('div', { class: 'relay-up' }, h('h2', {}, 'Relay'), spinner(), ' probing…'));
    if (!url) {
      // Honest unknown: without a port from /api/ports there is nothing to dial.
      relayBox.replaceChildren(h('div', { class: 'relay-up' },
        h('h2', {}, 'Relay status unknown'),
        h('p', { class: 'hint' }, 'No relay listen address is available (GET /api/ports returned nothing usable), so the panel cannot probe it.')));
      return;
    }
    const controller = new AbortController();
    const bail = setTimeout(() => controller.abort(), 3000);
    let up = false;
    try {
      // mode:'no-cors' gives an opaque response: we cannot read status codes cross-origin, but a
      // resolve proves the socket answered. Reachability is all the panel claims here.
      await fetch(url, { mode: 'no-cors', cache: 'no-store', signal: controller.signal });
      up = true;
    } catch {
      up = false;
    } finally {
      clearTimeout(bail);
    }
    if (!alive.on) return; // view changed while probing; drop the result

    if (up) {
      relayBox.replaceChildren(h('div', { class: 'relay-up' },
        h('h2', {}, 'Relay reachable'),
        h('p', { class: 'hint' }, 'The data plane answered at ', h('span', { class: 'mono' }, url), '.'),
        probeButton()));
    } else {
      // The one big red surface in the panel: requests routed at the relay will fail right now.
      relayBox.replaceChildren(h('div', { class: 'relay-down' },
        h('h2', {}, 'Relay not reachable'),
        h('p', {}, 'Nothing answered at ', h('span', { class: 'mono' }, url), '. Requests forwarded to the relay will fail until it is running again.'),
        h('p', { class: 'hint' }, 'The control plane is fine — this is about the relay process or its listen port.'),
        probeButton()));
    }
  }

  function probeButton() {
    const b = h('button', { class: 'btn btn-sm', type: 'button' }, 'Probe again');
    b.addEventListener('click', () => probeRelay());
    return b;
  }

  // --- live health subscription --------------------------------------------

  ctx.onHealth((health) => {
    if (!alive.on) return;
    renderStats(health);
  });

  await Promise.all([ctx.loadProviders().catch(() => null), ctx.refreshPorts()]);
  renderStats(ctx.state.health);
  await probeRelay();
}
