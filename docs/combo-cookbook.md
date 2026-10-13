# Combo cookbook

Recipes you can paste. Each one states what it is for and what breaks first, because a combo that
looks sensible and fails in a specific way is more useful to know about than one that works.

Addressing, strategies and the full field list are in [Combos](06-combos.md). Every command here
recompiles the routing snapshot and prints the new digest.

---

## 1. Subscription first, then API key, then local

The most common setup: use the plan you already pay for, fall back to metered billing, and keep a
local model as the last resort that costs nothing.

```bash
mr combo add coding \
  --label "Coding" \
  --step openai-codex/gpt-5-codex \
  --step anthropic/claude-sonnet-4-5 \
  --step ollama/qwen3-coder:30b
```

**Use when** the first step is a flat-rate subscription and the second bills per token.

**Breaks first when** the subscription step is rate limited. Plain `priority` will not retry it in
this request, so the request lands on metered billing. Add `--strategy reset-aware-priority` to keep
it in the chain for later steps and to restore it the moment its window resets.

---

## 2. Cheapest first

For bulk work where quality barely matters and cost dominates.

```bash
mr combo add bulk \
  --step deepseek/deepseek-chat \
  --step openrouter/qwen/qwen3-32b \
  --step openai/gpt-5-mini
```

**Breaks first when** the cheap model is *slow* rather than unavailable. `priority` has no opinion on
latency; a slow cheap model is used for every request. If that is the problem you have, you want
recipe 3 instead.

---

## 3. Fastest wins — a hedged race

Send to three models at once and take the highest-priority one that replies inside the window.

```bash
mr combo add fast \
  --step anthropic/claude-sonnet-4-5 \
  --step openai/gpt-5 \
  --step google/gemini-2.5-pro \
  --hedge --participants 0,1,2 --window-ms 250 \
  --winner-metric first-response-byte \
  --on-winner-fail pick-highest-passed
```

**Use when** per-request latency matters more than cost.

**Costs quota.** All three legs consume quota; a loser cancelled before its first byte costs little,
but a loser that had begun generating is billed. See
[Quota tracking §hedging costs quota](08-quota-tracking.md#hedging-costs-quota).

**Breaks first when** the window is too small for any participant. The window is an *eligibility
gate*, not a timeout — expiring it does not fail the request, it just means nobody won on priority
and the race resolves by the ordering rule. A 250 ms window against providers whose TTFB is 400 ms
means the priority ordering stops mattering.

---

## 4. Fail over to a different provider

When a provider-wide outage is the risk rather than a single model being slow.

```bash
mr combo add resilient \
  --step openai/gpt-5 \
  --step anthropic/claude-sonnet-4-5 \
  --hedge --participants 0,1 --window-ms 400 \
  --on-winner-fail pick-highest-passed-different-provider
```

Two participants, not three, and the failure mode is the one that will not hand the retry back to the
provider that just failed.

---

## 5. Local first, cloud as the escape hatch

For privacy-sensitive work: keep everything on the machine until a local model cannot cope.

```bash
mr combo add private \
  --step ollama/llama3.3:70b \
  --step vllm/qwen3-coder \
  --step openrouter/anthropic/claude-sonnet-4-5
```

**Breaks first when** the local model is up but bad. Availability is not quality: the router will
happily use a local model that produces unusable output, because nothing in the router measures
output quality. If you want the cloud model for hard requests, that is a client-side decision or a
second combo, not a routing rule.

**Also note** the third step is the one that leaks data off the machine. A combo is not a privacy
policy; make sure the operator knows the chain.

---

## 6. A long-context combo

Route by context size rather than by preference.

```bash
mr combo add longcontext \
  --context-length 1000000 \
  --step google/gemini-2.5-pro \
  --step anthropic/claude-sonnet-4-5 \
  --step openai/gpt-5
```

`context_length` is informational: the panel warns when a request will not fit, and the router
**does not truncate**. Silently dropping a conversation's middle is worse than a clear failure.

**Breaks first when** a client sends a request larger than every step's window. You get the provider's
own error, which is the correct outcome but may be less clear than you would like.

---

## 7. A tool-calling combo

For agents, where a model that emits malformed tool calls is worse than no model at all.

```bash
mr combo add agent \
  --step anthropic/claude-sonnet-4-5 \
  --step openai/gpt-5 \
  --midstream-policy terminate
```

`terminate` rather than `restart`, because an agent has usually already acted on the partial tool
call. Under **both** policies the relay refuses to splice once a `tool_calls` delta has been
emitted, so a mid-stream failure is a clean truncation rather than a corrupt call.

---

## 8. Free tier only

Nothing here bills, which is the point and also the limitation.

```bash
mr combo add free \
  --step google/gemini-2.5-flash \
  --step groq/llama-3.3-70b \
  --step cerebras/llama-3.3-70b \
  --strategy round-robin
```

`round-robin` rather than `priority`, because free tiers have independent rate limits and spreading
across them keeps more of them alive.

**Breaks first when** every free tier is exhausted simultaneously, which under load is the normal
state rather than the exception.

---

## 9. Reset-aware across providers with different resets

The combo `reset-aware-priority` exists for.

```bash
mr combo add steady \
  --strategy reset-aware-priority \
  --step openai-codex/gpt-5-codex \
  --step anthropic/claude-sonnet-4-5 \
  --step openrouter/qwen/qwen3-coder
```

**Use when** the steps have different reset schedules — a 5-hour rolling window on one, per-minute
rate limits on another.

**Identical to `priority` while everything is healthy**, so enabling it is low risk. See
[Reset-aware priority](09-reset-aware-priority.md).

---

## 10. Pin a model version

A combo with one step: defeats a provider silently moving a floating alias.

```bash
mr combo add pinned-gpt5 --step openai/gpt-5-2026-01-15
```

Also the cleanest way to expose a single model under a stable name for clients you do not control.

**Breaks first when** the pinned version is retired upstream. The failure is a provider error rather
than a silent upgrade, which is the point.

---

## 11. One combo per client

Different clients want different trade-offs. Give each its own name rather than making one combo do
both jobs.

```bash
mr combo add editor    --strategy priority    --step openai-codex/gpt-5-codex --step anthropic/claude-sonnet-4-5
mr combo add chat-bulk --strategy round-robin --step deepseek/deepseek-chat --step groq/llama-3.3-70b
mr combo add agent     --midstream-policy terminate --step anthropic/claude-sonnet-4-5 --step openai/gpt-5
```

Then point each tool at its own name. This is usually better than one clever combo, because "the
right trade-off" is a property of the workload and the workload is what differs.

---

## 12. Hide an internal combo from the model list

```bash
mr combo add internal-canary --hidden \
  --step openrouter/some/new-model \
  --step openai/gpt-5-mini
```

`hidden` means callable but not advertised on `GET /v1/models`. Useful for a canary you want to test
by name without every client's model picker offering it.

---

## Debugging a combo

```bash
mr combo show coding      # steps, numbered — the numbers are the priority order
mr doctor                 # every step resolves to a real provider and a usable account
mr provider test <id>     # one real request, with the provider's actual response body on failure
```

`mr combo show` is the one to reach for first. Almost every "the wrong model answered" report is a
step order that is not what the operator thought it was, and the numbered list settles it immediately.

## See also

- [Combos](06-combos.md) — addressing, strategies, every field.
- [Hedged race](07-hedged-race.md) — the full semantics of the race used in recipes 3 and 4.
- [Reset-aware priority](09-reset-aware-priority.md) — recipe 9 in depth.
- [Key selection](05-key-selection.md) — which credential pays for a step.
- [Quota tracking](08-quota-tracking.md) — what a cooldown does to a combo.
