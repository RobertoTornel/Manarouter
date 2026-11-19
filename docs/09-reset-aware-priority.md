# Reset-aware priority

`reset-aware-priority` is `priority` with one change, and the change is the whole feature:

> **Steps with no usable account stay in the chain, at the back of the order, sorted by earliest
> reset. `priority` also leaves them at the back, but in the operator's authored order.**

If everything is healthy the two are indistinguishable. They differ only in how the demoted tail is
ordered, and only when something is cooling down — which is exactly when an operator needs it to.

## Why demotion rather than removal

The obvious implementation is to drop a step from the candidate list when its accounts are all rate
limited. It is simpler, and it is wrong in a way that is hard to notice.

Dropping the step also throws away the fallback: once every healthy step has failed, the request has
nothing left to try, even though the cooling-down step may be usable again by the time it is reached.
The step is still the operator's preference, just not available yet.

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
| All steps healthy | 1, then 2, then 3 | 1, then 2, then 3 |
| Step 1 cooling down | 2, then 3, then **1** | 2, then 3, then **1** |
| Steps 1 and 2 cooling down, 2 resets first | 3, then 1, then 2 | 3, then **2**, then 1 |

The last row is the point. Both strategies keep demoted steps in the chain; `priority` leaves them
in authored order, so a step that has been down longest can be tried last. `reset-aware-priority`
reorders only the demoted tail by earliest reset, so the step most likely to work comes first.

## Finding the earliest known reset

Each account cooling down carries an `until_ms` instant. It comes from, in order of trustworthiness:

1. A `retry-after` header, in either form.
2. A structured reset in the response body (`resets_in_seconds`, `resets_at`) on a recognised
   quota-exhaustion code.
3. A recovery instant stated **in words** in the body's message — `Resets every 24 hours`, `It
   resets in 13h 24m`, `try again in 45 minutes` — or as a **calendar date** — `It comes back on
   12 Nov 2026 at 15:05 UTC`, `Your next rolling 7-day period starts on 19 Nov 2026 at 01:29 UTC`.
   Either form is read only after an anchor word that promises recovery (`resets in`, `resets
   every`, `retry in`, `try again in`, `available in`; `comes back on`, `starts on`, `resets on`,
   `available from`, …), because a bare number or date in an error message is usually a request
   limit or a diagnostic and not a wait. A date must carry a day, a month name *and* a four-digit
   year; a clock is optional and assumed to be `00:00 UTC` when absent. A date in the past is
   ignored (the width of the new window is unknown, so the guess is safer than a negative wait) and
   so is anything more than ninety days away. This path exists because a spent daily window whose
   body names the reset in a sentence was otherwise read as an ordinary one-minute throttle, and the
   account spent every minute being refused again.
4. `default_cooldown_secs` from now — **a guess**, used when the provider gives no hint.

The body is read for a `429`, a `402`, a `403` and a `400`. The `400` is not a typo: a spent credit
balance sometimes arrives as a generic bad-request code with a message like *"You have insufficient
credits"*. Status `400` normally means the request was malformed — no cooldown, no account switch,
because another account would reject the same bytes — but a message that names money or a spent
balance is about the account, so a small fixed vocabulary (`insufficient credit`, `payment required`,
`quota exceeded`, …) promotes it to a quota cooldown. No reset is stated in those bodies, so the
guess applies and grows with repetition; the vocabulary names no provider, model or error code.
The same read drives a capability-addressed request that starts a fine-tuning job: a refusal for
money there cools the account and moves the creation to the next funded sibling, so the client sees
the second account's answer rather than the first account's bill.

A step's earliest reset is the **minimum** over its accounts, because any one account becoming usable
makes the step usable again.

That minimum is taken over the *effective* reset of each account, and the effective reset follows the
provider's `cooldown_scope`: a route is parked only while **every** applicable fact about it still
holds, so a model-scoped refusal is the latest of that account's account-wide and model-wide instants,
and a provider-scoped one also waits for the provider's model bucket. The demotion itself does not
care which shape decided it — only when the route becomes usable again does. See
[quota tracking](08-quota-tracking.md#what-a-refusal-is-recorded-against-cooldown_scope).

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

**1. The failure count survives the cooldown.** The relay keeps a consecutive-failure count on each
cooldown entry, resets it on a success, and reads it back to grow the next guess. It does **not** yet
consult `failures_before_dead` or move an account to `Dead`: an account that keeps failing stays in
the rotation between cooldowns, and the growth in mechanism 2 is what bounds the damage. The config
key is documented in [quota tracking](08-quota-tracking.md#failures-before-dead) as intent, not as
shipped behaviour.

**2. Backoff grows with repetition — for guesses, not for statements.** Each consecutive failure
multiplies the next cooldown when the provider gave no recovery instant, on the reasoning that a
provider which rejects the same retry three times is not honouring a minute-long window. The relay
reads the consecutive failure count back from `Health` and passes it to `cooldown_until`, so the
doubling applies: two doublings at most, so a bad day cannot park an account for a week.

A wait the provider **stated** is not multiplied. It is an answer rather than a guess, and doubling
`resets in 13h` into two days takes a healthy account out of rotation precisely when its quota
returns. If the provider then breaks its own promise, the next refusal cools it again — and that one
is a guess, so it may grow.

**3. The step is only tried after the healthy group is exhausted.** A demoted step is the last thing
tried, not the first. So a flapping step costs nothing while any healthy step is working — the
failure only appears once everything else has already failed, at which point trying it is correct
regardless. This part *is* implemented: `order_steps` partitions the steps and places the unavailable
group last.

Mechanisms 2 and 3 are live. Mechanism 1 is only half live: the count survives and drives the
growth, but nothing acts on `failures_before_dead`, so an account that keeps failing is throttled
harder each time rather than removed from rotation.

## Where the reset is applied

The reset is applied in the **relay**, on the selection that observes it. The relay keeps account health
in memory (`manarouter-relay/src/select.rs`): a `HashMap` of account id to `Cooldown { until_ms,
reason, failures }` behind an `ArcSwap`. A selection compares `now` against each entry's `until_ms`
lazily (`is_over(now)`). There is no timer thread in either process and no wake-up at `until_ms`.

Health is not carried in the compiled snapshot and there is no IPC per request: the snapshot holds the
authored ordering and each account's scheduling metadata, while the live view is relay-local and
rebuilt from `429`/`401` observations as they happen. Because that state is process-local and in
memory, a relay restart forgets every cooldown and treats all accounts as healthy again; a conductor
restart changes nothing about cooldowns, and the conductor being absent does not freeze them.

The request path reads `config.quota` from the relay's live state rather than from a compiled
constant, and that state is refreshed from `config.json` whenever the routing table is republished.
Editing `default_cooldown_secs` (60), `auth_failure_cooldown_secs` (900) or `live_signals_win`
therefore takes effect on the next republish, without a restart. (`failures_before_dead` is read
live too, but as documented above it is intent until it is wired into the request path.) Nothing
about the cooldown is persisted, so the panel's live view of it depends on relay-local state, not on
`state.sqlite`.

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
- **When a hard failure is wanted.** If the operator needs the request to fail rather than fall back
  — a compliance requirement, or a cost control where a silent fallback is unacceptable — neither
  strategy does that: both keep the cooling-down step at the back of the order. Use a single-step
  combo instead.
- **A single-step combo.** There is nothing to demote below.

## See also

- [Quota tracking](08-quota-tracking.md) — what "cooling down" means, and the signals behind it.
- [Key selection](05-key-selection.md) — the account-level layer.
- [Combos](06-combos.md) — the three strategies compared.
- [Routing snapshot](04-routing-snapshot.md) — how health reaches the relay without IPC.
- [Configuration](03-configuration.md) — `QuotaConfig` in full.
