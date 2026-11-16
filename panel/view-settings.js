// view-settings.js — relay ports, pool, authentication, proxy and quota display settings,
// panel password, and the raw configuration editor with validate + save.

import { api } from './api.js';
import { h, chip, field, toggle, errorBlock, applyFieldErrors, clearFieldErrors } from './dom.js';
import {
  describePortChange, moveStep, probePathListProblem, probePathProblem,
  QUOTA_PROBE_CHAT_PATH, QUOTA_PROBE_DEFAULT_PATHS,
} from './format.js';

export async function renderSettings(mount, ctx) {
  const portsCard = h('div', { class: 'card wide' }, h('h2', {}, 'Listen ports'));
  const latencyCard = h('div', { class: 'card' }, h('h2', {}, 'Relay connection pool'));
  const accessCard = h('div', { class: 'card' }, h('h2', {}, 'Client-key access'));
  const proxiesCard = h('div', { class: 'card wide' }, h('h2', {}, 'Upstream proxies'));
  const peopleCard = h('div', { class: 'card wide' }, h('h2', {}, 'People and their machines'));
  const quotaDisplayCard = h('div', { class: 'card' }, h('h2', {}, 'Quota display'));
  const quotaProbeCard = h('div', { class: 'card wide' }, h('h2', {}, 'Quota probing'));
  const pricingCard = h('div', { class: 'card' }, h('h2', {}, 'Pricing data'));
  const passwordCard = h('div', { class: 'card' }, h('h2', {}, 'Panel password'));
  const rawCard = h('div', { class: 'card wide' }, h('h2', {}, 'Raw configuration'));
  // Eight full-width cards made Settings a long single column with no reading rhythm. A two-up
  // grid keeps the wide forms (ports, proxies, people, raw JSON) full width and pairs the short
  // ones. Quota probing joins them wide: a candidate list is a row of paths, each with its own
  // label and reorder buttons, and pairing it with a narrow card would squeeze the paths into a
  // column.
  mount.replaceChildren(h('div', { class: 'settings-grid' },
    portsCard, latencyCard, accessCard, proxiesCard, peopleCard, quotaDisplayCard, quotaProbeCard,
    pricingCard, passwordCard, rawCard));

  await Promise.all([renderPorts(), renderPassword(), renderLatency(), renderAccess(), renderProxies(), renderPeople(), renderQuotaDisplay(), renderQuotaProbe(), renderPricing()]);
  renderRaw(); // sequential on purpose: the textarea is heavy and it can stream in last

  // ------------------------------------------------------------------ ports

  async function renderPorts() {
    portsCard.append('loading…');
    let ports;
    try {
      ports = await api.get('/api/ports');
      ctx.state.ports = ports; // keep the global banner in sync with what this view shows
    } catch (err) {
      portsCard.replaceChildren(h('h2', {}, 'Listen ports'), errorBlock(err));
      return;
    }

    const relay = ports.relay ?? {};
    const control = ports.control ?? {};

    const relayHost = h('input', { class: 'input mono', type: 'text', value: relay.host ?? '', 'data-field': 'relay_host', 'aria-label': 'Relay host' });
    const relayPort = h('input', { class: 'input mono', type: 'number', value: relay.port ?? '', 'data-field': 'relay_port', 'aria-label': 'Relay port' });
    const ctrlHost = h('input', { class: 'input mono', type: 'text', value: control.host ?? '', 'data-field': 'control_host', 'aria-label': 'Control host' });
    const ctrlPort = h('input', { class: 'input mono', type: 'number', value: control.port ?? '', 'data-field': 'control_port', 'aria-label': 'Control port' });

    const preview = h('div');
    const saveBtn = h('button', { class: 'btn btn-primary', type: 'button' }, 'Save ports');
    const restartBtn = h('button', { class: 'btn', type: 'button' }, 'Restart now');
    const pendingChip = chip('pending restart', 'chip-amber', 'Saved changes are not live yet');

    function dirty() {
      return (relayHost.value !== (relay.host ?? ''))
        || (Number(relayPort.value) !== Number(relay.port ?? NaN))
        || (ctrlHost.value !== (control.host ?? ''))
        || (Number(ctrlPort.value) !== Number(control.port ?? NaN));
    }

    function renderPreview() {
      const isDirty = dirty();
      const changes = isDirty
        ? describePortChange({
          relay: { host: relayHost.value, port: Number(relayPort.value), running_port: relay.running_port },
          control: { host: ctrlHost.value, port: Number(ctrlPort.value), running_port: control.running_port },
        })
        : describePortChange(ports);
      preview.replaceChildren();
      if (changes.length) {
        preview.append(h('ul', { class: 'hint settings-list' },
          ...changes.map((c) => h('li', {}, c))));
      } else if (ports.restart_pending) {
        preview.append(h('p', { class: 'hint' }, 'No unsaved edits; a previously saved change is still waiting for the restart.'));
      }
      const showPending = isDirty || ports.restart_pending;
      pendingChip.style.display = showPending ? '' : 'none';
      saveBtn.disabled = !isDirty;
    }

    for (const el of [relayHost, relayPort, ctrlHost, ctrlPort]) {
      el.addEventListener('input', renderPreview);
    }

    saveBtn.addEventListener('click', async () => {
      saveBtn.disabled = true;
      try {
        await api.put('/api/ports', {
          relay: { host: relayHost.value.trim(), port: Number(relayPort.value) },
          control: { host: ctrlHost.value.trim(), port: Number(ctrlPort.value) },
        });
        // PUT /api/ports replies applies_on: "next_restart" — the amber banner (and this chip)
        // is the warning that the change is saved but NOT live. Amber, not red: nothing is
        // wrong; the operator just has not restarted yet.
        ctx.toast.warning('Port change saved. It applies on the next restart — the relay is still listening on the old port.', null);
        await ctx.refreshPorts();
        await renderPorts();
      } catch (err) {
        applyFieldErrors(portsCard, err);
        if (!err.details?.length) ctx.toast.error('Could not save the ports', err);
        saveBtn.disabled = false;
      }
    });

    restartBtn.addEventListener('click', () => ctx.restartFlow());

    portsCard.replaceChildren(
      h('h2', {}, 'Listen ports'),
      h('p', { class: 'hint settings-lede' },
        'Changing a port does not move the listener immediately — edits apply on the next restart.'),
      h('div', { class: 'grid-2' },
        h('fieldset', { class: 'plain' },
          h('legend', {}, 'Relay (data plane)'),
          field('Host', relayHost),
          field('Port', relayPort, { hint: `currently running on ${relay.running_port ?? '—'}` })),
        h('fieldset', { class: 'plain' },
          h('legend', {}, 'Control plane (panel + API)'),
          field('Host', ctrlHost),
          field('Port', ctrlPort, { hint: `currently running on ${control.running_port ?? '—'} — this panel moves with it` }))),
      pendingChip,
      preview,
      h('div', { class: 'dialog-actions' }, restartBtn, saveBtn),
    );
    renderPreview();
  }

  // ------------------------------------------------------------------ password

  async function renderPassword() {
    let passwordSet = true;
    try {
      const health = ctx.state.health ?? await api.get('/api/health');
      passwordSet = Boolean(health.password_set);
    } catch { /* assume set: asking for the current password is the safe default */ }

    const current = h('input', { class: 'input', type: 'password', autocomplete: 'current-password', 'data-field': 'current' });
    const next = h('input', { class: 'input', type: 'password', autocomplete: 'new-password', 'data-field': 'new' });
    const confirmIn = h('input', { class: 'input', type: 'password', autocomplete: 'new-password', 'data-field': 'confirm' });
    const save = h('button', { class: 'btn btn-primary', type: 'submit' }, passwordSet ? 'Change password' : 'Set password');

    const form = h('form', { novalidate: true },
      h('p', { class: 'hint settings-lede' },
        passwordSet
          ? 'The password is stored as an Argon2id hash; the panel never sees the plaintext again.'
          : 'No password is set. The control plane is reachable without one from allowed hosts — setting one is recommended.'),
      passwordSet ? field('Current password', current) : null,
      h('div', { class: 'grid-fields' },
        field('New password', next),
        field('Repeat new password', confirmIn)),
      h('div', { class: 'dialog-actions' }, save));

    form.addEventListener('submit', async (e) => {
      e.preventDefault();
      clearFieldErrors(form);
      if (next.value !== confirmIn.value) {
        applyFieldErrors(form, { message: 'The two new passwords do not match.', details: [] });
        return;
      }
      save.disabled = true;
      try {
        const body = passwordSet ? { current: current.value, new: next.value } : { new: next.value };
        await api.post('/api/password', body);
        ctx.toast.success('Password updated.');
        current.value = next.value = confirmIn.value = '';
        ctx.state.session && (ctx.state.session.password_required = true);
        if (!passwordSet) {
          // A password exists now, so the form must switch to change mode: the next submit has
          // to send the current password, which this form does not ask for yet.
          if (ctx.state.health) ctx.state.health.password_set = true;
          await renderPassword();
          return;
        }
      } catch (err) {
        applyFieldErrors(form, err);
        if (!err.details?.length) ctx.toast.error('Could not change the password', err);
      }
      save.disabled = false;
    });

    passwordCard.replaceChildren(h('h2', {}, 'Panel password'), form);
  }

  // --------------------------------------------------------- global connection pool

  async function renderLatency() {
    latencyCard.append('loading…');
    let raw;
    try {
      raw = await api.getRaw('/api/config/raw');
    } catch (err) {
      latencyCard.replaceChildren(h('h2', {}, 'Relay connection pool'), errorBlock(err));
      return;
    }

    let config;
    try {
      config = JSON.parse(raw);
      if (!config || typeof config !== 'object' || Array.isArray(config)) throw new Error('Configuration must be an object.');
    } catch (err) {
      latencyCard.replaceChildren(
        h('h2', {}, 'Relay connection pool'),
        h('p', { class: 'hint' }, 'Could not safely read the current configuration. Use the raw configuration editor to inspect it.'),
        errorBlock(err),
      );
      return;
    }

    const latency = config.latency && typeof config.latency === 'object' && !Array.isArray(config.latency)
      ? config.latency : {};
    const defaults = { pool_idle_secs: 90, pool_max_idle_per_host: 4 };
    const effective = {
      pool_idle_secs: Number.isInteger(latency.pool_idle_secs) && latency.pool_idle_secs >= 0
        ? latency.pool_idle_secs : defaults.pool_idle_secs,
      pool_max_idle_per_host: Number.isInteger(latency.pool_max_idle_per_host) && latency.pool_max_idle_per_host >= 1
        ? latency.pool_max_idle_per_host : defaults.pool_max_idle_per_host,
    };
    const lowResource = effective.pool_idle_secs === 30 && effective.pool_max_idle_per_host === 2;
    const profileToggle = toggle('Low-resource profile (30s idle, 2 connections per host)', {
      checked: lowResource,
      help: 'Use shorter idle connection lifetime and a smaller per-host connection pool to retain fewer sockets.',
    });
    const profile = profileToggle.input;
    const advanced = h('details', { class: 'settings-advanced' });
    const idleSecs = h('input', { class: 'input mono', type: 'number', min: '0', max: '86400', step: '1', value: String(effective.pool_idle_secs), 'aria-label': 'Idle connection lifetime in seconds' });
    const maxIdle = h('input', { class: 'input mono', type: 'number', min: '1', step: '1', value: String(effective.pool_max_idle_per_host), 'aria-label': 'Maximum idle connections per host' });
    const status = h('div');
    const save = h('button', { class: 'btn btn-primary', type: 'button' }, 'Save pool settings');
    const restart = h('p', { class: 'hint' }, 'Relay restart required: saving updates conductor configuration, but the running relay does not hot-reload these pool settings.');

    function updateProfileFromValues() {
      profile.checked = Number(idleSecs.value) === 30 && Number(maxIdle.value) === 2;
    }
    profile.addEventListener('change', () => {
      if (profile.checked) {
        idleSecs.value = '30';
        maxIdle.value = '2';
      } else {
        idleSecs.value = String(defaults.pool_idle_secs);
        maxIdle.value = String(defaults.pool_max_idle_per_host);
      }
    });
    idleSecs.addEventListener('input', updateProfileFromValues);
    maxIdle.addEventListener('input', updateProfileFromValues);
    advanced.open = !lowResource;

    save.addEventListener('click', async () => {
      clearFieldErrors(latencyCard);
      status.replaceChildren();
      const idleValue = Number(idleSecs.value);
      const maxValue = Number(maxIdle.value);
      if (!Number.isInteger(idleValue) || idleValue < 0 || idleValue > 86400) {
        applyFieldErrors(latencyCard, { message: 'Idle connection lifetime must be an integer from 0 to 86400 seconds.', details: [] });
        return;
      }
      if (!Number.isInteger(maxValue) || maxValue < 1) {
        applyFieldErrors(latencyCard, { message: 'Maximum idle connections per host must be an integer of at least 1.', details: [] });
        return;
      }
      save.disabled = true;
      try {
        // Re-read before patching so other settings changed since page load are not replaced.
        const latest = await readConfig();
        const latestLatency = latest.latency && typeof latest.latency === 'object' && !Array.isArray(latest.latency)
          ? latest.latency : {};
        latest.latency = { ...latestLatency, pool_idle_secs: idleValue, pool_max_idle_per_host: maxValue };
        await api.putRaw('/api/config/raw', JSON.stringify(latest, null, 2));
        status.replaceChildren(chip('Saved · relay restart required', 'warn'), restart);
        ctx.toast.warning('Pool settings saved. Restart the relay for them to take effect.', null);
      } catch (err) {
        applyFieldErrors(latencyCard, err);
        if (!err.details?.length) status.replaceChildren(errorBlock(err));
      }
      save.disabled = false;
    });

    advanced.append(
      h('summary', {}, 'Advanced controls'),
      field('Idle connection lifetime (seconds)', idleSecs, { hint: '0–86400; shorter lifetimes release idle sockets sooner.' }),
      field('Maximum idle connections per host', maxIdle, { hint: 'At least 1; lower values reduce retained sockets across provider hosts.' }),
    );
    latencyCard.replaceChildren(
      h('h2', {}, 'Relay connection pool'),
      h('p', { class: 'hint' }, `Effective settings: ${effective.pool_idle_secs}s idle lifetime · ${effective.pool_max_idle_per_host} idle connection(s) per host. These values are read from the effective configuration, using defaults only when fields are absent or invalid.`),
      profileToggle.row,
      advanced,
      restart,
      h('div', { class: 'settings-actions' }, save),
      status,
    );
  }

  // --------------------------------------------------------------- client-key access

  async function renderAccess() {
    let config;
    try {
      config = await readConfig();
    } catch (err) {
      accessCard.replaceChildren(h('h2', {}, 'Client-key access'), errorBlock(err));
      return;
    }

    const noKey = toggle('Allow requests without a client key', {
      checked: config.no_key_required === true,
      help: 'When enabled, callers may use this relay without a client key. This can spend your upstream provider quota.',
    });
    const allowRemote = toggle('Allow keyless access on a remote relay bind', {
      checked: config.allow_remote_without_key === true,
      help: 'Explicitly permits an unauthenticated relay on a non-loopback address. Anyone who can reach it may use your provider accounts and incur charges.',
    });
    const warning = h('p', { class: 'hint' }, 'Remote exposure warning: do not enable remote keyless access on a network you do not fully trust. A remote open relay can let other people consume your provider quota or spend your money. Keep both switches off unless this exposure is deliberate.');
    const status = h('div');
    const save = h('button', { class: 'btn btn-primary', type: 'button' }, 'Save access settings');
    save.addEventListener('click', async () => {
      const latest = await readConfig().catch((err) => {
        status.replaceChildren(errorBlock(err));
        return null;
      });
      if (!latest) return;
      const previous = { noKey: latest.no_key_required, remote: latest.allow_remote_without_key };
      latest.no_key_required = noKey.input.checked;
      latest.allow_remote_without_key = allowRemote.input.checked;
      try {
        await api.putRaw('/api/config/raw', JSON.stringify(latest, null, 2));
        await renderValidation(status);
        ctx.toast.success('Client-key access settings saved.');
      } catch (err) {
        noKey.input.checked = previous.noKey === true;
        allowRemote.input.checked = previous.remote === true;
        status.replaceChildren(errorBlock(err));
      }
    });
    accessCard.replaceChildren(
      h('h2', {}, 'Client-key access'),
      h('p', { class: 'hint' }, 'Control whether the relay accepts requests with no client key. These are relay authentication settings, not upstream provider credentials.'),
      noKey.row, allowRemote.row, warning,
      h('div', { class: 'dialog-actions' }, save), status,
    );
  }

  // ------------------------------------------------------------------- proxy pool

  async function renderProxies() {
    let config;
    try {
      config = await readConfig();
    } catch (err) {
      proxiesCard.replaceChildren(h('h2', {}, 'Upstream proxies'), errorBlock(err));
      return;
    }

    const proxies = Array.isArray(config.proxies) ? config.proxies.map((proxy) => ({ ...proxy })) : [];
    const strategy = h('select', { class: 'input', 'aria-label': 'Proxy selection strategy' });
    for (const [value, label] of [['priority', 'Priority'], ['least-used', 'Least used'], ['random', 'Random'], ['round-robin', 'Round robin']]) {
      strategy.append(h('option', { value }, label));
    }
    strategy.value = config.proxy_strategy ?? 'priority';
    const list = h('div', { class: 'stack' });
    const status = h('div');
    const controlsByProxy = new Map();

    function syncProxyControls() {
      for (const [proxy, controls] of controlsByProxy) {
        proxy.id = controls.id.value.trim();
        proxy.url = controls.url.value.trim();
        proxy.enabled = controls.enabled.checked;
        proxy.priority = Number(controls.priority.value);
      }
    }

    function drawProxies() {
      controlsByProxy.clear();
      list.replaceChildren();
      if (!proxies.length) list.append(h('p', { class: 'hint' }, 'No upstream proxies configured.'));
      for (const [index, proxy] of proxies.entries()) {
        const id = h('input', { class: 'input mono', type: 'text', value: proxy.id ?? '', 'aria-label': `Proxy ${index + 1} id` });
        const url = h('input', { class: 'input mono', type: 'text', value: proxy.url ?? '', placeholder: 'http://proxy.example:8080', 'aria-label': `Proxy ${index + 1} URL` });
        const enabled = toggle(`Proxy ${index + 1} enabled`, {
          checked: proxy.enabled !== false,
          help: 'Disabled proxies stay configured but are not selected for upstream requests.',
        });
        const priority = h('input', { class: 'input mono', type: 'number', step: '1', value: String(Number.isInteger(proxy.priority) ? proxy.priority : 0), 'aria-label': `Proxy ${index + 1} priority` });
        const remove = h('button', { class: 'btn btn-sm', type: 'button' }, 'Remove proxy');
        remove.addEventListener('click', () => {
          syncProxyControls();
          proxies.splice(index, 1);
          drawProxies();
        });
        const row = h('div', { class: 'card settings-proxy' },
          h('div', { class: 'grid-2' },
            field('Proxy ID', id, { hint: 'Stable id used by provider/account proxy references.' }),
            field('Proxy URL', url, { hint: 'Use http://, https://, socks4:// or socks5://. Authentication secrets are managed separately; do not include passwords in the URL.' })),
          enabled.row,
          field('Priority', priority, { hint: 'Lower numbers are selected first with the priority strategy.' }),
          remove,
        );
        list.append(row);
        controlsByProxy.set(proxy, { id, url, enabled: enabled.input, priority });
      }
    }
    drawProxies();

    const add = h('button', { class: 'btn', type: 'button' }, 'Add proxy');
    add.addEventListener('click', () => {
      syncProxyControls();
      proxies.push({ id: '', url: '', enabled: true, priority: proxies.length });
      drawProxies();
    });
    const save = h('button', { class: 'btn btn-primary', type: 'button' }, 'Save proxies');
    save.addEventListener('click', async () => {
      const latest = await readConfig().catch((err) => {
        status.replaceChildren(errorBlock(err));
        return null;
      });
      if (!latest) return;
      syncProxyControls();
      const nextProxies = proxies.map((proxy) => ({ ...proxy }));
      const invalid = nextProxies.find((proxy) => !proxy.id || !proxy.url || !Number.isInteger(proxy.priority));
      if (invalid) {
        status.replaceChildren(h('div', { class: 'form-errors', role: 'alert' }, 'Each proxy needs a non-empty id, a URL, and an integer priority.'));
        return;
      }
      latest.proxies = nextProxies;
      latest.proxy_strategy = strategy.value;
      try {
        await api.putRaw('/api/config/raw', JSON.stringify(latest, null, 2));
        await renderValidation(status);
        ctx.toast.success('Proxy settings saved.');
      } catch (err) {
        status.replaceChildren(errorBlock(err));
      }
    });
    proxiesCard.replaceChildren(
      h('h2', {}, 'Upstream proxies'),
      h('p', { class: 'hint' }, 'These endpoints route upstream provider traffic. URLs can reveal internal hostnames; passwords must not be embedded in the URL. No proxy URLs or credentials are written to browser logs.'),
      field('Selection strategy', strategy, { help: 'Priority chooses the lowest number first; least-used, random and round-robin distribute among eligible enabled proxies.' }),
      list,
      h('div', { class: 'dialog-actions' }, add, save), status,
    );
  }

  // ------------------------------------------------------------------ people

  /// People, and the machine each one's traffic should leave from.
  ///
  /// A person is a tag plus a proxy. An account tagged `alice` egresses through alice's Tailscale
  /// exit node without a second per-account proxy reference that could drift away from the tag —
  /// the tag *is* the assignment. The entries live in the configuration file rather than behind
  /// their own endpoint, so this card reads and writes the whole document exactly as the proxy list
  /// above does: one validated commit, one digest bump, no half-saved people.
  async function renderPeople() {
    let config;
    try {
      config = await readConfig();
    } catch (err) {
      peopleCard.replaceChildren(h('h2', {}, 'People and their machines'), errorBlock(err));
      return;
    }

    const proxies = Array.isArray(config.proxies) ? config.proxies : [];
    const providerList = Array.isArray(config.providers) ? config.providers : [];
    const people = Array.isArray(config.people) ? config.people.map((person) => ({ ...person })) : [];
    const list = h('div', { class: 'stack' });
    const status = h('div');
    const controlsByPerson = new Map();

    function syncControls() {
      for (const [person, controls] of controlsByPerson) {
        person.tag = controls.tag.value.trim();
        person.label = controls.label.value.trim() || null;
        person.proxy = controls.proxy.value;
      }
    }

    // The join is the point of this card. A person with no tagged accounts routes nothing at all,
    // and nothing else in the panel would say so: the accounts themselves look perfectly ordinary.
    function accountsTagged(tag) {
      const needle = String(tag ?? '').trim().toLowerCase();
      if (!needle) return [];
      const found = [];
      for (const provider of providerList) {
        for (const account of Array.isArray(provider.accounts) ? provider.accounts : []) {
          const tags = Array.isArray(account.tags) ? account.tags : [];
          if (tags.some((t) => String(t ?? '').trim().toLowerCase() === needle)) {
            found.push(`${account.label || account.identity || account.id} (${provider.id})`);
          }
        }
      }
      return found;
    }

    function drawJoin(slot, tag) {
      const found = accountsTagged(tag);
      slot.replaceChildren(found.length
        ? h('span', {}, 'Tagged accounts: ', h('span', { class: 'mono' }, found.join(', ')))
        : h('span', { class: 'hint' }, 'No account carries this tag yet. Add it to an account under Providers.'));
    }

    function drawPeople() {
      controlsByPerson.clear();
      list.replaceChildren();
      if (!people.length) {
        list.append(h('p', { class: 'hint' },
          'No people yet. A person is a tag plus a proxy; tagging an account with that tag sends it through their machine.'));
      }
      for (const [index, person] of people.entries()) {
        const tag = h('input', {
          class: 'input mono', type: 'text', value: person.tag ?? '',
          'aria-label': `Person ${index + 1} tag`, placeholder: 'alice',
        });
        const label = h('input', {
          class: 'input', type: 'text', value: person.label ?? '',
          'aria-label': `Person ${index + 1} label`, placeholder: 'optional display name',
        });
        const proxy = h('select', { class: 'input', 'aria-label': `Person ${index + 1} proxy` },
          ...(person.proxy ? [] : [h('option', { value: '' }, 'choose a proxy…')]),
          ...proxies.map((option) => h('option', { value: option.id, selected: person.proxy === option.id },
            option.enabled === false ? `${option.id} (disabled)` : option.id)),
          // A proxy this person already names but the list no longer has must stay visible: a
          // select whose value matches no option shows the first one, and saving would then
          // silently repoint the person at a different machine.
          ...(person.proxy && !proxies.some((option) => option.id === person.proxy)
            ? [h('option', { value: person.proxy, selected: true }, `${person.proxy} (unknown)`)]
            : []));
        const join = h('p', { class: 'hint settings-person-join' });
        drawJoin(join, person.tag);
        tag.addEventListener('input', () => drawJoin(join, tag.value));

        const remove = h('button', { class: 'btn btn-sm', type: 'button' }, 'Remove person');
        remove.addEventListener('click', () => {
          syncControls();
          people.splice(index, 1);
          drawPeople();
        });
        const row = h('div', { class: 'card settings-proxy' },
          h('div', { class: 'grid-2' },
            field('Tag', tag, { hint: 'The word accounts carry. Accounts tagged alice egress through alice\u2019s proxy.' }),
            field('Label', label, { hint: 'Optional. Shown instead of the tag where the panel has room.' })),
          field('Proxy', proxy, { hint: 'A Tailscale exit node is usually a SOCKS5 endpoint on a tailnet address, for example socks5h://100.64.0.7:1080. Add it above first.' }),
          join,
          remove);
        list.append(row);
        controlsByPerson.set(person, { tag, label, proxy });
      }
    }
    drawPeople();

    const add = h('button', { class: 'btn', type: 'button' }, 'Add person');
    add.addEventListener('click', () => {
      syncControls();
      people.push({ tag: '', label: null, proxy: proxies.length === 1 ? proxies[0].id : '' });
      drawPeople();
    });
    const save = h('button', { class: 'btn btn-primary', type: 'button' }, 'Save people');
    save.addEventListener('click', async () => {
      const latest = await readConfig().catch((err) => {
        status.replaceChildren(errorBlock(err));
        return null;
      });
      if (!latest) return;
      syncControls();
      const nextPeople = people.map((person) => ({
        tag: person.tag,
        label: person.label,
        proxy: person.proxy,
      }));
      const blank = nextPeople.find((person) => !person.tag || !person.proxy);
      if (blank) {
        status.replaceChildren(h('div', { class: 'form-errors', role: 'alert' },
          'Each person needs a tag and a proxy. Add the proxy above first if the list is still empty.'));
        return;
      }
      // The server refuses duplicates too, but with a message about a tag rather than a row; the
      // card can say which row.
      const seen = new Map();
      const duplicate = nextPeople.find((person) => {
        const key = person.tag.toLowerCase();
        if (seen.has(key)) return true;
        seen.set(key, person);
        return false;
      });
      if (duplicate) {
        status.replaceChildren(h('div', { class: 'form-errors', role: 'alert' },
          `Two people share the tag ${JSON.stringify(duplicate.tag)}. Tags identify a person, so each one has to be unique.`));
        return;
      }
      latest.people = nextPeople;
      try {
        await api.putRaw('/api/config/raw', JSON.stringify(latest, null, 2));
        await renderValidation(status);
        ctx.toast.success('People saved.');
      } catch (err) {
        status.replaceChildren(errorBlock(err));
      }
    });
    peopleCard.replaceChildren(
      h('h2', {}, 'People and their machines'),
      h('p', { class: 'hint' },
        'A person here is a tag plus a proxy, so tagging an account with that tag sends its traffic through that machine — no per-account proxy to keep in step. An account\u2019s own proxy still wins, and a proxy that cannot be reached falls back to a direct connection.'),
      list,
      h('div', { class: 'dialog-actions' }, add, save), status,
    );
  }

  // --------------------------------------------------------------- quota display

  async function renderQuotaDisplay() {
    let config;
    try {
      config = await readConfig();
    } catch (err) {
      quotaDisplayCard.replaceChildren(h('h2', {}, 'Quota display'), errorBlock(err));
      return;
    }
    const display = config.quota?.display && typeof config.quota.display === 'object' ? config.quota.display : {};
    const mode = h('select', { class: 'input', 'aria-label': 'Quota display mode' },
      h('option', { value: 'chunks', selected: (display.mode ?? 'chunks') === 'chunks' }, 'Chunks'),
      h('option', { value: 'percentage', selected: display.mode === 'percentage' }, 'Percentage'));
    const chunks = h('input', { class: 'input mono', type: 'number', min: '1', max: '200', step: '1', value: String(Number.isInteger(display.chunks) ? display.chunks : 20), 'aria-label': 'Global quota chunk count' });
    const refreshOnLoad = toggle('Refresh quota when opening the Quota view', {
      checked: display.reset_on_load === true,
      help: 'When enabled, opening the Quota view refreshes every account from provider endpoints. This can make provider requests and hit rate limits; it does not run inference.',
    });
    const status = h('div');
    const save = h('button', { class: 'btn btn-primary', type: 'button' }, 'Save quota display');
    save.addEventListener('click', async () => {
      const count = Number(chunks.value);
      if (!Number.isInteger(count) || count < 1 || count > 200) {
        status.replaceChildren(h('div', { class: 'form-errors', role: 'alert' }, 'Global chunk count must be an integer from 1 to 200.'));
        return;
      }
      const latest = await readConfig().catch((err) => {
        status.replaceChildren(errorBlock(err));
        return null;
      });
      if (!latest) return;
      const quota = latest.quota && typeof latest.quota === 'object' && !Array.isArray(latest.quota) ? latest.quota : {};
      const oldDisplay = quota.display && typeof quota.display === 'object' && !Array.isArray(quota.display) ? quota.display : {};
      quota.display = { ...oldDisplay, mode: mode.value, chunks: count, reset_on_load: refreshOnLoad.input.checked };
      latest.quota = quota;
      try {
        await api.putRaw('/api/config/raw', JSON.stringify(latest, null, 2));
        await renderValidation(status);
        ctx.toast.success('Quota display settings saved.');
      } catch (err) {
        status.replaceChildren(errorBlock(err));
      }
    });
    quotaDisplayCard.replaceChildren(
      h('h2', {}, 'Quota display'),
      h('p', { class: 'hint' }, 'Choose how remaining quota is rendered. Chunk count is global across accounts and applies in chunks mode.'),
      field('Display mode', mode, { help: 'Chunks display approximate blocks of remaining allowance; percentage displays a percentage where the provider supplies enough information.' }),
      field('Global chunk count', chunks, { hint: '1–200 chunks per full allowance.' }),
      refreshOnLoad.row,
      h('div', { class: 'dialog-actions' }, save), status,
    );
  }

  // ---------------------------------------------------------------- quota probing

  async function renderQuotaProbe() {
    let config;
    try {
      config = await readConfig();
    } catch (err) {
      quotaProbeCard.replaceChildren(h('h2', {}, 'Quota probing'), errorBlock(err));
      return;
    }

    const section = config.quotaProbe && typeof config.quotaProbe === 'object' && !Array.isArray(config.quotaProbe)
      ? config.quotaProbe
      : {};
    // A section that names no list at all walks the router's built-in candidates, so the editor
    // opens on those. An explicit list is taken exactly as written — an empty one included, because
    // deleting every candidate is how an operator says "probe nothing here".
    const inherited = !Array.isArray(section.paths);
    const paths = (inherited ? QUOTA_PROBE_DEFAULT_PATHS : section.paths).map((path) => String(path));

    const walk = toggle('Walk the candidate list', {
      checked: section.enabled !== false,
      help: 'When this is off, an account whose provider has no dedicated quota reader and which declares no quota endpoint is left off the Quota view: nothing is requested from it and no allowance is shown. That is exactly the behaviour from before this section existed.',
    });
    const chatProbe = toggle(`Also try a one-token POST to ${QUOTA_PROBE_CHAT_PATH}`, {
      checked: section.chat_completion_probe !== false && section.chatCompletionProbe !== false,
      help: 'Tried only after every path above has answered nothing. It sends one short request and reads the rate-limit headers from the reply. That consumes a little of the account\'s allowance and is billed even when the reply carries no recognizable headers.',
    });
    // No `aria-label` here: the `field()` below gives this input a real `<label for>`, and a second
    // name would be the one screen readers announced.
    const model = h('input', {
      class: 'input mono', type: 'text', placeholder: 'the model this account uses',
      value: typeof section.model === 'string' ? section.model : '',
    });
    const list = h('div');
    const status = h('div');
    let inputs = [];

    // The list is the operator's order and is never sorted: moving a candidate is one splice, and
    // the number beside each row is the order requests are made in.
    function syncPaths() {
      inputs.forEach((input, index) => { if (index < paths.length) paths[index] = input.value; });
    }

    function checkInput(input, problem) {
      const bad = probePathProblem(input.value);
      problem.textContent = bad ? `This candidate ${bad}.` : '';
      if (bad) input.setAttribute('aria-invalid', 'true'); else input.removeAttribute('aria-invalid');
    }

    function drawPaths() {
      inputs = [];
      list.replaceChildren();
      if (!paths.length) {
        list.append(h('p', { class: 'hint' }, 'No candidates: no account is probed by this list. Add a path, or switch the walk off above if that is what you mean to say.'));
        return;
      }
      paths.forEach((path, index) => {
        const input = h('input', {
          class: 'input mono', type: 'text', value: path, placeholder: '/v1/balance',
          'aria-label': `Candidate ${index + 1} path`,
        });
        const problem = h('p', { class: 'hint' });
        problem.id = `probe-path-problem-${index}`;
        input.setAttribute('aria-describedby', problem.id);
        input.addEventListener('input', () => checkInput(input, problem));
        checkInput(input, problem);

        const up = h('button', {
          class: 'btn btn-sm', type: 'button', title: 'Move earlier', disabled: index === 0,
          'aria-label': `Move candidate ${index + 1} earlier`,
        }, '\u2191');
        const down = h('button', {
          class: 'btn btn-sm', type: 'button', title: 'Move later', disabled: index === paths.length - 1,
          'aria-label': `Move candidate ${index + 1} later`,
        }, '\u2193');
        up.addEventListener('click', () => { syncPaths(); reorder(index, index - 1); });
        down.addEventListener('click', () => { syncPaths(); reorder(index, index + 1); });
        const remove = h('button', { class: 'btn btn-sm', type: 'button', 'aria-label': `Remove candidate ${index + 1}` }, 'Remove');
        remove.addEventListener('click', () => { syncPaths(); paths.splice(index, 1); drawPaths(); });

        inputs.push(input);
        list.append(h('div', { class: 'settings-path' },
          h('div', { class: 'settings-path-row' },
            h('span', { class: 'hint mono settings-path-n' }, String(index + 1)),
            input, up, down, remove),
          problem));
      });
    }

    function reorder(from, to) {
      const next = moveStep(paths, from, to);
      paths.length = 0;
      paths.push(...next);
      drawPaths();
    }
    drawPaths();

    const addPath = h('button', { class: 'btn', type: 'button' }, 'Add candidate');
    addPath.addEventListener('click', () => { syncPaths(); paths.push(''); drawPaths(); });
    const resetPaths = h('button', { class: 'btn', type: 'button' }, 'Reset to the built-in list');
    resetPaths.addEventListener('click', () => {
      syncPaths();
      paths.length = 0;
      paths.push(...QUOTA_PROBE_DEFAULT_PATHS);
      drawPaths();
    });

    const save = h('button', { class: 'btn btn-primary', type: 'button' }, 'Save quota probing');
    save.addEventListener('click', async () => {
      syncPaths();
      // Caught here rather than at the first refresh: the router refuses the same entries, but it
      // cannot name them when the list came from an account rather than from this file.
      const problem = probePathListProblem(paths);
      if (problem) {
        status.replaceChildren(h('div', { class: 'form-errors', role: 'alert' },
          h('strong', {}, `Candidate ${problem.index + 1} cannot be probed:`),
          h('p', {}, `it ${problem.problem}.`)));
        return;
      }
      const latest = await readConfig().catch((err) => {
        status.replaceChildren(errorBlock(err));
        return null;
      });
      if (!latest) return;
      const existing = latest.quotaProbe && typeof latest.quotaProbe === 'object' && !Array.isArray(latest.quotaProbe)
        ? latest.quotaProbe
        : {};
      // A key this card does not edit (an account-wide override of something else) survives the
      // save; the three this card owns are written from the controls, always.
      const next = {
        ...existing,
        enabled: walk.input.checked,
        paths: [...paths],
        chat_completion_probe: chatProbe.input.checked,
      };
      const named = model.value.trim();
      if (named) next.model = named; else delete next.model;
      latest.quotaProbe = next;
      try {
        await api.putRaw('/api/config/raw', JSON.stringify(latest, null, 2));
        await renderValidation(status);
        ctx.toast.success('Quota probing saved. Refresh an account on the Quota view to try the new list.');
      } catch (err) {
        status.replaceChildren(errorBlock(err));
      }
    });

    quotaProbeCard.replaceChildren(
      h('h2', {}, 'Quota probing'),
      h('p', { class: 'hint settings-lede' },
        'Some providers report no allowance through an endpoint this router knows. For those, the router walks the candidate paths below against the account\'s own base URL, in order, and reads the first candidate that answers. Every request is made as that account, so it can count against the account\'s rate limit; a refresh from the Quota view is what runs the walk, and the page then names the candidate that answered. A single account can override this list in the configuration file, under its own quotaProbe key.'),
      walk.row,
      h('p', { class: 'hint settings-subhead' }, 'Candidates, in the order they are tried:'),
      list,
      h('p', { class: 'hint' }, inherited
        ? 'This deployment names no candidate list, so the router is walking the built-in candidates shown here. Saving writes this list into the configuration file explicitly.'
        : 'Each candidate is a path on the provider\'s own base URL: an account\'s credential is never sent to another host, so a full URL is refused rather than fetched.'),
      h('div', { class: 'dialog-actions' }, addPath, resetPaths),
      chatProbe.row,
      field('Model the one-token probe names', model, { hint: 'The router does not guess a model id. Leave this empty to skip the POST candidate even while it is enabled above.' }),
      h('div', { class: 'dialog-actions' }, save),
      status,
    );
  }

  // --------------------------------------------------------------------- pricing

  async function renderPricing() {
    const status = h('p', { class: 'hint', role: 'status' });
    const sync = h('button', { class: 'btn', type: 'button' }, 'Sync bundled prices');
    sync.addEventListener('click', async () => {
      sync.disabled = true;
      status.textContent = 'Applying bundled price metadata…';
      try {
        const result = await api.post('/api/pricing/sync', {});
        const changed = Number(result?.updated ?? 0);
        status.textContent = `Bundled price metadata synced; ${changed} configured model(s) updated. Manual prices are preserved.`;
        ctx.toast.success('Bundled pricing metadata synced.');
      } catch (err) {
        status.textContent = `Pricing sync failed: ${err.message}`;
      } finally {
        sync.disabled = false;
      }
    });
    pricingCard.replaceChildren(
      h('h2', {}, 'Pricing data'),
      h('p', { class: 'hint' }, 'Sync model price metadata from the bundled LiteLLM-derived table. This does not contact providers or estimate request costs; missing prices are unknown, not free.'),
      sync,
      status,
    );
  }

  // --------------------------------------------------------------- config helpers

  async function readConfig() {
    const raw = await api.getRaw('/api/config/raw');
    const config = JSON.parse(raw);
    if (!config || typeof config !== 'object' || Array.isArray(config)) throw new Error('Configuration must be an object.');
    return config;
  }

  async function renderValidation(target) {
    const validation = await api.get('/api/config/validate');
    const problems = Array.isArray(validation.problems) ? validation.problems : [];
    if (!problems.length) {
      target.replaceChildren(h('div', { class: 'form-ok', role: 'status' }, 'Configuration validated with no problems.'));
      return true;
    }
    target.replaceChildren(h('div', { class: 'form-errors', role: 'alert' },
      h('strong', {}, `${problems.length} configuration problem${problems.length === 1 ? '' : 's'} found:`),
      h('ul', {}, ...problems.map((problem) => h('li', {}, typeof problem === 'string' ? problem : (problem?.message ?? JSON.stringify(problem)))))));
    return false;
  }

  // ------------------------------------------------------------------ raw config

  async function renderRaw() {
    const ta = h('textarea', {
      class: 'input settings-raw', rows: '18', spellcheck: 'false',
      'aria-label': 'Raw configuration file contents',
    });
    const result = h('div');
    const loadBtn = h('button', { class: 'btn', type: 'button' }, 'Reload');
    const validateBtn = h('button', { class: 'btn', type: 'button' }, 'Validate');
    const saveBtn = h('button', { class: 'btn btn-primary', type: 'button' }, 'Save configuration');

    async function loadRaw() {
      ta.value = 'loading…';
      try {
        ta.value = await api.getRaw('/api/config/raw');
      } catch (err) {
        ta.value = '';
        result.replaceChildren(errorBlock(err));
      }
    }
    loadBtn.addEventListener('click', loadRaw);

    validateBtn.addEventListener('click', async () => {
      validateBtn.disabled = true;
      result.replaceChildren(h('p', { class: 'hint' }, 'Validating…'));
      try {
        const v = await api.get('/api/config/validate');
        result.replaceChildren();
        const problems = Array.isArray(v.problems) ? v.problems : [];
        if (!problems.length) {
          // Green because it is genuinely good news: the config validates clean.
          result.append(h('div', { class: 'form-ok', role: 'status' },
            h('strong', {}, 'No problems found.'),
            v.writable === false ? h('p', {}, 'Note: the configuration file is not writable — saving will fail.') : null));
        } else {
          // The backend reports every problem at once; the list must keep them all.
          result.append(h('div', { class: 'form-errors', role: 'alert' },
            h('strong', {}, `${problems.length} problem${problems.length === 1 ? '' : 's'} found:`),
            h('ul', {}, ...problems.map((p) => h('li', {}, typeof p === 'string' ? p : (p?.message ?? JSON.stringify(p)))))));
        }
      } catch (err) {
        result.replaceChildren(errorBlock(err));
      }
      validateBtn.disabled = false;
    });

    saveBtn.addEventListener('click', async () => {
      // Destination of this confirm: PUT /api/config/raw overwrites the whole configuration
      // file with the editor's contents. Not a delete, but certainly worth a beat.
      const ok = await ctx.confirm({
        title: 'Overwrite the configuration file?',
        message: 'The file is the single source of truth — this save writes exactly what is in the editor, and the routing snapshot is recompiled.',
        confirmLabel: 'Save configuration',
        danger: false,
      });
      if (!ok) return;
      saveBtn.disabled = true;
      try {
        await api.putRaw('/api/config/raw', ta.value);
        ctx.toast.success('Configuration saved.');
        result.replaceChildren();
        await Promise.all([ctx.refreshPorts(), ctx.loadProviders(true), ctx.refreshHealth()]).catch(() => { /* refresh is cosmetic; the save already succeeded */ });
      } catch (err) {
        result.replaceChildren(errorBlock(err));
        ctx.toast.error('Save failed', err);
      }
      saveBtn.disabled = false;
    });

    rawCard.replaceChildren(
      h('h2', {}, 'Raw configuration'),
      h('p', { class: 'hint settings-lede' },
        'The whole config file, exactly as on disk (secrets omitted by the API). Structured views elsewhere in the panel edit the same file.'),
      ta,
      h('div', { class: 'dialog-actions' }, loadBtn, validateBtn, saveBtn),
      result,
    );
    await loadRaw();
  }
}