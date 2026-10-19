# Combos

Use this guide to create and maintain named model routes, and to understand how their ordered steps are tried.

## What a combo does

A combo is a custom model name backed by one or more ordered `provider/model` steps. An application calls the combo by putting its name in the request's `model` field; it uses the usual relay endpoint, not a special combo URL. See [calling the API](03-calling-the-api.md) and [models and reasoning levels](04-models-and-reasoning-levels.md).

Use provider and model identifiers that are configured in your Manarouter instance. Replace every angle-bracket placeholder below with your own identifier; do not type the brackets. A combo name must be the name you want clients to call.

## Create and inspect a combo

```bash
mr combo add <COMBO_NAME> \
  --step <PROVIDER>/<MODEL> \
  --step <OTHER_PROVIDER>/<OTHER_MODEL> \
  --strategy priority
mr combo show <COMBO_NAME>
```

The add command confirms the combo and the number and order of steps. `show` prints the strategy, enabled state, and a numbered step table. Step numbers are zero-based. Use `mr combo list` to see every combo's strategy, enabled state, and status (`ready`, `disabled`, or `no enabled steps`). A ready combo still depends on its configured providers, accounts, models, and credentials being usable.

If a client key is restricted to an allow-list of models, allow the combo name as well as any direct model names that client needs. See [client keys](02-client-keys.md) and [the CLI reference](12-cli-reference.md).

## Choose how steps are tried

- `priority` (the default) keeps the authored order within each group, while steps with a usable account are tried before steps that are currently unavailable.
- `round-robin` rotates the candidate order; currently unavailable steps are still placed after available ones.
- `reset-aware-priority` keeps available steps in their authored order and moves unavailable steps after them; among unavailable steps with a known account cooldown, the earliest cooldown expiry is tried first. It does not remove those steps from the combo.

For example, use `--strategy reset-aware-priority` instead of `--strategy priority` when you want healthy steps preferred while a step's account is cooling down. The strategy uses the relay's account-availability/cooldown state, not declared quota-window counters or a live provider quota meter. See [quota and usage](06-quota.md) for the current quota limits.

## Edit, disable, or remove

Use `mr combo show <COMBO_NAME>` to check the current numbered order before editing. The CLI accepts zero-based positions:

```bash
mr combo step add <COMBO_NAME> --step <PROVIDER>/<MODEL> --at 0
mr combo step move <COMBO_NAME> 1 0
mr combo step remove <COMBO_NAME> 1
mr combo show <COMBO_NAME>
```

Adding without `--at` appends the step. The CLI reports the inserted, moved, or removed step; `show` lets you verify the resulting order. A combo must retain at least one step, so removing its last step is refused.

To pause a combo without deleting its configuration, run `mr combo disable <COMBO_NAME>`; `mr combo enable <COMBO_NAME>` makes it callable again. Removing a combo is different: `mr combo remove <COMBO_NAME>` deletes it. If aliases or client-key allow-lists still refer to it, removal is refused unless `--force` is supplied; force removal also drops those references. Review them first. See [troubleshooting](10-troubleshooting.md) before changing a route used by clients.

## Hedging: what it does and does not do

With `--hedge`, the relay dispatches the combo's participants **at the same time** and commits the
highest-priority participant that answers inside the window (`--window-ms`). It is not a
fastest-wins race: if the first step answers within the window, it wins even when a lower-priority
step answered earlier. That is the point — the combo's order is your choice of model, and hedging
buys insurance against the first one failing, not a lower average latency.

Two things to keep in mind:

- **It costs quota.** Every participant that was dispatched consumes provider quota, including the
  losers. A loser that had begun generating is billed.
- **A winner that dies mid-stream is replaced only if somebody is left to replace it.** When the
  committed winner's body fails, the relay falls back to the highest-priority participant that had
  *passed* the same window. If the client has not seen a byte yet the switch is invisible; if it has,
  the relay announces it with a `manarouter.retry` event so the client can tell the two answers
  apart. A stream that breaks after a tool call was already emitted is never spliced — it ends with
  an error, because a half-emitted tool call is worse than a clean failure.

  The replacement is a participant that had already answered when the winner was committed. In the
  usual shape of a race — the preferred model answers first and alone — there is nobody else to fall
  back to, and the stream ends with an error as it always did. `on_winner_fail` has four modes; the
  one that runs is "pick the highest-priority that passed" (`pick-highest-passed`), which is also
  the default. The other three are accepted configuration that the relay ignores for now.

With a priority combo, enabling the hedge setting can also activate a process-local preference for
the last successful step.
