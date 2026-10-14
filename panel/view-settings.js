// view-settings.js — ports and relay pool settings (with restart warnings), panel password, and
// the raw configuration editor with validate + save.

import { api } from './api.js';
import { h, chip, field, errorBlock, applyFieldErrors, clearFieldErrors } from './dom.js';
import { describePortChange } from './format.js';

export async function renderSettings(mount, ctx) {
  const portsCard = h('div', { class: 'card' }, h('h2', {}, 'Listen ports'));
  const latencyCard = h('div', { class: 'card' }, h('h2', {}, 'Relay connection pool'));
  const passwordCard = h('div', { class: 'card' }, h('h2', {}, 'Panel password'));
  const rawCard = h('div', { class: 'card' }, h('h2', {}, 'Raw configuration'));
  mount.replaceChildren(portsCard, latencyCard, passwordCard, rawCard);

  await Promise.all([renderPorts(), renderPassword(), renderLatency()]);
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
        preview.append(h('ul', { class: 'hint', style: 'padding-left:18px;' },
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
      h('p', { class: 'hint', style: 'margin-bottom:10px;' },
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
      h('p', { class: 'hint', style: 'margin-bottom:10px;' },
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
    const profile = h('input', { type: 'checkbox', 'aria-label': 'Low-resource profile', checked: lowResource });
    const advanced = h('details', { style: 'margin:12px 0;padding:10px;border:1px solid var(--line);border-radius:8px;' });
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
        // Preserve every raw/unknown field; only patch the two settings this view owns.
        config.latency = { ...latency, pool_idle_secs: idleValue, pool_max_idle_per_host: maxValue };
        await api.putRaw('/api/config/raw', JSON.stringify(config, null, 2));
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
      h('label', { style: 'display:flex;align-items:center;gap:10px;margin:12px 0;' }, profile, h('span', {}, 'Low-resource profile (30s idle, 2 connections per host)')),
      advanced,
      restart,
      h('div', { style: 'display:flex;gap:8px;margin-top:12px;' }, save),
      status,
    );
  }

  // ------------------------------------------------------------------ raw config

  async function renderRaw() {
    const ta = h('textarea', {
      class: 'input', rows: '18', spellcheck: 'false',
      'aria-label': 'Raw configuration file contents',
      style: 'font-family:var(--mono);',
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
            h('ul', {}, ...problems.map((p) => h('li', {}, typeof p === 'string' ? p : (p.message ?? JSON.stringify(p)))))));
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
        await Promise.all([ctx.refreshPorts(), ctx.loadProviders(true), ctx.refreshHealth()]);
      } catch (err) {
        result.replaceChildren(errorBlock(err));
        ctx.toast.error('Save failed', err);
      }
      saveBtn.disabled = false;
    });

    rawCard.replaceChildren(
      h('h2', {}, 'Raw configuration'),
      h('p', { class: 'hint', style: 'margin-bottom:10px;' },
        'The whole config file, exactly as on disk (secrets omitted by the API). Structured views elsewhere in the panel edit the same file.'),
      ta,
      h('div', { class: 'dialog-actions' }, loadBtn, validateBtn, saveBtn),
      result,
    );
    await loadRaw();
  }
}
