# Quota and usage

Use this guide to distinguish configured provider/account quota windows, transient cooldowns, and usage data that Manarouter does not yet record or show.

## What the panel shows today

The panel's **Declared windows** section reads the limits present in provider and account configuration. It can show a window kind and duration, request or token limits, and reset-only declarations. An account's own declared windows override its provider's windows when present; otherwise the provider's windows are the effective defaults. These are operator-entered settings, not a live reading of your provider's billing or remaining allowance.

The **Observed usage** section reads cached provider/account quota snapshots from `GET /api/quota`, including source, fetch time, windows, details, and per-account errors. Use the account refresh control or **Refresh all** to ask supported provider integrations to update their snapshot; OAuth accounts currently report refresh as unsupported. A configured provider/account window is only a declaration, not a measured allowance. The API returns `{ "accounts": [...] }` and preserves cached data alongside refresh errors.

This provider quota view is separate from request usage accounting. The **client-key** caps are a separate mechanism: the relay enforces the per-key rolling token and request caps on the request path and refuses a request that would exceed them with HTTP 429, before it contacts a provider. What the panel's provider view does *not* show is client spend: the relay writes the settled per-key usage rows into the store, but the panel's quota view reads the *provider* snapshots, not that ledger. Do not treat an empty request-usage history as zero provider usage.
## Cooldowns are not usage totals

The relay does temporarily cool down an account after certain upstream failures. A recognized numeric `Retry-After` on a rate-limit response can determine the cooldown; otherwise the relay uses its built-in default cooldown. The relay may then try a different step in a combo. This operational fallback is not a usage ledger, a billing-window counter, or a guarantee that the account's provider quota has reset. For diagnosing an upstream 429, see [troubleshooting](10-troubleshooting.md); for combo routing choices, see [combos](05-combos.md).

A quota-window row in configuration describes declared boundaries and request/token limits. A `reset_only` row marks reset timing without a limit. In this build, those declarations are shown in the panel but do not themselves count requests, enforce limits, or demote a combo step. Manarouter is not polling the provider or calculating remaining allowance from your upstream account.

## Interpretations to avoid

- **An empty observed-usage list is not zero provider usage.** This build has no service-path usage writer, so there may simply be no recorded rows.
- **`N/A` is not 0%.** In a remaining-allowance display, it means no remaining fraction was reported.
- **An unknown reset is not an expired reset.** Without a recognized reset instant, the panel cannot show a reliable countdown.
- **Configured limits are not verified upstream limits.** Check your provider's own account or billing dashboard for current consumption and reset details.

For safe credential handling and for supported CLI operations, see [security and credentials](11-security.md) and [the CLI reference](12-cli-reference.md). Do not paste provider credentials or client keys into commands shown in shared logs or conversations.
