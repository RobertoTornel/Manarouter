# Configuration

One file: `~/.config/manarouter/config.json`. It is the single source of truth for providers,
accounts, combos, aliases, client keys and tuning. The panel edits it, the `mr` CLI edits it, and an
operator can hand-edit it — all three write the same shape.

---

## 1. Why JSON, and why the file is the source of truth

An earlier revision used TOML, which is friendlier to hand-editing. It was rejected for one concrete
reason: **unknown-key preservation.**

Two instances of Manarouter may run different versions — that is the entire premise of the planned
cross-instance sync in `docs/22-sync-design.md`. A newer instance may add fields an older one has
never heard of, and when the older one saves the file it must not destroy them. With `serde_json` and
`preserve_order`, every struct here is `#[serde(default)]`, denies nothing, and captures unrecognised
members into an `extra` map that is written back out unchanged.

That is the difference between "downgrade is safe" and "downgrade silently corrupts the operator's
configuration". A compatibility property this important is worth more than TOML's comment support.

There is a test that asserts exactly this: it loads a file containing a fabricated
`future_top_level` key, a `future_provider_field` inside a provider and a `future_tuning_knob` inside
`latency`, saves the file again, and asserts all three survive.

**Unknown keys are data, not noise.** If you add a field to any struct in `mr-core/src/config.rs` or
`mr-core/src/model.rs`, the `extra` flatten map must remain — removing it turns every future
downgrade into data loss.

---

## 2. Locations

| Purpose | Path | Override |
|---|---|---|
| Configuration | `~/.config/manarouter/config.json` | `MANAROUTER_CONFIG_DIR` |
| Secret vault | `~/.config/manarouter/vault.{key,enc}` | `MANAROUTER_CONFIG_DIR` |
| Compiled snapshot | `~/.local/state/manarouter/routes.snapshot` | `MANAROUTER_STATE_DIR` |
| State database | `~/.local/state/manarouter/state.sqlite` | `MANAROUTER_STATE_DIR` |

The two directories are split on purpose. Configuration is small, human-edited, and worth backing up.
State is large, machine-written, and **safe to delete** — losing it costs usage history and cached
health, never configuration. Putting the snapshot and the database in a separate directory makes
"just delete the state and restart" a safe instruction to give someone.

`XDG_CONFIG_HOME` and `XDG_STATE_HOME` are honoured when set.

### Environment

| Variable | Effect |
|---|---|
| `MANAROUTER_CONFIG_DIR` | Configuration and vault directory. Wins over `XDG_CONFIG_HOME`. `mr --config-dir` and `manarouter-relay --config-dir` set the same variable, so a second instance can be pointed elsewhere without touching your real configuration. |
| `MANAROUTER_STATE_DIR` | Snapshot and database directory. Wins over `XDG_STATE_HOME`. |
| `XDG_CONFIG_HOME`, `XDG_STATE_HOME` | Standard XDG bases; `manarouter` is appended to each. |
| `MANAROUTER_CONTROL_PORT` | One-run override of `control.port`. The conductor parses no command-line arguments, so this is the only way to move the panel for a single run; `mr serve --control-port` sets it for the child. An unparseable value is a startup error, never a silent fallback. |
| `RUST_LOG` | Tracing filter for both services and `mr` (for example `RUST_LOG=manarouter_relay=debug`). On the relay it wins over `--log`; the conductor has no `--log` flag. |
| `MANAROUTER_FORCE_REFRESH` | Test-only. `=1` makes the opt-in live credential test force a real OAuth rotation instead of accepting a stored token that is not yet due to refresh. Never needed to run the router. |
| `NO_COLOR` | Disables ANSI colour in `mr` output. Colour is also off whenever stdout is not a terminal. |

---

## 3. Top-level schema

```jsonc
{
  "format_version": 1,

  "relay": {
    "host": "127.0.0.1",     // loopback by default: this process holds credentials
    "port": 20900,
    "reuse_port": true,       // carried in the schema but read by nothing: one socket is bound,
                              // with SO_REUSEPORT set unconditionally (see below)
    "backlog": 1024           // also carried but not applied to the socket today
  },

  "control": {
    "host": "127.0.0.1",
    "port": 20901,
    "allow_remote_without_password": false,
    "password_hash": null     // Argon2id, never a plaintext password
  },

  "providers": [ /* §4 */ ],
  "combos":    [ /* §5 */ ],
  "aliases":   [ /* §6 */ ],
  "client_keys": [ /* §7 */ ],

  "quota": {
    "enabled": true,
    "live_signals_win": true,
    "default_cooldown_secs": 60,
    "auth_failure_cooldown_secs": 900,
    "failures_before_dead": 3
  },

  "latency": {
    "prewarm": true,
    "pool_idle_secs": 90,
    "pool_max_idle_per_host": 4,
    "tcp_nodelay": true,
    "hedge_width": 3,
    "hedge_window_ms": 400,
    "hedge_max_requests": 4,
    "snapshot_swap_budget_ms": 50,
    "upstream_http2": true
  },

  "sync": {
    "instance_id": null,      // generated on first run
    "record_oplog": true,
    "enabled": false,         // no transport exists yet; see docs/22-sync-design.md
    "peers": [],
    "label": null
  }
}
```

Every field has a default, so a first run works with an empty file. `providers` is the one you must
populate before anything can route.

### `format_version`

The schema version of *this file*, independent of the snapshot format version. A file whose version
is at most one ahead of what the binary understands is loaded **and** writable. A file further ahead
is loaded on a best-effort basis and never saved, because it may express intent the older binary
cannot honour. `Config::is_writable()` reports this, and `validate()` produces an explanatory
warning.

### `relay.port` and `control.port`

Different ports, and by default different from other routers' so several can run side by side during
a migration. OmniRoute uses 20129; 9router uses 20127/20128. Manarouter defaults to 20900 (data
plane) and 20901 (control plane).

Ports are **next-restart**, not immediate. Changing one in the panel persists it and labels the change
"takes effect on next restart", because rebinding a listener underneath live connections would drop
them. There is a Restart button in the panel for when the operator is ready.

`validate()` reports it if the relay and control plane are configured for the same host and port,
since they cannot both bind.

### `relay.reuse_port` and `relay.backlog`

Both are carried in the schema so an existing file keeps loading, and both are **inert today**.
`reuse_port` does not change the listener count: the relay binds exactly one socket and sets
`SO_REUSEPORT` on it unconditionally. That flag is what lets a second *process* share the port; it does
not fan a single process out across CPUs. `backlog` is never applied to the socket, so you get the
operating system's default. Neither field is wrong to set, and neither does anything yet.

---

## 4. A provider

```jsonc
{
  "id": "openai",                      // stable; prefixes qualified model ids
  "label": "OpenAI",
  "dialect": "open-ai",                // see §8
  "base_url": "https://api.openai.com/v1",
  "auth": { "kind": "bearer" },        // see §9
  "capabilities": ["chat", "responses", "embeddings", "models"],
  "key_strategy": "priority",          // priority | round-robin | random | least-used
  "enabled": true,

  "accounts": [ /* one per credential */ ],

  "models": [ /* discovered from /models, or entered by hand */ ],
  "models_synced_at": null,

  "headers": { "openai-beta": "assistants=v2" },
  "route_overrides": { "messages": "chat/completions" },

  "connect_timeout_ms": 10000,
  "ttfb_timeout_ms": 90000,
  "idle_timeout_ms": 120000,
  "total_timeout_ms": 0,               // 0 disables

  "prewarm": true,
  "http2": "auto",                     // auto | required | disabled
  "note": null,
  "extra": {}
}
```

### `id` rules

The id must not be empty, must not contain `/`, and must not contain whitespace. The slash
restriction is not cosmetic: model ids are exposed as `provider/model`, so a slash inside a provider
id would make a qualified id ambiguous. `validate()` reports a violation.

Provider ids are also part of a secret reference (`provider/<id>/account/<id>`), so renaming one
orphans its credential. The CLI refuses to rename a provider that has credentials unless
`--force` is given.

### `key_strategy`

How an account is chosen from those that are available. See `docs/05-key-selection.md`.

| Value | Behaviour |
|---|---|
| `priority` | Lowest `priority` value first; ties broken by array order. Default. |
| `round-robin` | Advance a counter per request; `least-used`-by-rotation without per-account state. |
| `random` | Uniform choice among available accounts. |
| `least-used` | The account with the fewest recorded requests in the current window. |

All four **skip unusable accounts** — rate-limited, backed off, auth-failed or disabled. Account
health is never conflated with provider health; see §11.

### Discovery versus hand entry

Models come from two sources, and both are first class. Entries carry a `source` of `synced`,
`manual` or `inferred`.

Hand-entered metadata always wins over discovered metadata, because a discovery response knows
nothing about context length or which routes a model actually serves. A `synced` entry that collides
with a `manual` one updates the discovered fields and leaves the authored ones alone.

`hidden` removes a model from the panel's pickers without deleting it, which matters because a
provider's `/models` frequently lists more than the operator wants to see.

---

## 5. A combo

A combo is a **named model**: clients call it exactly like a model name.

```jsonc
{
  "id": "0193f2c1-...",                // UUIDv7, generated
  "name": "Brains",                    // what clients send as "model"
  "label": "Brains (best of three)",
  "steps": [
    { "provider": "codex",        "model": "cx/gpt-6-astra", "enabled": true, "weight": 1 },
    { "provider": "command-code", "model": "cmd/gpt-5.5",    "enabled": true }
  ],
  "strategy": "priority",              // priority | round-robin | reset-aware-priority
  "hedge": {
    "enabled": true,
    "participants": [0, 1],            // [] means "the first `count` enabled steps"
    "count": 2,
    "window_ms": 800,
    "winner_metric": "first-response-byte",
    "on_winner_fail": { "mode": "rerun-excluding", "max_rounds": 2 },
    "cancel_losers": true,
    "loser_grace_ms": 0
  },
  "sticky_success_window_secs": 1800,  // priority + hedge only; 0 disables the preference
  "midstream_policy": "restart",       // restart | terminate
  "max_retries": 1,
  "retry_delay_ms": 2000,
  "affinity": true,
  "compression": { "enabled": false, "mode": "rtk" },
  "context_length": null,
  "hidden": false,
  "enabled": true,
  "sort_order": 0
}
```

**Step order is exactly the operator's order and is never reordered.** It is the tie-breaker of last
resort in the hedged race, which is why nothing in the system sorts it. `validate()` refuses a combo
whose steps name a provider that does not exist, because discovering that at request time is the
worst possible moment.

`combo.signature()` is a `u64` digest of the routing-relevant fields. The conductor uses it to skip
recompiling a snapshot when nothing that affects routing changed.

The full semantics — including the eligibility gate and the four `on_winner_fail` modes — are in
`docs/07-hedged-race.md`. Note that `on_winner_fail` is internally tagged: the mode name goes in
`mode`, and `rerun-excluding` carries `max_rounds` while `stable-fallback` carries `step`, an index
into the combo's own step list. `sticky_success_window_secs` is the separate, already-implemented
preference for the last successful step; it applies only to `priority` combos with `hedge.enabled`,
and `0` disables it.

---

## 6. An alias

```jsonc
{ "from": "gpt-4o", "to": "openai/gpt-4o", "generated": false }
```

Aliases let a client keep using a familiar name while the canonical id drifts, and they make the
OmniRoute migration reproducible rather than magic: OmniRoute exposed some models under three names
(prefixed, short-prefix, bare), and the importer generates an alias for each so existing client
configurations keep working.

An alias target must resolve to something real: a combo id, a combo name, a provider id, or a model
id reachable through a provider. `validate()` reports an alias pointing nowhere.

Resolution is a single lookup and is **not** transitive — `resolve_alias` returns the target, and a
chained alias will not be followed. That is deliberate: a chain is a loop waiting to happen, and a
single hop is what the panel can display unambiguously.

---

## 7. A client key

```jsonc
{
  "id": "0193f2c1-...",
  "label": "claude-code",
  "key_hash": "…",                     // SHA-256 of the key; never the key itself
  "prefix": "mr-3f9a",                 // non-secret, for display
  "enabled": true,
  "allow_models": ["Brains", "openai/*"],
  "allow_routes": ["chat.completions"],
  "limit_requests": 0,                 // 0 means unlimited
  "note": null
}
```

The relay compares **hashes**, so a leaked configuration file does not leak a usable credential, and
the panel renders only the non-secret `prefix`. The plaintext is shown exactly once, when the key is
created, and is never recoverable afterwards.

`allow_models` and `allow_routes`, when non-empty, restrict what the key may address. Globs are
supported with a trailing `*`. An empty list means unrestricted.

---

## 8. `dialect` values

Serialised in kebab-case:

| Value | Speaks |
|---|---|
| `open-ai` | OpenAI Chat Completions (also the default for most compatible gateways) |
| `anthropic` | Anthropic Messages (`/v1/messages`) |
| `google` | Gemini `generateContent` |
| `codex` | OpenAI Responses API as served at `chatgpt.com/backend-api/codex` |
| `copilot` | GitHub Copilot's surface: OpenAI chat, OpenAI responses, and native `/v1/messages` |
| `antigravity` | Google Cloud Code `v1internal:*` |
| `command-code` | Command Code CLI's private `alpha/generate` envelope |

`Dialect::is_passthrough_to` decides whether a request can be forwarded verbatim. `open-ai` to
`open-ai` and `anthropic` to `anthropic` are the passthrough pairs; everything else is translated by
`mr-dialects`.

---

## 9. `auth` shapes

```jsonc
{ "kind": "bearer" }
{ "kind": "api-key-header", "header": "x-api-key" }
{ "kind": "header", "name": "authorization", "prefix": "Bearer " }
{ "kind": "query-param", "param": "key" }
{ "kind": "oauth", "platform": "openai-codex" }
```

`AuthKind` is data rather than an enum of provider names, so adding a provider that uses a novel
header arrangement needs no code.

**Cookie and web-session authentication is not supported at all**, by ratified decision. No provider
in the catalogue uses it, and a test enforces that. See `docs/21-security.md` §4 for the reasoning.

---

## 10. Validation

`Config::validate()` returns **every** problem it finds rather than the first, because an operator
fixing a file by hand benefits enormously from seeing the whole list at once. An empty vector means
valid. It checks:

- conflicting listener host and port;
- duplicate provider, combo, alias and client-key ids;
- provider problems, from `Provider::validate()`;
- a combo step naming an unknown provider;
- an alias pointing at nothing resolvable;
- a zero `hedge_width`, `hedge_max_requests` or `pool_max_idle_per_host`;
- a `format_version` newer than the binary understands.

Any `mr` command that would save an invalid configuration refuses to write it, prints every problem
and exits non-zero; `mr doctor` reports the same list without saving anything. `mr config validate`
itself is still a stub, so it cannot be the command you reach for. The conductor refuses to compile
a snapshot from an invalid configuration and keeps serving the previous one, so a typo degrades to
stale routing rather than to an outage.

---

## 11. Health is per account, never per provider

The single most important operational rule in this file, and the one both reference projects got
wrong.

A provider is not healthy or dead. **Its accounts are.** `ProviderHealth::from_counts(total, usable)`
rolls the accounts up:

| Total (enabled) | Usable | Reported |
|---|---|---|
| 0 | 0 | `Unconfigured` |
| > 0 | 0 | `Dead` |
| > 0 | < total | `Degraded` |
| > 0 | = total | `Healthy` |

Two consequences worth stating:

- **One account returning 401 does not disable the provider.** The other accounts keep serving.
- **A provider with every account deliberately disabled is `Unconfigured`, not `Dead`.** A
  deliberate operator action is not an outage, and painting the panel red for something working as
  instructed trains the operator to ignore the indicator.

Account state is stored per account id in `state.sqlite`, never in `config.json`, because it is
observed rather than authored.

---

## 12. Atomic writes

`Config::save` writes a sibling temporary file, calls `sync_all`, then renames. A crash mid-save must
never leave a truncated configuration behind, because a truncated configuration means the next start
has no providers. Rename within a directory is atomic on every platform this targets.

The config directory is created `0700` and private files are written `0600` **in the `open` call
itself** — creating and then `chmod`-ing leaves a window in which the file is world-readable, and for
a secret that window is the difference between a defence and a gesture.

---

## See also

- `docs/02-quickstart.md` — a working configuration in five minutes
- `docs/04-routing-snapshot.md` — how the configuration becomes the compiled table
- `docs/05-key-selection.md` — `key_strategy` in detail
- `docs/06-combos.md` — combos, strategies and worked examples
- `docs/21-security.md` — the vault, the panel and the threat model
- `docs/22-sync-design.md` — why unknown keys are preserved
