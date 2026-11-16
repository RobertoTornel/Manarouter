# Accounts, tags and egress

Three things about an account are easy to confuse, and this page keeps them apart: **which accounts a
step may use**, **what an account is tagged with**, and **where an account's traffic leaves from**.
They are independent. A tag is a note; a selection is a filter; egress is a route. The only place they
touch is that a tag naming a *person* also supplies that person's egress, so declaring who an account
belongs to is enough to send it through their machine.

> **Status.** All of this is implemented and tested: the authored fields in `mr-core/src/model.rs` and
> `mr-core/src/config.rs`, the compiled `StepPlan::accounts` and `AccountPlan::proxy` in
> `mr-core/src/snapshot.rs` (snapshot version 24), the request-path filter in
> `manarouter-relay/src/select.rs` and `server.rs`, the two-stage proxy fallback in
> `manarouter-relay/src/upstream.rs`, and the `mr person`, `mr account tag` and `mr combo step
> accounts` commands. All three are also editable in the panel — the account table's `Tags` column,
> Settings' People card, and the combo step's account checkboxes — see
> [the web panel](15-web-panel.md) and [implementation status](23-implementation-status.md).

---

## 1. Selecting the accounts a step may use

A combo step names a provider and a model. By default it may use **every** account of that provider,
which is what every combo did before this existed, so an existing configuration behaves identically.
A step may instead name the accounts it is allowed to use:

```jsonc
{
  "provider": "openai",
  "model": "gpt-5.5",
  "accounts": ["alice", "tag:work"]     // [] or absent means "every account"
}
```

Each entry is a **selector**, and there are exactly two kinds:

- an **account id**, matched exactly;
- a **tag**, matched case-insensitively. A bare word is a tag, and `tag:<name>` is the same thing
  spelled out. The prefix exists so a tag that happens to look like an id is unambiguous.

An id is checked first, so an account whose id is `alice` is selected by `alice` even if some other
account carries the tag `alice`. Selectors are trimmed, and an empty selector is a configuration
problem rather than a wildcard.

The selection is resolved **at compile time**, not per request. `ComboExpander::resolve_accounts` in
`mr-core/src/snapshot.rs` walks the provider's compiled account list in its authored order and keeps
the accounts that match, so the relay receives a `StepPlan::accounts: Option<Vec<AccountId>>` and
never re-reads the authored combo. Two consequences are worth stating:

- **The order is the provider's, not the selector's.** Writing `["bob", "alice"]` does not reorder
  anything; `key_strategy` still decides, over the smaller set.
- **A selection that matches only disabled accounts stays empty.** Disabled accounts are dropped
  before this point, so a step whose selectors name nothing usable compiles to `Some(vec![])` — a
  step that can serve nobody. It is deliberately *not* widened back to "all accounts": silently
  serving from an account the operator excluded is worse than a step that fails.

`Config::validate()` reports a selector that matches no account of the step's provider, naming the
accounts that do exist, and the CLI refuses to write one. That is a typo caught while the operator is
looking at it, not a routing decision.

### What a selection does to failover

The filter is applied to the candidate list before any account is tried
(`select::step_allows_account`, called from `attempt_step`). Failover therefore stays inside the
selection: if the selected account is parked, the step is unusable and the combo moves to its next
step. A healthy account that the step excluded does **not** rescue it. That is the point of the
feature — a step that must use your accounts must not quietly spend someone else's.

### From the CLI

```bash
# every step of a new combo, which is only useful when the steps share a provider
mr combo add coding --step openai/gpt-5.5 --step openai/gpt-5.5-mini --accounts alice,tag:work

# one step, at creation
mr combo step add coding --step openai/gpt-5.5 --accounts alice

# change an existing step, or let every account serve again
mr combo step accounts coding 0 --accounts alice,tag:work
mr combo step accounts coding 0 --all
```

`mr combo show` prints an `ACCOUNTS` column, `all` when the step has no selection. `--accounts` takes
a comma-separated list and may be repeated.

---

## 2. Tags

A tag is a short free-text note on an account: `alice`, `work`, `team-1`, `do-not-use`. It is stored
as `tags: Vec<String>` on the account, is matched case-insensitively, and is **not a security
boundary** — it is a label the operator writes and reads, and anything that can edit the configuration
can change it.

```bash
mr account add openai --label main --secret-stdin --tag alice --tag work
mr account tag openai main alice
mr account untag openai main work
mr account list openai          # the TAGS column
```

Tags are additive and idempotent: tagging an account with a tag it already has changes nothing, and
`untag` of a tag it does not have is a no-op rather than an error. Both are case-insensitive, so
`ALICE` and `alice` are the same tag.

Two things read tags:

- a combo step's `accounts` selectors (§1), so a step can be limited to one person's accounts;
- the person lookup below, which turns a tag into an egress route.

---

## 3. People, and why egress is per account

A **person** is a tag plus a proxy:

```jsonc
{
  "people": [
    { "tag": "alice", "proxy": "alice-laptop", "label": "Alice" },
    { "tag": "bob",   "proxy": "bob-desktop" }
  ]
}
```

```bash
mr person add alice --proxy alice-laptop --label Alice
mr person list
mr person remove alice
```

The reason this exists is a limitation of exit nodes. A Tailscale exit node is a **machine-wide**
routing decision: `tailscale up --exit-node=…` sends *everything* that machine does through the peer.
The router needs the opposite — one account's requests through Alice's laptop, another's through Bob's
desktop, in the same process, at the same time. So the unit of egress is the account, and the
mechanism is a proxy: each person's machine runs a SOCKS5 listener on its tailnet address, and the
account's requests are tunnelled to it.

```jsonc
{
  "proxies": [
    { "id": "alice-laptop", "url": "socks5h://100.64.0.7:1080" },
    { "id": "bob-desktop",  "url": "socks5h://100.101.42.9:1080" }
  ]
}
```

`socks5h` rather than `socks5` matters: the `h` makes the *proxy* resolve the provider's hostname, so
DNS happens on the person's machine and the router's own resolver never sees the query. With plain
`socks5` the name is resolved locally and only the connection is tunnelled, which leaks the lookup and
can resolve to a different address than the peer would.

UNCERTAIN: the exact Tailscale flag that opens a SOCKS5 listener varies by version — check
`tailscaled --help` on the peer. An SSH dynamic forward (`ssh -D`) to the peer works equally well and
is what the tests stand in for.

### Precedence

`Config::effective_proxy` resolves one proxy per account, in this order:

1. the account's own `proxy`, if set;
2. the proxy of the **first** tag on the account that names a person;
3. the provider's `proxy`;
4. nothing — the proxy pool's `proxy_strategy`, or a direct connection when the pool is empty.

So tagging an account `alice` is the whole configuration: it selects the account for a step *and*
sends it through Alice's machine, with no second step. An account that needs a different route than
its owner's — a shared account, a test account — sets its own `proxy` and wins.

The compiled `AccountPlan::proxy` carries the resolved id, so the relay does not repeat this lookup.
A proxy id that is missing or disabled at request time is a direct connection, not an error: a person
deleting a proxy should not take their accounts offline.

---

## 4. Falling back to a direct connection

A proxy that cannot be used must not fail the request. The fallback has two halves, and the split is
deliberate.

**Configuration failures resolve to direct before anything is sent.** `resolve_proxy` in
`manarouter-relay/src/server.rs` returns `None` — a direct connection — when the named proxy does not
exist or is disabled, when the pool has nothing to offer, when the proxy's `secret_ref` names a vault
entry that is not there, when the credential cannot be read, or when the URL does not parse into a
usable endpoint. Each of those logs a `warn!` naming the proxy, so a typo is visible in the log
without taking a provider offline.

**Transport failures fall back only when nothing was written.** A tunnel that never opened is retried
once, directly, by `Upstream::send`; the connector failed before a byte of the request left the
process, so the retry cannot double-charge. A tunnel that *opened* and then broke is not retried: the
request may already be at the provider, and sending it again would bill it twice. The same applies to
a response that stalls or a body that dies mid-stream. Those surface as `502 proxy_error` with the
message that the request is not being rerouted.

The distinction is carried by `UpstreamError::ProxyConnect` (safe to retry, derived from
`hyper_util`'s `Error::is_connect()`) versus `UpstreamError::Proxy` (never retried). A proxy that
answers `CONNECT` with a refusal is the first case — the tunnel never opened — so the account still
serves, directly.

---

## 5. Observing it

- `mr account list <provider>` — the `TAGS` column.
- The panel — **Providers** has a `Tags` column per account, **Settings → People** lists each
  person with the accounts carrying their tag, and a combo's step editor shows how many of the
  provider's accounts the step may use. `node --test panel/test/account-tags.test.mjs`,
  `panel/test/settings-people.test.mjs` and `panel/test/combo-step-accounts.test.mjs` pin that
  those editors send exactly the `tags`, `people` and `accounts` arrays described above.
- `mr person list` — each person's tag, label, proxy, and how many accounts are tagged for them.
- `mr combo show <combo>` — the `ACCOUNTS` column per step.
- The relay log — `the proxy tunnel could not be established; retrying directly`, and the
  configuration warnings above, each naming the proxy.
- The snapshot — `StepPlan::accounts` and `AccountPlan::proxy`, version 24. A snapshot written by an
  older binary is refused and recompiled, as always; see
  [the routing snapshot](04-routing-snapshot.md) §10.

---

## See also

- [Key selection](05-key-selection.md) — the strategy that orders whatever the selection leaves.
- [Combos](06-combos.md) — steps, strategies and how a combo compiles.
- [Configuration](03-configuration.md) — the authored `tags`, `accounts`, `proxies` and `people` fields.
- [The routing snapshot](04-routing-snapshot.md) — `StepPlan::accounts` and `AccountPlan::proxy`.
- [Security](21-security.md) — what a proxy does and does not protect.
- [Troubleshooting](20-troubleshooting.md) — a request that egresses from the wrong place.
