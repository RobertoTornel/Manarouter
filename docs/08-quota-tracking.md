# Quota tracking

Quota is the reason most multi-provider setups break. A request fails not because a model is down but
because one credential has hit a limit, and the router must know that before it picks it again.

## Four things that get conflated

Other routers tend to use "quota" for all four of these. They behave differently, they are observed
differently, and conflating them produces a router that either gives up too early or retries into a
wall.

| Kind | Example | Observable by Manarouter? |
|---|---|---|
| **Rate limit** | 500 requests/minute, 200k tokens/minute | **Yes**, from response headers and `retry-after`. |
| **Subscription usage window** | A plan's rolling 5-hour or weekly allowance | **Partly.** Only when the provider exposes it; otherwise it is declared by the operator. |
| **Spend budget** | $20/month on an API key | **No.** A budget is a billing concept; the provider does not tell the router about it. |
| **Credit balance** | Prepaid account with $3.14 left | **Only** through an optional provider-specific probe. |

The honest position is that Manarouter tracks what it can observe and does not pretend to track what
it cannot. A budget is something you set in a provider's billing console; the router learns about it
the moment the provider starts returning 402 or 429, and not before.

## Live signals first, declared windows second

A request against a provider is itself a measurement. Every response carries evidence about how much
quota is left, and discarding it in favour of a locally declared limit is throwing away knowledge in
exchange for a guess.

So the rule is:

> **Read the upstream's own signals. Fall back to the operator's declared window only where the
> provider has said nothing.**

If the two disagree, the **live signal wins**, because it is a measurement and the declared window is
a configuration. This is ratified decision Q4, and the `live_signals_win` flag in `QuotaConfig` exists
so the operator can turn it off deliberately — for a provider whose headers are known to be wrong —
rather than because the router guessed.

### What is read

| Signal | Where | Notes |
|---|---|---|
| `retry-after` | Any 429 or 503 | Both forms: a number of seconds, and an **HTTP date**. The date form is the one implementations get wrong, and a naive parse of it yields a multi-decade cooldown. |
| `x-ratelimit-limit-requests`, `-remaining-requests`, `-reset-requests` | OpenAI-compatible | `-reset-requests` is a duration, not an instant. |
| `x-ratelimit-limit-tokens`, `-remaining-tokens`, `-reset-tokens` | OpenAI-compatible | Absent on many providers. Its absence is normal, not an error. |
| `anthropic-ratelimit-requests-*`, `-tokens-*`, `-input-tokens-*`, `-output-tokens-*` | Anthropic | Separate input and output token budgets, which most providers do not have. |
| `reset_at` / `reset` inside a 429 body | Various | **Not standardised.** The field name and its unit differ per provider, so each is parsed per provider family rather than guessed at generically. |

### When nothing is said

The configured `default_cooldown_secs` (default **60**) applies, and the account is marked
`rate-limited`.

**That is a guess, not knowledge**, and the panel shows it differently from a cooldown whose reset
instant was reported by the provider. An operator looking at a 60-second cooldown that keeps renewing
can see that the provider is not telling the router anything and that the configured default is what
is driving it.

## Configuration

```json
{
  "quota": {
    "enabled": true,
    "live_signals_win": true,
    "default_cooldown_secs": 60,
    "auth_failure_cooldown_secs": 900,
    "failures_before_dead": 3
  }
}
```

Per-account windows are declared on the account:

```json
{
  "provider": "openai",
  "label": "work",
  "windows": [
    { "name": "rpm", "kind": "rolling", "seconds": 60, "limit_requests": 500 },
    { "name": "daily", "kind": "fixed", "seconds": 86400, "limit_requests": 10000 },
    { "name": "weekly", "kind": "provider-declared", "seconds": 0, "limit_tokens": 0 }
  ]
}
```

### The three window kinds

| Kind | Starts | Resets | Use when |
|---|---|---|---|
| `rolling` | At the first request in the window | `seconds` after it started | The provider describes a sliding window, e.g. "500 requests per minute". |
| `fixed` | On a wall-clock boundary | On the next boundary | The provider's limit is aligned to the clock — hourly, or daily at midnight UTC. |
| `provider-declared` | Learned from the upstream's headers | When the upstream says | The provider tells you, and you do not want to guess its schedule. With `seconds: 0` and no limits, this window exists purely to record what the provider reports. |

The distinction between `rolling` and `fixed` is not academic. With a 60-second rolling window, a
burst at the start of one minute and another burst 61 seconds later is legal. With a fixed
per-minute window, the same two bursts may land in the same minute and be rejected. A router that
assumes the wrong one will either under-use its quota or overrun it.

## Why quota is per account, not per provider

Two API keys on one provider have independent limits. Two OAuth accounts on one subscription may have
separate allowances, or may share one, depending on the provider. But the *consumption* is always
per credential: a rate limit is applied to the key that made the request.

So the tracked unit is the account. A provider's health is a **rollup** and is never a statement
about quota:

```text
ProviderHealth::from_counts(total, usable)
  total  == 0  ->  Unconfigured   (nothing is configured, which is not a failure)
  usable == 0  ->  Dead
  usable == total ->  Healthy
  otherwise    ->  Degraded
```

`Unconfigured` and `Dead` are deliberately different states. A provider the operator has not set up
yet, or whose accounts they have all disabled on purpose, is not broken — and the panel must not paint
it red, or operators learn to ignore red.

## The cooldown lifecycle

```text
  request  ->  429 with retry-after: 47
                  |
                  v
       account.state = RateLimited
       account.reset_at = now + 47s        (from the header: knowledge)
                  |
                  v
       every KeyStrategy skips this account
       (priority moves on, round-robin skips it, least-used ignores it)
                  |
                  v
       a timer fires at reset_at
                  |
                  v
       account.state = Ready
       a reset-aware combo promotes the step back to its authored position
```

Under `reset-aware-priority`, a step whose accounts are **all** cooling down is **demoted below every
healthy step, never removed** — so the operator's preferred model is the first thing tried again the
moment its quota returns. That is documented in
[Reset-aware priority](09-reset-aware-priority.md).

What a client sees during a cooldown: nothing different, provided another step is usable. The request
succeeds against a lower-priority step. If every step for every account is cooling down, the relay
returns a 429 **of its own** — distinguishable from a forwarded one, which
[the API reference](18-api-reference.md) describes.

## Why a 401 and a 429 are treated oppositely

| | 401 / 403 | 429 |
|---|---|---|
| Meaning | The credential is wrong | The credential is fine, it is being throttled |
| Will a retry help? | **No** | **Yes**, after the reset |
| Cooldown | `auth_failure_cooldown_secs` (**900**) | `retry-after`, or `default_cooldown_secs` (**60**) |
| State | `auth-failed` | `rate-limited` |
| Panel wording | "Log in again" / "Check the key" | "Cooling down until 14:32" |

A 900-second cooldown on a 401 is not a penalty; it is an acknowledgement that the operator has to do
something before a retry can possibly work. Retrying a bad key every 60 seconds produces logs that
hide the one line that matters, and a decade of experience says the operator will not notice.

## Failures before dead

`failures_before_dead` (default 3) is the number of consecutive failures — of any kind, after cooldown
handling — before an account is marked `Dead` rather than `Degraded`.

The reason this is a count and not a boolean: a single failure is almost always the network, the
provider, or a transient 5xx. Marking an account dead on one failure means a brief provider wobble
takes a perfectly good credential out of rotation. Three consecutive failures is a pattern.

The counter **resets on any success**. That is what makes it a measure of consecutive failure rather
than of lifetime unreliability, and it is why an account that fails twice a day but succeeds a
thousand times is never marked dead.

## Hedging costs quota

This is stated plainly because it is a real cost that is easy to forget when reading about the
latency benefit.

If a hedge sends a request to three participants and one wins, **the other two also consumed quota**.
For a non-streaming request, a loser may have been cancelled before its first byte — which is why
`cancel_losers` is on by default and why cancelling early matters — but a loser that had already
begun generating has spent tokens that were billed.

Two consequences:

- **Hedging trades quota for latency.** On a subscription with a tight five-hour window, a hedge that
  triples request volume will exhaust it three times faster. [Hedged race §4](07-hedged-race.md)
  discusses when that trade is worth making.
- **The spend is attributed to each account, not to the combo.** Usage records show the losing legs
  against their own accounts. A combo-level view would be tidier but would misattribute the cost, and
  the cost is attached to the credential that was charged.

## Credit balances

Some providers expose a balance endpoint. Where one is known, the conductor probes it **off the
request path** on a long interval, and the panel shows the result.

A balance that cannot be fetched is shown as **unknown**, never as zero. Displaying an unreadable
balance as zero would suggest an account is empty when it may be perfectly full, and would send an
operator hunting for a problem that does not exist.

## What is recorded per request

| Field | Notes |
|---|---|
| Account and provider | Which credential was charged |
| Model | The model id as sent upstream, not the combo name |
| Prompt and completion tokens | **Optional.** A streamed response may report no usage at all. |
| Duration | Wall clock, end to end |
| Outcome | Success, upstream error, or router error |
| Hedge role | **Which participant won and which lost** |

Token counts are partial by nature. Many providers report no usage on a streamed response, and some
report it only on the final chunk. A usage figure that looks lower than reality usually means "the
provider did not tell us", not "the tokens were free", and the panel says which responses lacked
usage rather than silently summing what it has.

## Where the data lives

Usage and quota state live in **`state.sqlite`**, never in `config.json`. This matters for two
reasons: the configuration is meant to be readable, diffable and version-controllable, and a file
that changes on every request is none of those. Deleting `state.sqlite` loses history and cooldown
state but never breaks the router — every account simply starts `Ready`.

Retention is the operator's decision. There is no default pruning, because silently deleting an
operator's usage history is worse than an unpruned database, and the panel shows the file's size so
the decision can be made with information.

## Limitations

Stated so they are not discovered later:

- **Usage a provider does not report is not counted.** Streamed responses frequently omit it.
- **Requests that fail before reaching the provider are not counted against quota**, correctly — they
  did not consume any — but they do appear in the error rate.
- **Race spend is attributed per account**, not per combo, so a combo's total cost is not a single
  figure.
- **A dropped usage record is counted, not retried.** The usage channel is bounded so a slow disk
  cannot stall a request, and when it is full the drop is recorded and surfaced. Losing a usage row
  is preferable to adding latency to a completion, and pretending the row exists is worse than both.
- **Spend budgets and credit balances are not enforced.** The router can tell you a provider is
  returning 402; it cannot predict it.

## See also

- [Reset-aware priority](09-reset-aware-priority.md) — demotion when a step has no usable account.
- [Key selection](05-key-selection.md) — how accounts are chosen and how unusable ones are skipped.
- [Configuration](03-configuration.md) — `QuotaConfig` and account windows in full.
- [Hedged race](07-hedged-race.md) — the quota cost of racing.
- [Web panel](15-web-panel.md) — how cooldowns and health are displayed.
- [Migration](17-migrating-from-omniroute.md) — what happens to the reference router's quota data.
