# Troubleshooting

Symptom → cause → fix. Start with `mr doctor`, which checks most of this in one pass.

- [The relay will not start](#the-relay-will-not-start)
- [Every request 404s](#every-request-404s)
- [Every request 401s](#every-request-401s)
- [No usable accounts](#no-usable-accounts)
- [Requests are slow but succeed](#requests-are-slow-but-succeed)
- [A model that used to work stopped](#a-model-that-used-to-work-stopped)
- [The panel is unreachable](#the-panel-is-unreachable)
- [Validation errors on save](#validation-errors-on-save)
- [A port change did not take effect](#a-port-change-did-not-take-effect)
- [A streamed response is truncated after a failover](#a-streamed-response-is-truncated-after-a-failover)
- [Usage numbers look wrong](#usage-numbers-look-wrong)
- [The migration did not import everything](#the-migration-did-not-import-everything)
- [Logs and bug reports](#logs-and-bug-reports)

---

## The relay will not start

| Cause | How to confirm | Fix |
|---|---|---|
| The port is in use | `ss -ltnp \| grep 20900` | Stop the other process, or change `relay.port` in `config.json` and restart (`mr config set-port` is not implemented yet). |
| The configuration is invalid | Any `mr` command that saves refuses and prints **every** problem | Fix them in one pass rather than one at a time. |
| The snapshot is missing | The relay logs `no usable compiled snapshot; compiling from the configuration` | Nothing to fix: the relay recompiles `routes.snapshot` from `config.json` on load, and any `mr` command that saves rewrites it. |
| The snapshot is corrupt | A checksum error naming a mismatch | Delete the snapshot; it is derived data and is regenerated from `config.json`. |
| A non-loopback bind with no password | The message says so explicitly | Set a password, or bind loopback. This is refused by design, not a bug. |

Deleting `routes.snapshot` is always safe. It is compiled from `config.json`, and if the two disagree
the config wins.

---

## Every request 404s

The `model` string matched nothing. Resolution order, first match wins:

1. Alias
2. Combo id
3. Combo name
4. `provider/model`
5. Bare model id, if exactly one enabled provider offers it

Work out which you meant:

```bash
mr combo list          # names and ids
mr alias list          # aliases
mr models list         # qualified provider/model ids
```

The error names the model it could not resolve, so compare that string against those three lists.
Common causes:

- **An alias you expect does not exist.** Aliases are not transitive: an alias pointing at another
  alias does not chain.
- **A bare model id that is offered by two providers.** It deliberately does not resolve, because
  picking one would be a coin flip. Use `provider/model`.
- **A combo that is disabled.** `mr combo show <name>` says so.
- **The provider is disabled**, so its models are not reachable and a bare id stops resolving.

---

## Every request 401s

| Cause | Fix |
|---|---|
| Client keys are configured and you sent none | Send one, or remove the keys if this instance should be open. |
| An Anthropic-shaped client sending `Authorization` | Both `Authorization: Bearer` and `x-api-key` are accepted on every route, so this is a malformed header rather than the wrong one. |
| The key is revoked or disabled | `mr key list`. |
| A key from a different instance | Keys do not sync. |

Note the asymmetry that surprises people: **with no keys configured, an unauthenticated request
proceeds.** A router on loopback with no keys is a development setup, and demanding a key before it
can be used at all would be hostile. If you are seeing 401s, keys *are* configured — check
`mr key list`.

---

## No usable accounts

Three distinct states, three different fixes, and the panel shows them differently:

| State | Meaning | Fix |
|---|---|---|
| `disabled` | Switched off, by a person | `mr account enable <provider> <account>` |
| `rate-limited` | Throttled; will recover on its own | Wait for the reset, shown in the panel. Nothing is broken. |
| `auth-failed` | The credential is rejected | Re-key the account, or log in again for OAuth. A retry will never help. |
| `quota-exhausted` | A declared window is full | Wait, or declare a larger window. |
| `backoff` | Repeated transient failures | Check the provider's status page; the backoff grows deliberately. |

**The case that confuses people:** a provider whose accounts are **all deliberately disabled** reports
`Unconfigured`, **not** `Dead`.

That is correct. An operator who disabled three accounts on purpose while switching providers has not
broken anything — the provider is parked. Red is reserved for things that are wrong, and if it were
shown here, operators would learn to ignore red.

`mr doctor` reports any provider with zero usable accounts, and `mr provider test <id>` makes one real
request so you can see the provider's actual response rather than a summary of it.

---

## Requests are slow but succeed

Four different problems, and they need different fixes. Distinguish them:

| Symptom | Cause | Fix |
|---|---|---|
| The **first** request after a restart is slow, the rest are fast | A cold connection pool: TCP and TLS handshakes | Expected. The `prewarm` flag is carried in the configuration but nothing calls the prewarm routine yet, so the first real request still pays for the handshakes. |
| Every request is consistently slow | A distant provider, or a model that is genuinely slow | Compare `mr provider test` across providers. The router adds microseconds; a provider adds hundreds of milliseconds. |
| Latency is **highly variable** and no model is consistently chosen | A hedge whose window is too short | Cannot be the cause yet: hedging is not wired up — `hedge_decide` is never called by the relay — so this row describes the intended design. |
| Slow only for one combo | A step earlier in the list is slow but not failing | `priority` has no opinion on latency. Reorder; a hedge ([Combo cookbook recipe 3](combo-cookbook.md#3-fastest-wins--a-hedged-race)) is not wired up yet either. |

**The router is almost never the cause of a slow request.** Its budget is under a millisecond of added
overhead against providers whose own TTFB is hundreds of milliseconds. If you suspect it anyway, see
[Latency benchmarks](19-latency-benchmarks.md) for how to measure rather than guess.

---

## A model that used to work stopped

| Cause | How to confirm |
|---|---|
| An OAuth grant expired | The account shows `auth-failed`. Log in again. Refresh tokens are single-use on some platforms, and a consumed one cannot be reused — a re-login is the fix. |
| The provider changed a model id | `mr models sync <provider>` and compare. |
| A key was revoked upstream | `mr provider test <id>` shows the provider's real error. |
| The step was demoted by reset-aware priority | `mr combo show <name>`; the panel marks a demoted step. It returns when its quota resets. |
| The step was disabled | `mr combo show <name>` marks it. |
| A pinned model version was retired | Switch to a floating alias. This is the trade a pin makes. |

---

## The panel is unreachable

| Cause | Fix |
|---|---|
| Bound to loopback and you are on another machine | That is the default. Tunnel it (`ssh -L 20901:127.0.0.1:20901 host`) rather than exposing it — the panel holds provider credentials. |
| Non-loopback bind with no password | **Refused at startup by design**, with a message saying how to set one. This is not a bug. |
| A reverse proxy strips the `Origin` header | Mutations are refused. Configure the proxy to preserve `Origin`, or use the CLI. |
| The port is wrong | `mr version` prints the config path; read the `relay`/`control` `port` fields there (`mr config path` is not implemented yet). A pending change needs a restart. |
| The conductor is not running | The panel is served by the conductor on its own port. `mr serve` starts both. |
| A service is `inactive` after a reboot but `systemctl --user start` works | Suspect a **systemd ordering cycle**, not a crash. A unit that is both `WantedBy=default.target` and `After=default.target` deadlocks at boot; systemd breaks the cycle by deleting a start job, so the unit is never started and is *not* marked failed. Check with `systemd --user --test` (reports `Found ordering cycle`) or `scripts/check-systemd-cycle.py <unit-dir>`. `scripts/post-reboot-check.sh` runs this and the rest of the post-boot checks in one command. `systemd-analyze verify` does not detect this. |

The panel runs on a **different port from the relay** and is never on the request path, so a panel
problem never affects a completion, and a completion problem is never the panel.

---

## Validation errors on save

Every problem is returned at once, in `error.details`. **Read them all** rather than fixing the first
and retrying — that is what the list is for.

```json
{
  "error": {
    "code": "validation_failed",
    "message": "The configuration has 3 problems.",
    "details": [
      { "field": "providers[2].base_url", "message": "must start with http:// or https://" },
      { "field": "combos[0].steps[1].provider", "message": "unknown provider \"openaii\"" },
      { "field": "quota.default_cooldown_secs", "message": "must be greater than 0" }
    ]
  }
}
```

An invalid configuration is **never written to disk**. If a save appears to have done nothing, it was
rejected — check the response body rather than the file.

---

## A port change did not take effect

It is not supposed to. A port change is **persisted but not applied**, labelled "next restart", and an
amber banner stays up until the restart happens.

Rebinding a listening socket closes the old one, which drops everything connected to it — including
an in-flight completion. An operator expects a settings change to take effect; they do not expect it
to kill a request that is mid-answer. So the restart is explicit: the **Restart** button in the panel,
or `mr serve` again.

The banner is dismissible for the session and returns on the next load, because forgetting that a
change is pending produces a confusing failure later.

---

## A streamed response is truncated after a failover

Working as intended, in one specific case — once mid-stream failover is wired up. In this tree
`midstream_policy` is compiled into the routing snapshot but the relay does not consult it: failover
happens only on the upstream response status, before any byte reaches the client. A stream that fails
after the first byte reaches the client is terminated with a body error; the relay does not restart it
on another model and does not emit the `manarouter.retry` boundary, so the guard described below is not
reached. Everything from here to the next section describes the intended design.

When the winning model dies mid-stream, `midstream_policy` decides between restarting and terminating.
But under **both** policies there is a hard guard: **once any `tool_calls` delta has been emitted, the
relay refuses to splice.**

A tool call is a structured object the client is already assembling. Half of one model's call plus
half of another's produces a tool call that is either invalid, or — worse — valid and wrong. Given
the choice between a clearly truncated stream and a silently incorrect function call, truncation is
correct.

What you see: the stream ends with `[DONE]`, and the partial tool call is what it is. What you do
about it is a client decision, and the honest one is to retry the turn rather than act on the partial
call.

If this happens often, the winning model is unhealthy. `mr provider test <id>`.

---

## Usage numbers look wrong

| Cause | Detail |
|---|---|
| A streamed response reported no usage | Very common. Many providers report usage only on the final chunk, and some never do. The panel distinguishes responses that lacked usage rather than silently summing what it has. |
| A usage record was dropped | The usage channel is bounded so a slow disk cannot stall a request. Drops are **counted and surfaced**, not hidden. |
| A losing hedge leg was attributed to its own account | Correct. The cost attaches to the credential that was charged, not to the combo. A combo-level view would misattribute it. |
| Requests that failed before reaching a provider are missing | Correct. They consumed no quota. They still appear in the error rate. |

Token counts are partial by nature. A low figure usually means "the provider did not tell us", not
"the tokens were free".

---

## The migration did not import everything

Expected, in three specific ways — each is a decision, not a defect
([Migration](17-migrating-from-omniroute.md)):

| What | Why |
|---|---|
| Six web-session providers were **refused** | Cookie and browser-session authentication is never supported, for any provider, behind any flag. They are reported as refused, not skipped, because a policy and a bug look different to an operator. |
| `kiro` and `fishaudio` were **skipped** | No `Dialect` can express an AWS CodeWhisperer event-stream provider or fishaudio's bespoke TTS shape. |
| OAuth accounts need a **fresh login** | A grant is bound to the client that obtained it. Importing it would produce accounts that look configured and fail on the first request. |

The migration prints a table of everything it did not import with the specific reason for each. If
something is missing from that table, that is a bug.

---

## Logs and bug reports

```bash
# Raise verbosity for one run.
RUST_LOG=manarouter_relay=debug,manarouter_conductor=debug,mr_core=debug mr serve

# Per-component.
RUST_LOG=manarouter_relay=trace mr serve      # the request path
RUST_LOG=manarouter_conductor=debug mr serve  # the control plane
```

**Logs never contain credentials.** Tokens, keys and authorization headers are masked or omitted —
including at trace level. A `Debug` rendering of any token-bearing type prints the shape, not the
value. If you find a credential in a log line, that is a security bug worth reporting.

A useful bug report contains:

1. `mr doctor` output.
2. The exact command or `curl` that failed.
3. The full response, including headers.
4. `mr version` and how it was built.
5. Relevant `RUST_LOG` output — **redacted**. Read it before pasting; check for a provider key or an
   `Authorization` header.

Please do not paste a real credential into an issue. A masked prefix is enough to identify which one
is at fault.

## See also

- [API reference](18-api-reference.md) — what each response is supposed to look like.
- [Quota tracking](08-quota-tracking.md) — the account states in full.
- [Key selection](05-key-selection.md) — why an account was skipped.
- [Web panel](15-web-panel.md) — what each indicator means.
- [Latency benchmarks](19-latency-benchmarks.md) — measuring rather than guessing.
- [Migration](17-migrating-from-omniroute.md) — what a migration deliberately leaves behind.
