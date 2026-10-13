# The hedged race

This is the most subtle behaviour in Manarouter, and the one most likely to be misread. The
important thing to understand is what it is **not**: it is not "send to several models and use
whichever answers first."

> **The winner is the highest-priority participant that produced a valid reply within the latency
> window.**

The latency window is an **eligibility gate**, not a stopwatch race. Reading it the other way round
produces a system that is slower *and* more expensive, so this document works through the
consequences carefully.

---

## 1. What a combo is

A combo is a **named model**. A client calls it exactly like a model:

```json
{ "model": "Brains" }
```

There is no separate endpoint and no special syntax. This matters for adoption: an existing tool
that hard-codes a model name only needs that name changed in its configuration, not its code.

A combo is an ordered list of steps in **exactly the order the operator put them in**. That order is
never sorted, never re-weighted by the router, and never inferred from a benchmark. It is the
operator's statement of preference, and it is the tie-breaker of last resort.

```jsonc
{
  "name": "Brains",
  "strategy": "priority",
  "steps": [
    { "provider": "codex",        "model": "cx/gpt-6-astra",        "enabled": true },
    { "provider": "command-code", "model": "cmd/gpt-5.5" },
    { "provider": "openadapter",  "model": "oad/DeepSeek-V4.1-Flash" }
  ],
  "hedge": {
    "enabled": true,
    "participants": [0, 1, 2],
    "count": 2,
    "window_ms": 800,
    "winner_metric": "first_response_byte",
    "on_winner_fail": "rerun-excluding",
    "max_rounds": 2
  },
  "midstream_policy": "restart",
  "limits": { "max_retries": 1, "retry_delay_ms": 2000 }
}
```

---

## 2. Strategies without hedging

Three strategies determine which single step is tried first. They are the whole story when
`hedge.enabled` is `false`, and they determine the *order* from which hedged participants are drawn
when it is `true`.

### `priority`

Always start at step 1. On failure, step 2. And so on. Deterministic and sticky.

This is the right default. It is predictable, it is the cheapest in upstream spend, and it keeps
upstream prompt caches warm because the same model sees the same conversation every turn.

### `round-robin`

Start at `n mod len`, then fall forward in priority order from there, wrapping. `n` is a per-combo
relaxed atomic counter, so advancing it costs one `fetch_add` and the relay holds no lock.

"Fall forward from there" is the part that is easy to get wrong. Round-robin chooses the *starting
point*; it does not mean "if this one fails, pick a random other one." The ordering after the
starting point is still the operator's priority order, so a failure degrades predictably.

Rotating across several accounts on one provider is the common use, because a provider's per-account
rate limits are usually independent. Note that this does not evade quota — it spreads load across
allowances the operator legitimately holds.

### `reset-aware-priority`

Priority order, with one modification: any step **with no available account** is demoted below all
healthy steps rather than removed.

*Demoted, not removed* is the important part. A step whose accounts are all inside a rate-limit
cooldown is not broken — it is temporarily unavailable, and it will come back. Removing it would
mean the operator's preferred model silently stops being preferred until a config reload; demoting
it means it returns to the front automatically when its earliest known reset passes, carried by the
same timer that clears the cooldown.

Within each bucket — healthy and unhealthy — the operator's order is preserved exactly. A
reset-aware combo therefore behaves identically to a priority combo whenever everything is healthy,
which is what makes it safe to turn on by default for a combo that spans providers with different
reset schedules.

---

## 3. The eligibility gate, worked through

The single rule again:

> Winner = the highest-priority participant that produced a valid reply within `window_ms`.

Worked timelines, with `window_ms = 800` and participants `[step1, step2]`:

### Case A — the preferred model is healthy

```
 0 ms   dispatch step1 and step2
150 ms   step1 first byte  ──►  step1 is highest-priority AND inside the window
150 ms   commit step1 as winner, cancel step2
```

**The race resolves at 150 ms, not at 800 ms.** No window wait, no added latency. Hedging costs
nothing when the preferred model is healthy — this is the single most important property, and it is
the reason a hedge is safe to leave on.

### Case B — a lower-priority model is faster

```
  0 ms   dispatch step1 and step2
200 ms   step2 first byte   (inside window)
600 ms   step1 first byte   (inside window)
600 ms   commit step1 as winner
```

**Step 1 still wins.** Both replied inside the window; step 1 is higher in the operator's order. The
hedge bought insurance against step 1 failing or stalling — *not* raw speed.

This is the behaviour that surprises people, so the reasoning is worth stating: if the operator
listed step 1 first, they want step 1. Silently substituting a model they ranked lower because it
happened to start generating 400 ms sooner would change the answer to their prompt without their
consent, and would make the combo's output non-reproducible. Speed is not the objective; *bounded
worst-case latency* is.

If an operator genuinely wants fastest-wins, the correct configuration is to put the fast model
first. The combo order is the control for that.

### Case C — the preferred model fails outright

```
  0 ms   dispatch step1 and step2
 90 ms   step1 returns 500  ──►  resolved as failed
200 ms   step2 first byte   ──►  every higher participant is resolved, so commit step2
```

The winner is the next highest participant that replied inside the window.

### Case D — nobody replies inside the window

```
  0 ms   dispatch step1 and step2
800 ms   window expires for step1
900 ms   step2 first byte
1200 ms  step1 first byte   ──►  commit step1
```

**The request does not fail.** The window expiring does not cancel anything; it only expires a
participant's *priority protection*. The race keeps waiting, and the highest-priority participant
that eventually replies still wins.

This is the case that distinguishes an eligibility gate from a timeout. A timeout-based design would
have failed the request at 800 ms or committed step2; this design commits step1 at 1200 ms because
step1 is what the operator asked for and it did, eventually, work.

### The general rule

Participant `i` is committed as winner as soon as **every participant above it** is resolved — where
resolved means either replied outside the window, or failed. Worst-case added latency is therefore
bounded by `window_ms`, and that cost is only ever paid when a higher-priority model is slow, which
is exactly the situation where waiting is the point.

---

## 4. `winner_metric`: what counts as "replied"

| Value | Definition | Cost of a losing hedge |
|---|---|---|
| `first_response_byte` | The first non-error byte of the upstream response **body** — time to first byte. | Approximately nothing. The loser has generated no billable output. |
| `first_complete_token` | The first fully-parsed content delta, or the complete response body when the client asked for a non-streaming reply. | One token of output on every participant that loses. |

`first_response_byte` is the default because it is nearly free: a losing participant is cancelled
after its headers, before it has produced any output tokens, so the only cost is a wasted request
slot. `first_complete_token` is available for providers that open a stream immediately and then
think for seconds — with those, TTFB says nothing about who will actually answer first.

Non-streaming requests are a special case worth noting: there is no "first byte of content" to key
on until the whole response arrives, because a non-streaming body is delivered as one document. The
relay therefore measures the first body byte, which for a non-streaming upstream is the completion
itself. This is documented rather than hidden because it changes the economics: hedging a
non-streaming request with a slow preferred model can cost a full completion on every loser.

Ties are always broken by the operator's step order. Never by arrival order, and never nondeterministically.

---

## 5. When the chosen model dies: four modes

`on_winner_fail` is evaluated when the committed winner fails — either before delivering anything, or
mid-stream. It has four values because there are four genuinely different things an operator might
want, and guessing between them would be wrong in at least three cases.

### `rerun-excluding`

Re-run the whole competition over the remaining participants, **excluding every model that has
already failed**, up to `max_rounds`.

This is the "run it again" behaviour. It is the most expensive mode and the most robust, because a
fresh race can find a winner that was not even a participant the first time. `max_rounds` bounds it
so that a systemic outage cannot turn one client request into an unbounded number of upstream calls.

### `pick-highest-passed`

Do **not** re-dispatch. Take the highest-priority participant that already **passed** the
eligibility gate in the first competition — its result is either already in the race's result set or
still in flight.

**Zero added latency.** This is the right mode when the operator's goal is a fast, safe answer and
they do not want the router to spend more upstream requests on recovery.

### `pick-highest-passed-different-provider`

As above, restricted to participants on a **different provider** than the one that died.

This exists because the most common real failure is not a bad model, it is a bad provider: an
outage, a revoked credential, or a provider-wide quota reset. In that case the sibling model on the
same provider will fail in exactly the same way, and picking it wastes the entire retry. Requiring a
different provider turns a provider outage from a stalled request into a fast, correct fallback.

### `stable-fallback`

Jump straight to a user-chosen `stable_model`, which need not be a participant in the competition
and need not appear anywhere in the combo's steps.

This is the "on fail" model. It is the right choice when the operator has one model they trust to be
always available and are willing to trade quality for availability exactly when things go wrong.

### Note on `participants`

`hedge.participants` lets the operator choose **exactly which steps enter the competition**,
independently of the full ordered step list. A model can therefore be part of the fallback chain
without ever being hedged — useful when a model is cheap but rate-limited, or expensive and reserved
for the case where everything else is down.

When `participants` is empty, the first `hedge.count` enabled steps are used. This is the "first X
models" behaviour, expressed as a default rather than a separate mechanism.

---

## 6. When the winner dies *mid-stream*

The ratified decision is that the prompt is retried with the next best model according to the mode
and rules inside the combo. That is a strong behaviour, and it is only safe in two shapes, so
`midstream_policy` makes the choice explicit rather than implicit.

### `restart` (default)

The retry proceeds.

- **If no bytes have reached the client**, the switch is completely invisible. The upstream died
  before it said anything, so the client simply sees a slower answer from the next model.
- **If bytes have already been flushed**, the relay cannot un-send them. It emits an explicit,
  documented boundary:

  ```text
  event: manarouter.retry
  data: {"reason":"upstream_failed","from":"cx/gpt-6-astra","to":"cmd/gpt-5.5"}

  data: {"choices":[{"delta":{},"finish_reason":"stop"}]}

  ```

  The `finish_reason` terminates the partial message cleanly; the following frames begin a new,
  complete, non-overlapping message. Clients that ignore unknown SSE events — which, per the SSE
  specification, they must — therefore still receive a structurally valid, terminated response
  rather than a truncated one.

### `terminate`

Never retry once bytes are on the wire. Close the stream with an error and let the client decide
whether to retry.

Choose this when duplicated or merged text in one message would be worse than a clear failure — for
example when the client is itself an agent that will retry, or when the response is being piped into
something that cannot tolerate two answers concatenated.

### The hard guard

**If any `tool_calls` delta has already been emitted, the relay refuses to splice and terminates
with an error — under both policies.**

This is not a preference, it is a correctness constraint. A client's tool-call state machine is built
incrementally from `tool_calls` deltas: it accumulates an id, a name and a partial JSON argument
string per index. Emitting a second, different tool call into that same accumulation produces either
a duplicate execution or a malformed argument string, and the client cannot tell which. A clean
failure is strictly better than a corrupted tool call, because a clean failure is recoverable.

The relay therefore tracks whether any tool-call delta has been written and consults that flag
before every splice attempt.

---

## 7. Why this is still "outside the router"

The relay evaluates **no policy**. It executes a `ComboPlan` the conductor already compiled:

- a fixed-length array of fully-resolved step plans,
- the participant index list,
- a `u8` count and a `u16` window,
- two enums (`winner_metric`, `on_winner_fail`),
- a set of `Arc<AtomicU64>` availability stamps.

Running a race *is* sending and receiving, which is exactly the relay's remit. Deciding order,
eligibility, participants and fallback is not, and it happens in the conductor, once per
configuration change rather than once per request.

The practical consequence: adding a new strategy or a fifth `on_winner_fail` mode is a conductor
change plus a new enum variant. The relay gains a `match` arm and nothing else — no new
configuration parsing, no new per-request allocation, no new lookup.

---

## 8. Session affinity

A hedged race is worth doing once per conversation, not once per turn. Affinity makes that happen.

The session key is either the client-supplied `x-manarouter-session` header, or a hash of the
conversation's first system message plus its first user message. The relay keeps a small sharded LRU
map from session key to the chosen step, and once a conversation has a winner it stays with that
winner for the session.

Two benefits, and the second is easy to miss:

1. **Latency.** Only the first turn pays for a race. Later turns go straight to the known-good model.
2. **Cost and quality.** Upstream prompt caching keys on the conversation prefix. Staying on the
   same model keeps that cache warm — both reference routers observed a meaningful cost reduction
   from exactly this — and keeps the conversation's voice consistent, which matters for multi-turn
   agentic work.

The map is lock-free and sharded, so it costs no lock on the hot path. Affinity is per-combo and can
be turned off for a combo whose steps are meant to rotate.

---

## See also

- `docs/09-reset-aware-priority.md` — how cooldowns and resets interact with step ordering
- `docs/06-combos.md` — the combo configuration reference
- `docs/16-latency-engineering.md` — the techniques that keep the race itself off the critical path
- `docs/08-quota-tracking.md` — what a losing hedge costs and how it is accounted
