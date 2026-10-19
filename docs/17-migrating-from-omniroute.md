# Migrating from OmniRoute

`mr migrate omniroute` imports providers, accounts, combos, aliases and client keys from an existing
OmniRoute installation. It is designed around two rules that override convenience:

1. **It reads a copy, never the original.**
2. **It does nothing until you tell it to.**

Everything else in this document follows from those.

**What OmniRoute is, here:** the author's own personal router, run on their own machine mostly for
testing and for comparison. Manarouter is not a port of it and is not expected to match it. This
document describes an **import path** — how to bring a configuration across once — not a
compatibility target, and nothing in the catalogue is required to match the other router. What the
catalogue derived from that install is marked **UNCERTAIN** in
[the provider catalogue](providers/catalog.md), the MIT attribution for the derived work is in
[the credits](credits.md), and the provider inventory it was built from is kept as
[reference notes](omniroute-reference.md).

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
| OAuth grants | Yes, for supported platforms | Imported by **one refresh**, never by copying an access token. See below. |

### How OAuth grants are migrated

An OAuth grant is normally bound to the client that obtained it, and a token endpoint will reject a
refresh presented by a different client. That is the general rule, and it is why the first version of
this document said grants could not be migrated at all. For these platforms the rule does not bite,
and the reason is checkable rather than a matter of trust:

**The reference and this project ship the same public client identities.** The Codex client id is
published in OpenAI's own open-source Codex CLI (`openai/codex`). The Antigravity pair is the **IDE
(T2) client identity**, which was extracted from the shipped Antigravity IDE bundle rather than
published in an open-source repository — `docs/12-oauth-antigravity.md` §2 marks that extraction
UNCERTAIN, and the open-source gemini-cli pair is the separate (T1) tier. Both projects copied the
same constants, so a refresh token issued to one is accepted from the other.
`crates/mr-core/src/published.rs` holds them, and a digest test pins each of the four Google
constants (`GEMINI_CLI_CLIENT_ID`/`SECRET` and `ANTIGRAVITY_CLIENT_ID`/`SECRET`); the Codex client id
is not digest-pinned, only used. So a future edit that changed a pinned constant would fail the build
rather than silently break migration.

The migration therefore does **one refresh per account** and stores the result:

1. Read the reference's **refresh** token (never its stored access token, which is stale within the
   hour).
2. Hand it to the platform's own `refresh_with_context` — the same code the login flow uses.
3. Store whatever that returns in this installation's vault.

After step 3 the credential belongs to *this* installation and the reference is irrelevant: it can be
stopped, uninstalled or deleted. `crates/mr-core/tests/imported_grants_are_independent.rs` is the
executable form of that claim. By default it reads this installation's own vault/store only (it never
opens the reference database) and proves each imported credential is present, well-formed and usable
without a network call when it is not yet due. Setting `MANAROUTER_FORCE_REFRESH=1` makes it perform
the real rotation through `refresh::Refresher::ensure_fresh`, which is what proves the grants are
independently refreshable; that mode is destructive for single-use Codex refresh tokens, which is why
it is opt-in rather than the default.

#### WARNING: for Codex this is a *move*, not a copy

**Importing a Codex account consumes the reference's copy of it.** Codex issues *single-use* refresh
tokens: the moment the migration presents one, the provider returns a new token and invalidates the
old. After a successful import, OmniRoute's stored refresh token for that account is dead, and
OmniRoute will need its own re-login if you intend to keep using it.

This is a property of the provider, not a choice made here, and it is worth being blunt about because
it is invisible until it bites. Antigravity issues *reusable* refresh tokens, so an Antigravity import
leaves the reference still working — which means a test written against Antigravity alone would pass
while Codex accounts were irreversibly consumed.

Practical consequences:

- **Do not run `--apply` against a live reference you still depend on** unless you accept that its
  Codex accounts will stop working there. Do a dry run first and read the list.
- **A dry run is completely safe.** It performs no refresh and touches no credential.
- **Re-running `--apply` is safe for accounts already imported.** They are recognised as
  `already present` and skipped without a network call, so no second refresh is attempted (and a
  second refresh would fail anyway, for the reason above).
- **The failure is loud, not silent.** A consumed token answers `401 invalid_request_error — "Your
  refresh token has already been used to generate a new access token"`, which is distinguishable
  from an expired one ("sign in again" alone) and from a wrong client (`invalid_client`).
- **A test or diagnostic must not refresh a credential it will not persist.** This was learned the
  hard way during development: an earlier version of the independence test called
  `refresh_with_context` directly to "check" the accounts, discarded the replacements, and
  permanently killed a Codex account — on the development installation the forced run therefore
  reports **9 of 10** credentials rotating. `Refresher::ensure_fresh(.., false)` is the safe read: it
  returns the stored token without a network call when it is not yet due, and persists any rotation
  when it is. The test now uses that path.

#### What each platform carries

| Platform | Durable credential | Also carried |
|---|---|---|
| `openai-codex` | The refresh token | **The `id_token`**, because the relay reads `chatgpt-account-id` from it; without it every request is a hard 400. |
| `antigravity` | The refresh token | The **Cloud Code project id**, which the relay injects into the request envelope. |
| `github-copilot` | The long-lived **GitHub** token, which the reference keeps in its access-token column | Nothing else; the migration moves it to `refresh_token` so a session token can be minted from it. |

#### Why an account can still need a fresh login

Not every grant transfers, and the migration says which and why rather than importing something that
will fail later:

- **Copilot tokens GitHub will not honour.** A GitHub token the vendor refuses to mint a Copilot
  session from cannot become a working account. GitHub answers the mint endpoint with
  `403 programmatic_token_generation, approved clients only` for a token it does not consider to come
  from an approved client — which is what a `ghu_` GitHub **App** token gets, as opposed to the
  `gho_` OAuth App token this build's own device flow produces. The prefix is visible without a
  network call, so the migration checks it up front instead of importing an account that fails every
  request.
- **Antigravity grants with no Cloud Code project.** The request envelope needs a project id, and the
  reference writes the literal `__REQUIRES_GCP_PROJECT__` when it has none. Importing that would
  produce a configured account that cannot serve a request. `--apply` tries to onboard a project
  first and only creates the account if that succeeds; a dry run reports the account as
  `(needs Cloud Code onboarding)`.

#### Model lists come across too

A provider with no model entries resolves nothing, so an account imported without its models would be
live and unreachable. The migration therefore also carries the reference's saved model lists, along
with the labels, context windows and output limits it recorded. Two details are worth knowing:

- **The reference lists synthetic reasoning-effort variants that are not real model ids.** For a
  reasoning model it lists the model once and then repeats it per effort — `gpt-6-luna`, then
  `gpt-6-luna-high`, `gpt-6-luna-low`, and so on. Only the base id exists upstream; the suffixed ones
  are names the reference resolves to *base model + reasoning effort* before it calls the provider.
  Sending them verbatim is answered with `400 The 'gpt-6-luna-high' model is not supported when using
  Codex with a ChatGPT account`. The migration imports the base model and registers the suffixed
  names as **aliases** onto it, in both the bare and `provider/model` spellings, so a client that has
  been calling the suffixed name keeps working.
- **A genuinely suffixed model is kept.** Antigravity really does serve `gemini-3.8-flash-high` and
  `gpt-oss-120b-medium`; these have no base entry in the list, so they are models and are imported as
  such. The distinction is drawn from the listing itself rather than from a hardcoded vocabulary.

See [OAuth overview](10-oauth-overview.md) for how to log in when a fresh login really is needed.

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
([Security §4](21-security.md#4-authentication-methods-that-are-refused)), not a gap to be filled.

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

OAuth accounts to import   3
  openai-codex / personal      credential …4f21c8ab  refreshable
  antigravity / work           credential …9ab03d17  refreshable  project aicode-consumers
  github-copilot / work        credential …7712ee04  refreshable

OAuth accounts needing a fresh login   1
  github-copilot / personal    GitHub will not mint a Copilot session from this token (it is a
                               GitHub App `ghu_` token, and the mint endpoint accepts approved
                               clients only); log in again through the panel

Combos to import     4
  coding            priority        3 steps
    1. openai-codex/gpt-5-codex
    2. anthropic/claude-sonnet-4-5
    3. openrouter/qwen/qwen3-coder
  ...

Aliases to import    9
  gpt-6-luna-high -> gpt-6-luna
  gpt-5-codex     -> openai-codex/gpt-5-codex
  sonnet          -> anthropic/claude-sonnet-4-5
  ...

Nothing was written. Re-run with --apply to perform the import.
```

Credentials are shown **masked**, and never in full, even here.

The reference database path defaults to `~/.omniroute/storage.sqlite` and can be overridden with
`--db`. If the file is not where expected, the migration says where it looked rather than failing
with a bare "not found".

Two further flags narrow a run:

- `--data-dir <DIR>` — where to find the reference's `.env` (which holds `STORAGE_ENCRYPTION_KEY`).
  Defaults to the directory holding the database.
- `--only <provider>` — consider just these reference providers, repeatable. Useful for re-importing
  a single platform without re-reading the whole plan, for example `--only codex --only agy`.

`--json` prints the same report as a machine-readable object.

The reference database is opened as a **copy**: the migration copies the file and its `-wal`/`-shm`
siblings, folds the write-ahead log into the copy, reads it read-only, and deletes it. The original is
never written to, which is why a running OmniRoute is not a problem — if it changes the file mid-read,
the migration reports that rather than failing, because the copy it read is the one it hashed.

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

The vault key is generated on first use and stored `0600`. See [Security §2](21-security.md#2-credential-storage).

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

Alias resolution is a single hop and not transitive ([Combos §8](06-combos.md#8-addressing-a-combo)),
so an alias pointing at another alias is never generated.

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
      so it is not part of an import that is meant to be offline and repeatable. Providers whose
      models came from the reference already have entries, so this is a top-up rather than a
      prerequisite.
- [ ] **Check the accounts reported as needing a fresh login.** `mr account list <provider>`. Each
      one is listed with the reason it could not be imported; re-login only for those, through the
      panel's Connect action (`mr account add` writes a pasted secret, not an OAuth grant, and is
      not the login path for these providers).
- [ ] **Wait for the relay to pick up the change, or restart it.** The relay re-reads an externally
      written `config.json` every ~2 s and recompiles without a restart; the conductor adopts it on
      the next authenticated `GET`. A restart (`systemctl --user restart manarouter-relay.service`)
      remains a valid way to make it immediate.
- [ ] **Review any combo with a missing step.** A step whose provider did not come across is
      reported, and the combo is kept with that step disabled rather than silently shortened.
- [ ] **Check the alias list.** `mr alias list`. Particularly the bare aliases, which are only
      created when unambiguous.
- [ ] **Validate.** `mr config validate` is the intended check, but `mr config` is not implemented
      yet — until it is, the conductor reports a configuration that fails to validate rather than
      adopting it.

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
| `the credential metadata could not be saved` | The state database could not be written after a successful refresh | The imported credential is **kept in the vault** under the key named in the message, because deleting it would permanently destroy an account whose reference token has already been spent. Re-attach it by hand or log in again; re-running `--apply` cannot recover it, because the reference's token for that account is already consumed. |
| A Codex account that worked yesterday now returns `401 ... refresh token has already been used` | Its single-use refresh token was rotated by something that did not persist the replacement — the migration itself, or a diagnostic that refreshed without saving | Nothing in the reference can help: that token is spent too. Log in again through the panel. See [WARNING: for Codex this is a *move*, not a copy](#warning-for-codex-this-is-a-move-not-a-copy). |

## See also

- [Security](21-security.md) — the vault, and why cookie authentication is never supported.
- [Configuration](03-configuration.md) — what the importer writes.
- [OAuth overview](10-oauth-overview.md) — logging in after a migration.
- [Troubleshooting](20-troubleshooting.md) — general operational problems.
- [Provider catalogue](providers/catalog.md) — which providers ship, and their dialects.
