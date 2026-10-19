# Reset-aware priority

`reset-aware-priority` is `priority` with one change, and the change is the whole feature:

> **A step with no available account is demoted below every healthy step — never removed.**

If everything is healthy, it is indistinguishable from `priority`. It only behaves differently when
something is cooling down, which is exactly when an operator needs it to.

## Why demotion rather than removal

The obvious implementation is to drop a step from the candidate list when its accounts are all rate
limited. It is simpler, and it is wrong in a way that is hard to notice.

Removing the step means the operator's preferred model **silently stops being preferred**. It does not
come back when the quota resets, because nothing re-inserts it — the combination is re-derived from
the authored list on each request, and the authored list is unchanged. So the operator must edit the
configuration, or wait for something to reload it, to get their preference back.

And because nothing in the panel would say *why* the model was being skipped, the symptom is "the
router is using the wrong model" with no visible cause. An operator who has deliberately ordered
`subscription -> API key -> local` will find the router quietly using the API key and billing them,
with nothing on screen explaining it.

Demotion says the same thing without losing the information: *this step is wanted, it is simply not
available right now, and it goes back to its authored position the moment it is.*

## The algorithm

```text
1. Stable partition the combo's enabled steps into two groups:
     healthy      — at least one account is usable
     unavailable  — every account is cooling down, disabled, or auth-failed

2. Keep the operator's authored order inside each group.
     (a stable partition, not a sort — the input order must survive)

3. Order the unavailable group by earliest known reset, soonest first.

4. Traverse: healthy group in order, then unavailable group in order.
```

Step 2 is the part that matters most and the part most easily lost. A `sort_by_key` on "is this
available" in Rust is **stable**, so it happens to preserve order — but the moment someone adds a
second sort key, or reaches for `sort_unstable_by` for speed, the operator's priority order is
silently permuted. That is not a performance bug; it is a routing bug that changes which model answers.

Step 3 is a best-effort tiebreak. A step whose accounts are all cooling down is only reached when
every healthy step has failed, and by then the question is which demoted step is most likely to work.
The soonest reset is the best available answer.

## Everything healthy means identical to priority

This is the property that makes the strategy safe to enable by default:

| Situation | `priority` | `reset-aware-priority` |
|---|---|---|
| All steps healthy | Step 1, then 2, then 3 | Step 1, then 2, then 3 |
| Step 1 cooling down | Step 2, then 3, then **fail** | Step 2, then 3, then **step 1** |
| Step 1 and 2 cooling down | Step 3, then fail | Step 3, then step 1, then step 2 |

The middle row is the point. Under plain `priority`, a combo whose first step is temporarily
unavailable will not try it again even after a lower step has failed — the request just fails. Under
`reset-aware-priority`, the demoted step is still in the chain and gets its chance.

## Finding the earliest known reset

Each account cooling down carries a `reset_at` instant. It comes from, in order of trustworthiness:

1. A `retry-after` header, in either form.
2. A `reset`/`reset_at` field in the response body, parsed per provider family.
3. `default_cooldown_secs` from now — **a guess**, and marked as such in the panel.

A step's earliest reset is the **minimum** over its accounts, because any one account becoming usable
makes the step usable again.

When nothing is known, the step sorts by the guess. It stays in the unavailable group either way, so
an unknown reset degrades the ordering but never the correctness of the partition.

## The flapping problem

Here is where a naive implementation breaks, and it is worth being precise about.

An account's cooldown expires. The next selection treats it as eligible again. The step moves back to
the healthy group at its authored position. The next request uses it, the provider is still throttling,
and it returns 429 again. The account cools down, the step is demoted, and the cycle repeats.

Every cycle costs a failed request, and — worse — the step **interrupts** a working lower step to
fail. The router becomes less reliable by having the feature enabled.

Three mechanisms prevent this:

**1. The failure count survives the cooldown.** `failures_before_dead` is meant to count *consecutive*
failures and reset only on a success, so an account that fails, cools down, fails again is at two
consecutive failures, not back to zero, and after three it becomes `Dead`, leaves the rotation
entirely, and stops flapping. The relay does keep a consecutive-failure count on each cooldown entry,
but **it never reads it back**: no code consults `failures_before_dead` or transitions an account to
`Dead`, so an account that keeps failing is not yet removed from the rotation.

**2. Backoff grows with repetition.** Each consecutive failure is meant to multiply the next cooldown
rather than restarting it at the provider's stated value. A provider that says "47 seconds" three times
in a row gets a longer wait on the third, on the reasoning that its stated reset is not being honoured —
either it is throttling us harder than it says, or the reset instant is wrong. Either way, retrying
on the same schedule is not working. The doubling exists in `cooldown_until`, but the relay calls it
with `previous_failures = 0` at every site, so the growth never actually applies today.

**3. The step is only tried after the healthy group is exhausted.** A demoted step is the last thing
tried, not the first. So a flapping step costs nothing while any healthy step is working — the
failure only appears once everything else has already failed, at which point trying it is correct
regardless. This part *is* implemented: `order_steps` partitions the steps and places the unavailable
group last.

Only mechanism 3 is live. Mechanisms 1 and 2 are the intended stabilisers, and until they are wired a
badly throttled preferred step can still be retried on a fixed schedule.

## Where the reset is applied

The reset is applied in the **relay**, on the selection that observes it. The relay keeps account health
in memory (`manarouter-relay/src/select.rs`): a `HashMap` of account id to `Cooldown { until_ms,
reason, failures }` behind an `ArcSwap`. A selection compares `now` against each entry's `until_ms`
lazily (`is_over(now)`). There is no timer thread in either process and no wake-up at `reset_at`.

Health is not carried in the compiled snapshot and there is no IPC per request: the snapshot holds the
authored ordering and each account's scheduling metadata, while the live view is relay-local and
rebuilt from `429`/`401` observations as they happen. Because that state is process-local and in
memory, a relay restart forgets every cooldown and treats all accounts as healthy again; a conductor
restart changes nothing about cooldowns, and the conductor being absent does not freeze them.

Two consequences worth stating. The relay currently builds `QuotaConfig::default()` on the request
path rather than reading `config.quota`, so the documented `default_cooldown_secs` (60),
`auth_failure_cooldown_secs` (900) and `live_signals_win` values are the ones in force, but editing
those keys in `config.json` has no effect yet. And nothing about the cooldown is persisted, so the
panel's live view of it depends on relay-local state, not on `state.sqlite`.

## Worked example

A combo with three steps, where step 1's provider has a 20-minute cooldown starting at t=0:

| Time | Step 1 (`codex`) | Step 2 (`anthropic`) | Step 3 (`openrouter`) | Order tried |
|---|---|---|---|---|
| t=0 | cooling, reset 20:00 | healthy | healthy | **2, 3, 1** |
| t=5min | cooling, reset 15:00 | healthy | healthy | **2, 3, 1** |
| t=19min | cooling, reset 1:00 | healthy | healthy | **2, 3, 1** |
| t=21min | **ready** | healthy | healthy | **1, 2, 3** |
| t=21min + 1 req | fails again, reset 22:00 | healthy | healthy | **2, 3, 1** |
| t=22min, 2nd failure | fails, count 2, reset 23:00 | healthy | healthy | **2, 3, 1** |
| t after 3rd failure | still cooling, reset 24:00 | healthy | healthy | **2, 3, 1** |

The last row is where the intended design and today's relay diverge: the count reaching three is
stored, but nothing turns it into `Dead`, so the step does not leave the rotation and the cooldown
does not lengthen until `failures_before_dead` is actually wired into the request path. Note that in
every row where steps 2 and 3 are healthy, they are tried first — step 1 never interrupts a working
path, because that half of the mechanism is live.

## Two levels of skipping, and why not to conflate them

There are two distinct decisions, and it is easy to read one as the other:

| Level | Question | Mechanism |
|---|---|---|
| **Step** | Which model should answer? | `ComboStrategy` — this document. Demotes a step with no usable account. |
| **Account** | Which credential pays for it? | `KeyStrategy` — [Key selection](05-key-selection.md). Skips `disabled`, `rate-limited`, `backoff`, `auth-failed`, `quota-exhausted`. |

Both skip unusable things, but at different granularities. A step is unusable only when **every**
account in it is unusable; a single healthy account keeps the step healthy and at its authored
position. That is the correct behaviour: a provider with two keys and one rate limit is still a
perfectly good provider.

## When not to use it

- **Consecutive steps on the same provider.** Two steps pointing at the same provider will demote
  together and re-enter together, which adds ordering churn with no benefit. Use `priority` and let
  the account layer handle it.
- **When a hard failure is wanted.** If the operator needs to know that the preferred model was
  unavailable — a compliance requirement, or a cost control where falling back silently is
  unacceptable — then `priority` is honest and `reset-aware-priority` is not.
- **A single-step combo.** There is nothing to demote below.

## See also

- [Quota tracking](08-quota-tracking.md) — what "cooling down" means, and the signals behind it.
- [Key selection](05-key-selection.md) — the account-level layer.
- [Combos](06-combos.md) — the three strategies compared.
- [Routing snapshot](04-routing-snapshot.md) — how health reaches the relay without IPC.
- [Configuration](03-configuration.md) — `QuotaConfig` in full.
