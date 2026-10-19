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

## Important runtime limit: no concurrent hedge

The CLI accepts hedge-related options, but concurrent hedging is not wired into the relay: requests still try one step at a time. Do not enable hedge settings expecting two providers to race or latency to improve. With a priority combo, enabling its hedge setting can also activate a process-local preference for the last successful step; this is not a concurrent race.
