# Migrating from OmniRoute

`mr migrate omniroute` imports providers, accounts, combos, aliases and client keys from an existing
OmniRoute installation. It is designed around two rules that override convenience:

1. **It reads a copy, never the original.**
2. **It does nothing until you tell it to.**

Everything else in this document follows from those.

## What can be imported

| Item | Imported | Notes |
|---|---|---|
| Providers | Yes, when the catalogue has a match | Matched by id. A provider with no catalogue entry is reported, not guessed at. |
| Credentials | Yes | Written to the vault. Only an id reaches `config.json`. |
| Combos | Yes | Names **and step order** are preserved exactly. |
| Aliases | Yes | Generated for the alternative model spellings the reference router exposed. |
| Client keys | Yes | The hash is carried over, so existing client configurations keep working. |
| Settings | Partially | Ports, timeouts and quota policy map. Anything with no equivalent is reported. |
| Usage history | No | Different schema, different meaning. Left behind. |
| OAuth grants | **No** | See below. This is not an oversight. |

### Why OAuth tokens cannot be migrated

An OAuth grant is bound to the client that obtained it. The access and refresh tokens in the
reference database were issued to *that* client, with *its* redirect URI and *its* client id, and
the token endpoint will reject a refresh presented by a different client — usually with
`invalid_grant`, which is indistinguishable from an expired token.

Importing them would produce accounts that look configured, pass a superficial check, and then fail
on the first real request with an authentication error. Reporting them as "needs a fresh login" is
the honest outcome, and the migration does exactly that. See
[OAuth overview](10-oauth-overview.md) for how to log in afterwards.

### Why the provider count is not what the plan said

The original plan claimed **35 credentialed providers**. That number cannot be reproduced, and
`plan.md` §19.1 records why: the research listing is truncated, its count includes providers that are
forbidden outright, and several remaining entries cannot be expressed as an HTTP provider at all.

The honest figure is **28 of 45 reachable providers ship**. The gap breaks down as:

| Excluded | Count | Reason |
|---|---|---|
| Web-session providers | 6 | Forbidden. See below. |
| `kiro` | 1 | AWS CodeWhisperer uses an event-stream protocol no `Dialect` can express. |
| `fishaudio` | 1 | Bespoke TTS with a non-standard request and response shape. |
| Non-HTTP agents | 4 | `auggie`, `zcode`, `codex-app-server`, `devin-cli-agentic` are not HTTP services. |
| WebSocket | 1 | `cloudflare-playground`. |
| Not a provider | 1 | `combo` is a routing construct, not an upstream. |
| No registry row | 1 | `uno`. |
| User-supplied node | 1 | `wawawa`. |
| No-session proxies | 2 | `aihorde`, `pollinations` require a browser session. |

An operator migrating needs to know what did not come across. **A silent omission is the worst
possible outcome**, so the migration prints a table of everything it skipped with the specific
reason for each.

### Web-session providers are refused, not skipped

`claude-web`, `deepseek-web`, `gemini-web`, `zai-web`, `duckduckgo-web` and `veoaifree-web` are
**refused**. Manarouter does not support cookie or browser-session authentication for any provider,
at any time, behind any flag. This is a settled design decision
([Security §6](21-security.md#6-cookie-authentication-is-never-supported)), not a gap to be filled.

The distinction between "refused" and "skipped" matters in the output: a refusal is a policy, and the
panel says so. An operator who sees six providers silently missing will reasonably assume a bug.

## The procedure

### 1. Dry run (the default)

```bash
mr migrate omniroute
```

This writes nothing. It prints:

```text
Reference database  ~/.omniroute/storage.sqlite
                    (copy opened read-only; original untouched)
Original sha256     9f2c1e...  (will be re-checked when this finishes)

Providers to import  11
  openai             api-key   2 accounts   catalogue match
  anthropic          api-key   1 account    catalogue match
  google             api-key   1 account    catalogue match
  openrouter         api-key   1 account    catalogue match
  ...

Providers refused    6
  claude-web         web session authentication is never supported
  deepseek-web       web session authentication is never supported
  ...

Providers skipped    3
  kiro               no dialect can express an AWS event-stream provider
  fishaudio          bespoke TTS request and response shape
  some-local-proxy   no catalogue entry and no matching base URL

Accounts to import   7
  openai / personal            (credential present, sk-…4f21)
  openai / work                (credential present, sk-…9ab0)
  anthropic / main             (credential present, sk-ant-…7712)
  ...

OAuth accounts needing a fresh login   2
  github-copilot / work        grant is bound to the other client
  openai-codex / personal      grant is bound to the other client

Combos to import     4
  coding            priority        3 steps
    1. openai-codex/gpt-5-codex
    2. anthropic/claude-sonnet-4-5
    3. openrouter/qwen/qwen3-coder
  ...

Aliases to import    9
  gpt-5-codex -> openai-codex/gpt-5-codex
  sonnet      -> anthropic/claude-sonnet-4-5
  ...

Nothing was written. Re-run with --apply to perform the import.
```

Credentials are shown **masked**, and never in full, even here.

The reference database path defaults to `~/.omniroute/storage.sqlite` and can be overridden with
`--db`. If the file is not where expected, the migration says where it looked rather than failing
with a bare "not found".

### 2. Apply

```bash
mr migrate omniroute --apply
```

The dry run's output is repeated, followed by what was actually written.

## Why a copy

```text
locate the reference database
  -> verify it opens and is an SQLite database
  -> hash it
  -> COPY it to a temporary file
  -> open the COPY read-only
  -> read everything
  -> close, delete the copy
  -> re-hash the original and confirm it is byte-identical
```

The original belongs to a running application. It may be mid-write, it may be in WAL mode with
uncommitted frames, and it may be locked. Opening it read-only is not sufficient protection: a
read-only connection can still trigger recovery, and a lock held by the other application turns a
migration into an outage.

Copying also makes the operation **repeatable**. A migration that failed halfway can simply be run
again, because nothing about the source changed. That property is worth the copy on its own.

The hash is re-checked at the end specifically to catch a migration that modified the source by
accident. If the hashes differ, the migration reports it as an error rather than a warning, because
at that point something is wrong that the operator needs to know about immediately.

## Credential handling

A credential is read out of the reference database, checked against
`mr_core::secret::looks_like_a_credential`, and written to the vault under the account's id:

```text
storage.sqlite  ->  provider_key  ->  Vault::put(account_secret_id(provider, account))
                                              |
config.json     ------------------------------+-->  "account_secret_id": "01924f..."
```

Only the id reaches `config.json`. A credential that fails the format check is **reported and not
imported** — importing a malformed credential produces an account that is configured, enabled, and
guaranteed to fail, which is worse than an account that is visibly absent.

The vault key is generated on first use and stored `0600`. See [Security](21-security.md#3-the-vault).

## Aliases

The reference router exposed the same model under several spellings — a vendor-prefixed id, a
short-prefix id, and a bare id. Client configurations in the wild use all three.

The migration generates one alias per alternative spelling so those clients keep working unchanged:

| Reference spelling | Alias target |
|---|---|
| `openai/gpt-5` | `openai/gpt-5` |
| `gpt-5` (bare) | `openai/gpt-5` |
| `openai:gpt-5` | `openai/gpt-5` |

An alias is only created when the target resolves unambiguously. If two enabled providers offer the
same bare model id, no bare alias is created, and the migration says so — a bare alias that resolved
to whichever provider happened to be compiled last would be worse than no alias.

Alias resolution is a single hop and not transitive ([Combos §how a client
addresses a combo](06-combos.md#how-a-client-addresses-a-combo)), so an alias pointing at another
alias is never generated.

## Idempotency

Running the migration twice must be safe, and it is:

| Situation | Behaviour |
|---|---|
| Provider already imported, unchanged | Left alone. |
| Provider already imported, reference has different settings | **Reference does not win.** The local value is kept and the difference is reported. |
| Account already imported, credential unchanged | Left alone. |
| Account already imported, credential **re-keyed by hand** | **Never overwritten.** Reported as `kept local`. |
| Account already imported, credential still the reference's | Updated. |
| Combo already exists | Left alone if identical; reported as a conflict if the step order differs, and **not** merged. |

The third row is why the migration records a hash of each credential it imported. Without that it
could not tell "the operator changed this deliberately" from "the reference database changed", and
in that ambiguity the operator's edit must win.

The last row is the same reasoning applied to combos. Two different step orders are two different
routing policies, and there is no merge that produces the one the operator meant. Reporting the
conflict and leaving the local combo intact is the only safe outcome.

## After the migration

A checklist, because the migration deliberately does not do these:

- [ ] **Sync models per provider.** `mr models sync <provider>` — discovery is a network operation,
      so it is not part of an import that is meant to be offline and repeatable.
- [ ] **Log in to the OAuth platforms again.** `mr account add` for each. The grants could not be
      migrated.
- [ ] **Review any combo with a missing step.** A step whose provider did not come across is
      reported, and the combo is kept with that step disabled rather than silently shortened.
- [ ] **Check the alias list.** `mr alias list`. Particularly the bare aliases, which are only
      created when unambiguous.
- [ ] **Validate.** `mr config validate` prints every problem at once.

## Rollback

The import writes to exactly two places: `config.json` (plus the compiled snapshot) and the vault.
The reference database is untouched.

To roll back, restore the previous `config.json` and the previous vault, or delete
`~/.config/manarouter/` and start clean. Neither the reference installation nor any provider account
is affected.

**What is not reversible:** if you re-keyed an account after importing it, and then delete the vault,
that new credential is gone — it existed only in the vault. Roll back from a backup rather than
deleting.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `reference database not found at ~/.omniroute/storage.sqlite` | Different install path | Pass `--db /path/to/storage.sqlite`. The message lists the paths that were tried. |
| `database is locked` | The other application holds an exclusive lock | Stop the other application, or copy the file yourself while it is stopped and point `--db` at the copy. |
| `original file changed during migration` | Something wrote to the source, or the copy was made from a moving target | Stop the other application and re-run. |
| `credential failed format check` for one account | The stored value is truncated or was already invalid | Re-key that account by hand after the import. |
| `no catalogue entry` for a provider | A private or new endpoint | Add it by hand with `mr provider add`, or contribute it to `catalog/providers.toml`. |
| A combo imported with a disabled step | Its provider did not come across | Expected. Fix the provider, then re-enable the step. |
| Nothing at all was imported | `--apply` was not passed | The dry run is the default. Read its output, then re-run with `--apply`. |

## See also

- [Security](21-security.md) — the vault, and why cookie authentication is never supported.
- [Configuration](03-configuration.md) — what the importer writes.
- [OAuth overview](10-oauth-overview.md) — logging in after a migration.
- [Troubleshooting](20-troubleshooting.md) — general operational problems.
- [Provider catalogue](providers/catalog.md) — which providers ship, and their dialects.
