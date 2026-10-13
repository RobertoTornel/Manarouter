# Combos

A **combo** is an ordered list of `provider/model` steps published under a name the operator chooses.
A client passes that name in the ordinary `model` field, exactly as it would pass a real model id, and
the router decides which step actually answers:

```json
{ "model": "Brains", "messages": [ { "role": "user", "content": "hello" } ] }
```

There is no combo endpoint, no custom header and no SDK. That is the whole adoption story: a tool that
hard-codes a model name only needs that name changed in its own configuration, not its code.

This document is the reference for what a combo *is* and how the router treats it. The racing
behaviour that a combo can switch on is a separate subject and lives in [the hedged
race](07-hedged-race.md); the authoring syntax lives in [configuration](03-configuration.md) §5.

---

## 1. What a combo is

A combo has two identifiers, both of which a client may use:

| Field | What it is | Why it exists |
|---|---|---|
| `id` | A UUIDv7 generated at creation | Stable across a rename, so a client pinned to an id is never broken by a name change. |
| `name` | The operator's custom name | The human-facing handle, and the thing an existing client already sends. |

It also carries a `label` for the panel (display only), an `enabled` flag, a `hidden` flag, and a
`sort_order`. None of those affect routing except `enabled`; §9 explains exactly which fields do.

Each step names a provider, a model id as the **upstream** expects it, and whether the operator has
it enabled:

```json
{
  "provider": "openai",
  "model": "gpt-5.5",
  "enabled": true
}
```

`weight` is present in the schema but reserved: no current strategy reads it, and step order — not
weight — is what expresses preference. It is kept so a future weighted strategy does not need a
config migration.

---

## 2. Step order is the operator's priority order

The single most important property of a combo is that its step list is **exactly the order the
operator wrote**, never sorted, never re-weighted by the router, and never inferred from a benchmark.
`Combo::steps` documents this as authoritative. The order is the tie-breaker of last resort, which is
why nothing in the system is allowed to quietly rearrange it.

This is a deliberate rejection of "smart" routing. A router that reorders steps by observed latency
makes the operator's intent unreadable: after a change, nobody can say what the router will do without
reading a metrics history. An authored order is inspectable, diffable, and reviewable before it ever
sees traffic. If the priority is wrong, that is a configuration bug with a one-line fix.

Two consequences follow:

- **To change the priority, change the list.** There is no priority score, no ranking table, and no
  override field. Move the step, or disable it.
- **The only runtime reordering is a demotion, and it is bounded.** `reset-aware-priority` may move a
  step with no usable account *below every healthy step*. It never promotes a step ahead of a healthy
  one, and it never changes which steps are members of the combo. §3 covers when that is worth using.
- **A step the operator disabled is not a lower-priority step; it is gone.** Disabled steps are
  dropped by the compiler (§4), so they cannot be promoted back by any strategy.

---

## 3. The three strategies

`strategy` controls the traversal order of the step list. All three fall forward: when a step fails,
the next eligible step is tried. They differ only in where traversal starts and whether health can
change the order.

| `strategy` | Starts at | Health affects order | Use it when |
|---|---|---|---|
| `priority` (default) | step 0, every request | no | One model is genuinely preferred — a subscription model, the cheapest acceptable model, or the one the operator has decided is best. |
| `round-robin` | a rotating index | no | The steps are *equivalent* and the goal is to spread load across separate quotas or providers. |
| `reset-aware-priority` | step 0, but demoting unavailable steps | yes | Steps have independent reset schedules and the operator wants the best one that can actually serve right now. |

### `priority`

Always start at step 0; on failure, fall to step 1, and so on. Deterministic and sticky: the same
combo asked twice in a row goes to the same place.

Use it when the order encodes a real preference. This is the default because it is the only strategy
whose behaviour can be predicted by reading the configuration, and because it pairs naturally with
session affinity (§6): the best model answers, and every later turn of the same conversation keeps
going there instead of re-deciding.

### `round-robin`

Start at `n mod len` and fall forward from there, where `n` is a per-combo counter incremented once
per request. The counter is a relaxed atomic, so the cost is a single `fetch_add` — there is no lock
and no coordination between requests.

Use it for **equivalent** steps: two API keys on the same provider, or two providers serving the same
model at the same price and quality. Rotating them spreads load instead of pinning everything to one
account's quota while the other sits idle.

It is the wrong choice for steps that are *not* equivalent. Rotation means a request that would have
gotten the better model gets the worse one on purpose, so a round-robin list of a strong model and a
weak one degrades half the traffic. It also defeats prompt caching: consecutive turns of one
conversation land on different models, so the upstream cache key changes every turn. If a combo is
round-robin *and* `affinity` is true, the affinity map wins for the rest of a conversation — the two
are not contradictory, but the rotation then only happens across conversations, which is usually what
the operator wanted anyway.

### `reset-aware-priority`

Priority order, with one modification: a step whose accounts are all unavailable is **demoted below
every healthy step** rather than removed, and restored automatically by a timer when its earliest
known reset passes.

The rollup is per step but the state is per account: a step is demoted only when *every* account
backing it is unavailable. If a provider has three keys and one is throttled, the step keeps its
place and account selection routes around the dead key. This is what stops a mixed-quota provider
from being skipped wholesale and is the difference between "one key is cooling down" and "this
provider is out of quota".

Use it when steps reset on different schedules — an hourly free tier, a rolling five-hour
subscription window, a daily API cap. Plain `priority` would keep sending to the exhausted first step
and eat a failure on every request until the operator noticed; reset-aware demotes it for exactly as
long as the reset takes and promotes it back without human intervention.

It depends on reset information existing. With no quota signals and no observed `429` reset times,
every step looks healthy and the strategy degrades to plain `priority`. It is not a substitute for
configuring quota tracking.

> **Status.** All three values compile into the snapshot and are covered by tests. The round-robin
> counter and the reset-aware demotion loop execute in the relay and are in progress; the strategy
> enum itself is finished. See [overview](00-overview.md) §7 for the live status table.

---

## 4. How a combo becomes a snapshot

The authored combo and the compiled combo are different objects on purpose. `Combo` in
`mr-core/src/model.rs` is the rich, hand-editable form. `ComboPlan` in `mr-core/src/snapshot.rs` is
what the relay reads, and `RoutingSnapshot::compile` is the **only** place that interprets a combo.
A request never walks the authored list; it looks up a pre-resolved plan. The reasoning is in [the
routing snapshot](04-routing-snapshot.md) §1, and the short version is that interpretation on the
request path is both slower and racy against the next config save.

Compilation does this, in order:

1. **Disabled combos are skipped entirely.** An `enabled: false` combo does not exist in the snapshot,
   so calling it returns a normal "model not found" rather than an error explaining that it is
   disabled. Disabling is the off switch, not a soft hide.
2. **Disabled steps are dropped.** `enabled: false` on a step removes it from the compiled list.
3. **Steps naming a missing provider are dropped.** A step is kept only if its provider slug resolves
   to a compiled provider. Steps pointing at a *disabled* provider are dropped silently, because the
   operator disabled that provider on purpose.
4. **The two remaining cases are different failures.** If at least one step survives, the combo
   compiles and the dropped steps are only reported by `Config::validate()` as warnings. If no step
   survives *and* the reason was unknown providers, compile fails the whole snapshot with
   `Error::Snapshot` naming the combo — that combination is an operator typo, and failing loudly at
   compile time is much better than failing that one combo at request time. If no step survives
   because everything was disabled, the combo is silently omitted, because that is an intentional
   state, not a mistake.
5. **Indices are remapped.** Hedge `participants` and the `stable-fallback` step are written as
   indices into the authored step list. Compilation rewrites them onto the surviving list, so a
   dropped step cannot leave the race pointing at the wrong model. A stable step that no longer
   resolves has its mode replaced rather than left dangling.
6. **A race of one is downgraded.** If fewer than two participants resolve, the hedge is not a race —
   it is a normal request with bookkeeping — so the compiler turns it off instead of paying for the
   machinery.
7. **A per-plan signature is computed.** `ComboPlan::signature` is an FNV-1a digest of the plan's
   routing-relevant fields, used as the cheap "did anything I care about change" check. It is
   deliberately computed from the compiled plan, not from the authored `Combo::signature`, so changes
   the authored signature does not cover still move it.

The compiled plan carries the name, the strategy, the surviving `StepPlan`s (each with its dialect,
auth kind, base URL and timeouts already copied in from the provider), the resolved hedge, the
`midstream_policy`, the retry counts, `affinity`, and the compression spec. The relay never chases a
reference back into the provider table to find a timeout.

One hard bound: a combo may have at most `u16::MAX` steps, because the compiled plan indexes steps
with a `u16`. A larger list is a compile error, not a truncation.

---

## 5. Retries: `max_retries` and `retry_delay_ms`

```json
{
  "max_retries": 1,
  "retry_delay_ms": 2000
}
```

`max_retries` is how many additional attempts a whole combo attempt may make after the initial one
before the combo gives up. `retry_delay_ms` is the pause between attempts of the same step. The
defaults are **1 retry after a 2000 ms delay**.

The default delay is not arbitrary. The failure a retry is meant to survive is usually a transient
one — a dropped connection, a provider rolling a node, a `429` that clears in seconds — and an
immediate retry hammers a service that is already unhappy. Two seconds is long enough to be useful
and short enough that a person waiting on a chat response has not yet assumed it hung.

Both fields are bounded by `Combo::validate()`:

- `retry_delay_ms` above **60 000** is reported as "will look like a hang to most clients."
- `max_retries` above **10** is reported as "risks amplifying an upstream outage."

These are warnings, not hard limits. The panel surfaces them; nothing prevents an operator from
setting them anyway. The second one is the important one to internalise: retries multiply load on a
failing upstream, and the failure mode is a retry storm that keeps a struggling provider down. A
combo with several steps already has fallback; retries are for the transient case, not for papering
over a dead provider.

Retries and hedging compose. A hedge dispatches several steps at once; the retry budget applies to
the attempt that is committed to answering, following the combo's own rules. A losing participant
that is cancelled before its first byte costs nothing to abandon and does not consume a retry.

---

## 6. `affinity`

```json
{ "affinity": true }
```

`affinity` (default **true**) keeps the winning step of a conversation sticky for the rest of that
conversation. Once a combo has answered a session, later requests in that session go straight to the
model that answered, without re-running the strategy or the race.

The session key is either the client-supplied `x-manarouter-session` header or a hash of the
conversation's first system message plus its first user message. The `AffinityMode` vocabulary —
`first-system-and-user` (default), `header-only`, `header-or-content`, `off` — describes how that key
is derived; the per-combo field is the boolean above. The relay keeps a small sharded LRU map from
session key to chosen step, so the lookup is lock-free on the request path.

Two wins, and the second is easy to miss:

1. **Latency.** Only the first turn pays for a race. Every later turn goes directly to the known-good
   model.
2. **Cost and quality.** Upstream prompt caching keys on the conversation prefix. Staying on the same
   model keeps that cache warm, which is a real cost reduction, and it keeps the conversation's voice
   consistent — which matters for multi-turn agentic work where a mid-conversation model switch is
   visible in the answers.

Turn `affinity` off when stickiness actively hurts:

- **Stateless probes and one-shot requests.** There is no conversation to be sticky about, so the
  map is pure overhead.
- **A round-robin combo whose whole purpose is spreading load.** Affinity confines rotation to
  conversation boundaries.
- **Benchmarking.** To measure each step of a combo fairly, every request must re-run the strategy.

> **Status.** `affinity` is in the schema and compiled into the plan; the relay-side LRU is in
> progress. [The hedged race](07-hedged-race.md) §8 has the full rationale.

---

## 7. `midstream_policy`

A step can die **after** bytes have already reached the client. That is a different problem from a
step dying before it answered: the client has already seen part of a response and cannot be told to
start over. `midstream_policy` is the explicit switch for it, so there is no hidden third behaviour.

| Value | Meaning |
|---|---|
| `restart` (default) | Retry with the next best step according to the combo's rules. |
| `terminate` | Never retry once bytes are on the wire. Close the stream with an error. |

Under `restart`, if nothing has been flushed to the client the switch is invisible. If bytes are
already on the wire, the relay emits an explicit `manarouter.retry` SSE boundary and then continues
with the new model. A client that ignores unknown SSE events still sees a complete, terminated,
non-overlapping response — it simply gets a stitched one.

There is **one hard guard under both values**: if any `tool_calls` delta has already been emitted, the
relay refuses to splice and terminates with an error. A duplicated or half-emitted tool call corrupts
the client's state machine, and a clean failure is strictly better than a corrupted agent loop. This
guard is not configurable, because there is no correct way to restart a tool call that has already
been partially delivered.

Choose `terminate` when the client is strict about a single, contiguous stream — some SDKs treat any
unrecognised SSE event as a protocol error — or when a stitched answer is worse than no answer, as
with structured output. Otherwise `restart` is the better default: it converts a mid-stream provider
failure into a completed request.

---

## 8. Addressing a combo

A client addresses a combo by putting its name in the `model` field. Resolution is
ASCII-case-insensitive (model ids and provider slugs are ASCII by construction, and folding only
ASCII keeps the lookup allocation-free). `RoutingSnapshot::resolve` tries five ranks in this fixed
order and takes the first that matches:

| Rank | Form | Example | Notes |
|---|---|---|---|
| 0 | alias | `oad/DeepSeek-V4.1-Flash` | Aliases are flattened at compile time; whatever the alias chain ends at is what is matched. |
| 1 | combo **id** | `0193f2c1-…` | Stable across a rename. |
| 2 | combo **name** | `Brains` | The everyday form. |
| 3 | `provider/model` | `openai/gpt-5.5` | Names one exact step, bypassing every combo. |
| 4 | bare model id | `gpt-5.5` | Names the first provider in configured order that offers it. |

The order matters in exactly one direction: **an alias beats a combo of the same name, and a combo
name beats a bare model id.** The last rule is the useful one. Naming a combo after a bare model id —
for instance a combo called `gpt-5.5` whose first step is `openai/gpt-5.5` and whose second is a
fallback — means every existing client that hard-codes `gpt-5.5` now gets the fallback chain without
changing anything. The bare-model lookup still works for every *other* model id; it is only the
shadowed name that is redirected.

Two lookup details worth knowing:

- **Alias chains are resolved at compile time, not request time.** `AliasPlan::to` holds the final
  target after following any alias-to-alias hops. The request path takes at most one hop; the loop
  bound of `MAX_ALIAS_HOPS` in `resolve` is purely a guard against a snapshot built by something that
  did not flatten. An alias that would loop is rejected while compiling, not bounded at request time.
- **A bare model id resolves to one provider only.** The compiler keeps the first provider in
  configured order that offers the model and ignores later duplicates, so the bare form is
  deterministic — that tie-break is documented in the compile pass, not left to hash order.

`RoutingSnapshot::lookup` is the same resolution restricted to combos: a direct `provider/model` hit
returns `None` there, which is what callers that genuinely only handle combos want. A `hidden` combo
is still callable by either of its identifiers; `hidden` only keeps it out of panel pickers.

---

## 9. Changing a combo requires a recompile

Because the relay only ever reads a compiled `ComboPlan`, editing `config.json` does not change what
the router does until the conductor compiles a new snapshot and swaps it in. This is the direct cost
of having zero interpretation on the request path, and it is the reason a combo edit is a two-step
operation: save the config, then let the conductor republish.

The trigger is a change to `Config::routing_digest()`. For a combo it hashes `id` and
`Combo::signature()`, which covers the name, strategy, hedge enable/window/count/participants,
`midstream_policy`, `max_retries`, `retry_delay_ms`, and each step's provider, model and enabled flag.
Everything the compiler does not read is deliberately excluded, so a cosmetic edit — a `label`, a
`note`, a `sort_order` — does not republish the table. See [the routing snapshot](04-routing-snapshot.md)
§12 for the full list and the reason it is hashed field by field.

The failure behaviour is chosen, not incidental:

- **A failed recompile changes nothing.** The relay keeps serving the previous snapshot, so a typo
  cannot take routing down; it just means the edit is not live. `Config::validate()` is the place that
  reports the problem to the operator.
- **A compile failure is loud.** As in §4, a combo whose routable steps all name unknown providers
  fails the whole compile. Fix the provider, then republish.
- **The relay's pointer swap is atomic.** It uses `arc_swap`, so in-flight requests finish against the
  snapshot they started with, and the next request sees the new one.

The practical rule: if a change did not alter `routing_digest()`, it will not be visible, and if it
did, it will be visible after the next reload. A recompile is cheap, but it is not free — it briefly
costs a table rebuild and a pointer swap — which is exactly why cosmetic fields are excluded.

---

## See also

- [Configuration](03-configuration.md) — the authored combo JSON and the `config.json` section it lives in.
- [The routing snapshot](04-routing-snapshot.md) — how `ComboPlan` is compiled, encoded and swapped in.
- [The hedged race](07-hedged-race.md) — `hedge`, the eligibility window, and the winner-failure modes.
- [Combo cookbook](combo-cookbook.md) — copy-pasteable recipes for the goals above.
- [Key selection](05-key-selection.md) — how the account behind a step is chosen.
- [Quickstart](02-quickstart.md) — creating a first combo and pointing a client at it.
- [Architecture](01-architecture.md) — why compilation lives in the control plane.
- [Overview](00-overview.md) — the feature status table this document refers to.
