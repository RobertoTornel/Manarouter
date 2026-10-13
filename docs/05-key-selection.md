# Key selection

A provider with several accounts is a pool, and choosing from it is the only piece of live scheduling
that happens per request. Everything else about a request was decided before it arrived. The strategy
is published in the compiled snapshot as a `KeyStrategy` on the `ProviderPlan`, and the account order
it reads is the order the compiler already resolved.

> **Status.** The strategy vocabulary, the `AccountPlan` fields, the `AccountState` set and the
> `ProviderHealth` rollup are implemented in `mr-core` and tested. The relay's selection loop is not
> yet written — `manarouter-relay` does not bind a listener in this tree. The algorithms below are the
> intended behaviour of the strategies as their own doc comments define them, and this document says
> where a rule is not enforceable from code yet.

---

## 1. What the relay has to work with

The snapshot's `ProviderPlan` carries `key_strategy` and an `accounts: Vec<AccountPlan>` that is
already ordered by ascending `priority`, with ties keeping authored order. Each plan has `id`,
`priority`, `weight`, `max_concurrent`, `models_allow`, `models_deny`, and credential *references* —
never a value.

Two facts about that list matter for everything below:

- **The relay must not re-sort it.** The comment on the field is explicit: the authored array order is
  the tie-break the operator sees in the panel. A relay that sorts by `(priority, id)` would break the
  determinism the operator configured.
- **Disabled accounts are already gone.** The compiler drops `enabled = false` accounts, so the relay
  never evaluates that flag. This is why `AccountState::Disabled` in a live table can only come from a
  change made after the snapshot was compiled — and why that change triggers a recompile instead.

The four strategies are a closed set (`KeyStrategy::ALL`-style exhaustive match), and they share one
property: **every strategy skips unusable accounts.** The difference between them is only which usable
account they pick.

---

## 2. The four strategies

| Strategy | Config spelling | Picks | Costs |
|---|---|---|---|
| `priority` | `priority` (also `first`, `fill-first`) | the first usable account in ascending `priority` | one scan to the first usable entry; keeps prompt caches warm |
| `round-robin` | `round-robin` (also `roundrobin`, `rr`, `rotate`) | the next usable account after the last one chosen | a rotation cursor, which is mutable state |
| `random` | `random` (also `rand`) | a uniformly random usable account | nothing to store; no fairness guarantee |
| `least-used` | `least-used` (also `leastused`) | the usable account with the fewest requests so far | a per-account request counter |

`KeyStrategy::parse` normalises case, underscores and spaces to hyphens, and accepts the aliases in
the table. `fill-first` is OmniRoute's spelling for priority, kept so a migrated configuration does
not need editing.

### `priority` — deterministic and sticky

Scan the accounts in compiled order and take the first one that is usable. That is the whole
algorithm.

It is the default (`KeyStrategy::Priority` carries `#[default]`), and it is the right default because
of a property that has nothing to do with fairness: a provider's prompt cache is warm on the account
that served the previous turn. Rotating accounts on every request of a conversation means paying
cold-cache prices on every turn. Priority keeps a conversation on one account until that account
becomes unusable, then moves to the next and stays there.

The cost is that the first account takes all the traffic until it breaks. If it has a rate limit and
you keep several accounts to avoid it, `priority` is the wrong choice — that is what `round-robin` is
for.

### `round-robin` — spread the load

Keep a cursor, take the next usable account after it, advance past the one chosen.

The important subtlety is what the cursor counts. If the cursor advances through the **post-filter**
list — the usable accounts at this moment — then an account that is temporarily rate-limited does not
consume a turn, and when it returns it re-enters at a fair position. If the cursor indexed the
*unfiltered* list and skipped unusable entries while advancing, an account in a long backoff would
consume turns it could not serve and shift the rotation permanently. The intended semantics are the
first: the cursor is over the eligible set for this request.

Because a round-robin cursor is mutable state that must be consistent across workers, it cannot live
in the immutable snapshot (see [routing snapshot](04-routing-snapshot.md) §8). Its exact placement is
a conductor-side decision, and it is the clearest example of the design's central split: the snapshot
publishes the *order* and the *strategy*; the counters live with the state, not the table.

### `random` — for unbalanced shards

Pick uniformly at random from the usable accounts. It stores nothing and needs no coordination between
workers, which makes it the only strategy whose behaviour is identical in one process and in eight.

Use it when the accounts are not interchangeable — several upstream keys behind an unbalanced shard,
or a pool where you cannot know in advance which one is fast. Documented in the enum as "useful when
upstream shards are unbalanced".

The cost is the mirror image of priority: no stickiness, so a conversation can land on a different
account every turn and lose its prompt cache. It also has no fairness guarantee beyond the law of
large numbers — over six requests it can easily pick one account four times.

### `least-used` — cost-aware balancing

Pick the usable account with the fewest requests so far. Ties are broken by compiled order, which
means priority order, which means the tie-break is deterministic rather than incidental.

The doc comment says "best for keeping a pool fresh when requests have wildly different costs". The
underlying metric is a request count, not a token count or a dollar amount — so two accounts can have
equal counts and wildly unequal spend. Treat it as a load spreader, not a budget balancer.

The cost is a per-account counter that must be readable on the request path and written after each
request. Nothing is free about a counter the relay writes: it is shared mutable state with the same
placement question as the round-robin cursor, and it has the same answer.

---

## 3. Worked examples

Take three accounts of one provider, all enabled, in compiled order A, B, C with `priority` 0, 1, 2 —
so the list the relay sees is `[A, B, C]`. All three allow the requested model, so health is the only
filter. Six consecutive requests, no failures:

| Request | `priority` | `round-robin` | `random` (one possible draw) | `least-used` |
|---|---|---|---|---|
| 1 | A | A | C | A |
| 2 | A | B | B | B |
| 3 | A | C | C | C |
| 4 | A | A | A | A |
| 5 | A | B | C | B |
| 6 | A | C | B | C |
| **Totals** | A×6 | A×2 B×2 C×2 | A×1 B×2 C×3 | A×2 B×2 C×2 |

- `priority` is deterministic and sticky: A serves every request until it becomes unusable.
- `round-robin` is a strict cycle; its cursor moves through the eligible set.
- `random` has no guarantee, and this draw is deliberately uneven — three of six to C — because
  unevenness is the honest expectation over a short run. A uniform draw is only fair in aggregate.
- `least-used` produces the same totals as round-robin here and differs in mechanism: it reads the
  counter rather than a cursor, so an unequal starting position stays unequal until the counts level.
  Starting with A at 10 requests and B and C at 0 selects B then C then A, whereas round-robin would
  start wherever its cursor happened to be.

### One account is rate-limited mid-sequence

The interesting case, and the one all four strategies must handle identically: request 3 returns 429
from A, so A becomes `RateLimited` with an `available_after`. Requests 4–6 see `[B, C]`.

| Request | `priority` | `round-robin` | `random` | `least-used` |
|---|---|---|---|---|
| 1 | A | A | C | A |
| 2 | A | B | B | B |
| 3 | **A → 429** | **C** | **C** | **C** |
| 4 | B | B | B | B |
| 5 | B | C | C | C |
| 6 | B | B | B | B |

Four behaviours are worth separating out.

**`priority` moves down and stays down.** From request 4 it selects B and keeps selecting B until B
fails or A's `available_after` passes. A is skipped, not removed: when its cooldown expires it is
eligible again, and because it is first in the compiled order it resumes serving. That is the desired
shape for a sticky strategy — the account that was preferred before the outage is preferred after it.

**`round-robin` advances past the ineligible account without giving it a turn.** The cursor is over
the eligible set, so with A out the cycle over `[B, C]` is B, C, B, C. Had the cursor indexed the
unfiltered list and skipped A while advancing, A's skipped turns would still consume rotation slots
and the pattern would be distorted. When A returns it rejoins the eligible set at the cursor's
position rather than at the front.

**`random` and `least-used` need the same filter as the other two.** A is excluded from the draw, not
merely deprioritised — "pick at random, then fall back if it is unusable" and "pick at random among
usable accounts" are different algorithms, and only the second is correct. The same applies to
`least-used`: the minimum is taken over eligible accounts, or an account that is out for an hour would
keep winning a comparison it cannot serve.

**A failed request is still a request.** `least-used` counts the 429 against A, because the request
was issued; the count reflects load, not successes. An account in a long backoff therefore accumulates
no new counts and looks *less* used when it returns. That is one more reason it is a load spreader and
not a health signal, and it is why the strategy must never be used as a proxy for "which account is
best right now" — that question belongs to `AccountState`, not to a counter.

The four strategies differ in which usable account they pick. They do not differ in whether an
unusable account can be picked, and there is no strategy — including `random` — for which that is a
probabilistic question.

---

## 4. What "unusable" means

An account is a candidate for selection if and only if it is in `AccountState::Ready`. Every other
state is skipped, by every strategy, without exception.

| `AccountState` | Config spelling | Set when | Clears when |
|---|---|---|---|
| `Ready` | `ready` | usable now | — |
| `Disabled` | `disabled` | the operator set `enabled = false` | the operator re-enables it (and the snapshot is recompiled) |
| `RateLimited` | `rate-limited` | the upstream returned 429 or a quota signal | `available_after` passes |
| `Backoff` | `backoff` | repeated transport or 5xx failures | `available_after` passes |
| `AuthFailed` | `auth-failed` | the token expired and could not be refreshed | credentials are repaired and verified |
| `QuotaExhausted` | `quota-exhausted` | every configured quota window is exhausted | the next window reset |

`AccountState::is_usable()` is a one-line match on `Ready`, and it is the *only* definition of
usable in the codebase. There is no second place where "can I use this account" is computed, which is
what stops the strategies from drifting apart.

Two consequences worth naming:

- **A state is an observation, not a configuration.** `Account::configured_state()` answers a
  different question — what the authored `enabled` flag implies before any traffic is seen — and
  returns `Ready` or `Disabled` only. The live state is a separate lookup.
- **The `Backoff` and `AuthFailed` states are the reason a pool recovers without an operator.** They
  are timers, not latches. A revoked key stays `AuthFailed` until it is repaired, because retrying it
  on every request is how a router turns one bad credential into a rate-limit penalty for the whole
  provider.

---

## 5. `models_allow` / `models_deny`, and where they apply

An account may be usable and still not be allowed to serve the model being requested. The filtering is
per model id, and it happens **after** health and **before** the strategy:

```text
accounts in compiled order
  → drop accounts whose live state is not Ready          (health)
  → drop accounts that do not allow the requested model  (permission)
  → apply the strategy to what remains                   (selection)
```

```text
allow = ["claude-*", "gpt-5"]      deny = []
→ serves claude-opus-4-1, claude-sonnet-4-5, gpt-5

allow = ["*"]                      deny = ["gpt-5-mini"]
→ serves everything except gpt-5-mini

allow = []                         deny = []
→ serves anything (an empty allow list is not "nothing", it is "no restriction")
```

`Account::allows_model` defines the rule exactly, and two details are easy to get wrong:

- **`models_deny` always wins over `models_allow`.** The deny check runs first and returns `false`
  immediately. That is what lets an operator write `allow = ["*"], deny = ["this-one"]` instead of
  enumerating everything else.
- **Matching supports a single trailing `*`.** `glob_match` is a prefix match, not a regex:
  `claude-*` matches `claude-opus-4-1`, and a `*` anywhere except the end is a literal character.
  `*` alone matches everything. This is deliberate — a regex engine is a large dependency for what is
  always a prefix match in practice.

The ordering matters when an account is the only one that allows a model. If the model-permitted
account is also rate-limited, the eligible set is empty and the request fails with the no-account
error in §7 — it does **not** fall through to an account that is healthy but forbidden to serve that
model. Routing a request to an account that must not serve it is worse than failing it.

`models_allow` and `models_deny` are copied into every `AccountPlan`, so the relay evaluates them
per request. [Routing snapshot](04-routing-snapshot.md) §5 records why that is not a compile-time
decision.

---

## 6. Health is per account, never per provider

This is the single most important operational rule in this document, and the one both reference
projects got wrong. A provider is **not** healthy or dead. Its accounts are. The provider-level value
is a *derived rollup*, computed by one function so the panel, the compiler and any future metrics
endpoint cannot disagree:

```rust
ProviderHealth::from_counts(total, usable)
```

where `Provider::health(state_of)` defines the inputs as:

```text
total  = accounts that are enabled                            (accounts dropped by the operator are not counted)
usable = enabled accounts whose live state is Ready
```

| Enabled accounts | Usable | Reported | Reads as |
|---|---|---|---|
| 0 | 0 | `Unconfigured` | nothing is set up here |
| > 0 | 0 | `Dead` | everything configured is failing |
| > 0 | < total | `Degraded` | something is wrong, but traffic flows |
| > 0 | = total | `Healthy` | fine |

Only `Healthy` and `Degraded` are `is_serviceable()`. `Dead` and `Unconfigured` both mean no traffic,
but they mean different things to the operator, and conflating them is how a panel trains people to
ignore it.

### The two subtle cases

**One account returning 401 does not disable the provider.** With six accounts and one `AuthFailed`,
the rollup is `Degraded`, not `Dead`, and selection routes around the bad account. This is the whole
reason health is tracked per account: a router that pools its provider's accounts into one health flag
takes an entire provider offline because one key was rotated on a Tuesday.

**A provider whose every account is deliberately disabled is `Unconfigured`, not `Dead`.** This
follows from `total` counting only *enabled* accounts: disable all three accounts and `total` becomes
zero, which is the `Unconfigured` row. It is not an accident of the arithmetic — it is the intended
semantics. A deliberate operator action is not an outage, and painting the panel red for something
working exactly as instructed teaches the operator to ignore red. `Dead` is reserved for the case
where accounts are configured and *none* of them can currently serve.

The distinction is not cosmetic. A `Dead` provider is a paging condition; an `Unconfigured` provider
is an unfinished setup. A `Degraded` provider is neither — it is a state the router handles silently
by selecting a different account.

---

## 7. When no account is usable

The eligible set can be empty for four distinct reasons, and they need four distinct messages:

1. the provider has no accounts at all;
2. every account is disabled by the operator;
3. every account is temporarily unavailable (`backoff`, `rate-limited`, `quota-exhausted`);
4. accounts are usable but none is permitted to serve this model (`models_allow`/`models_deny`).

The relay answers with a routing error — a 4xx the client can act on — and does **not** contact any
upstream, because there is nothing to contact. It must not be a generic 503 for three reasons:

- **A 503 tells the client "try again", and retrying is the one thing that cannot help.** With every
  account in backoff, a retry storm is the failure mode you least want to amplify. A 429-shaped
  failure with the earliest known reset is actionable; a 503 is an invitation to hammer.
- **The four causes have four different fixes**, and only the operator can tell them apart. "No
  accounts" is a missing step; "all disabled" is an instruction; "all cooling down" is a wait; "none
  permitted" is a `models_allow` mistake. One opaque error makes a five-second diagnosis into a
  support thread.
- **It is not an upstream failure.** The upstream was never called. Reporting a provider error for a
  local routing decision misattributes the fault and sends the operator to the wrong logs.

*The exact status code and body shape of this error are not fixed in code yet: the relay's router does
not exist in this tree. The design requirement is that the cause is distinguishable programmatically
— different codes or a machine-readable cause field — so a client can back off intelligently instead
of parsing prose. The generic error text is defined by `Error::NotFound` in `mr-core/src/error.rs`;
the HTTP mapping is the part still to be built.*

---

## 8. Reset-aware priority

`ComboStrategy::ResetAwarePriority` is the one combo strategy that changes traversal order at runtime,
and it exists because the alternative — removing an exhausted step from the chain — loses information
the chain needs.

**Unavailable steps are demoted, never removed.** A step with no usable account moves below every
healthy step and stays in the plan. When its earliest known reset (`available_after` on the account,
or the next quota-window reset) passes, it is **restored to its authored position**. Nothing about the
operator's declared order is destroyed, so restoring is a matter of letting a timer expire rather than
re-saving the configuration.

Removal would be simpler and wrong for three reasons:

- **The chain would silently shorten.** `hedge.participants` and `on_winner_fail`'s stable step are
  indices into the step list; removing a step renumbers everything after it. The compiler already has
  to remap those indices when a step is disabled — doing it again at runtime, per request, on a
  changing step set, is a class of bug with no upside.
- **The order is the operator's statement of preference, not a cache.** A step that is out for twenty
  minutes is still the step the operator wants first. A router that forgets that has to reconstruct it
  from a config the relay is not allowed to read.
- **`reset-aware-priority` is only needed because resets exist.** If the strategy removed steps, it
  would be indistinguishable from a chain that was edited by the router — and the operator's next
  panel view would disagree with the file they wrote.

The cost is visible in the relaying: a demoted step is still a place a request can go if everything
above it fails, and it may fail again immediately. That is the correct trade for a chain whose top
entries are quota-bound: trying a step that is about to reset is better than refusing the request, and
the account-level state machine refuses it cheaply if the reset has not actually passed.

`ComboStrategy::needs_availability()` returns true only for this strategy, which is how the machinery
knows to maintain per-step availability stamps for a combo — and, by omission, how it knows not to pay
for them under `priority` and `round-robin`.

The strategy vocabulary is `priority`, `round-robin`, `reset-aware-priority` (also `reset-aware`,
`resetaward`, `quota-aware`). The interaction with quotas and resets in full is developed in the
[combos](06-combos.md) reference and illustrated in a worked configuration in the
[combo cookbook](combo-cookbook.md).

---

## 9. Where the state lives, and how to observe it

Account state is **observed, not authored**, and it is stored accordingly:

| Data | Location | Written by |
|---|---|---|
| `enabled`, `priority`, `weight`, `models_allow`, `models_deny` | `config.json`, per account | the operator, the panel, or `mr` |
| Live `AccountState`, `available_after`, failure counts | the state database (`state.sqlite`), keyed by account id | the conductor, from traffic observations |
| Round-robin cursor, least-used counters | runtime state, not the snapshot | the conductor |

The rule stated in [configuration](03-configuration.md) §11 is unconditional: **never in
`config.json`.** Health is a moving observation; putting it in the authored file would mean the panel
rewrites the operator's configuration every time a request fails, the file's history becomes
unreadable, and a config copied between machines carries one machine's outages to the other. It would
also break unknown-key preservation's promise, which is that the file is exactly what a human or the
panel authored.

Observe it in two places:

- **The panel**, which shows the rollup per provider and the state per account. The rollup is
  `ProviderHealth::from_counts`, so what the panel shows and what selection computed are the same
  function.
- **Deleting `state.sqlite`**, which is always safe and is the supported way to clear a stuck state.
  Every account returns to `Ready` on next observation; nothing authored is lost. That is the test that
  the split was made correctly.

---

## See also

- [Configuration](03-configuration.md) — `key_strategy` in the authored file, and §11 on per-account health
- [Routing snapshot](04-routing-snapshot.md) — the `AccountPlan` the relay selects from
- [Combos](06-combos.md) — `reset-aware-priority` as a combo strategy
- [The hedged race](07-hedged-race.md) — what happens when the chosen step dies
- [Security](21-security.md) — how a credential is attached once an account is chosen
- [LLM_INDEX](LLM_INDEX.md) — the `KeyStrategy` and `AccountState` vocabularies
