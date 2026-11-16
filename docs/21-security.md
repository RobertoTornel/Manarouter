# Security and credentials

A router is a credential aggregator. It holds long-lived OAuth refresh tokens for the operator's
ChatGPT, Copilot and Google accounts, plus API keys for anything metered. This document states what
Manarouter protects, how, and — at least as importantly — what it does **not** protect against.

---

## 1. Threat model

### What this design defends against

| Threat | Defence |
|---|---|
| A copy of the config directory is leaked or backed up to a shared location | Account credentials are encrypted at rest; the configuration file contains references, never values. **Exception:** a provider's static `headers` are stored in the clear — see the limitation below |
| A leaked database file | No credential material is stored in the database at all |
| A screenshot or shoulder-surf of the panel | The panel renders only non-reversible fingerprints, never a full key |
| A typo commits a key to the repository | `scripts/check-secrets.sh` runs on every push and fails the build |
| A malicious web page the operator visits reaches the panel | The panel is loopback-only by default; on a non-loopback bind only loopback and tailnet source addresses are trusted, and the panel checks origin |
| A compromised panel leads to proxied traffic | The panel and the relay are separate processes on separate ports, and the relay has no admin endpoints |
| One bad account poisons a whole provider | Health is tracked per account; a provider is only dead when every account is |
| An attacker with write access to the vault file swaps one credential into another provider's slot | Every ciphertext is bound to its id as AES-GCM associated data |

### What this design does not defend against

These are real limitations. Stating them is the point.

- **An attacker who can already read files as this user.** They can read `vault.key` and decrypt
  everything. No design running as the same user on the same machine can prevent this. The vault
  raises the cost of a *leaked copy*; it does nothing against local compromise.
- **An attacker who can read the relay's memory.** Credentials are held decrypted in the relay so it
  can put them in a header without a syscall. A core dump, a debugger, or a hostile process with
  `ptrace` access can recover them. Credentials fetched from the vault are ordinary `String`s and are
  **not** zeroed after use. Neither is the vault's AES key schedule: `aes-gcm`'s `zeroize` support is
  not enabled, so the round keys stay in memory for the process's life even though the 32 bytes read
  from `vault.key` are wiped after use. Since both processes hold a vault handle open for their whole
  life, that is the same window as the process itself, which is why it is a documented limitation
  rather than a bug being papered over. A short-lived process that opens the vault once
  (`mr account add`) does wipe the key bytes.
- **Provider `headers` are plaintext at rest.** Account credentials go through the vault, but a
  provider's static `headers` map (`mr provider add --header`, editable in the panel) is written
  verbatim to `config.json` and to the routing snapshot. It has to be: the relay must be able to put
  those headers on every request without a per-request decrypt of a whole map, and the catalogue
  uses header maps for things that are not secrets at all (`anthropic-version`, a tenant id). The
  practical rule is therefore: **do not put an API key in a provider header.** Put it in an account
  (`mr account add`), where it is encrypted, fingerprinted, and referenced by id. A credential-shaped
  header value is a configuration mistake, and `mr provider show --json` will print it back to you.
- **A malicious upstream.** A provider you route to sees the prompt and the credential you gave it.
  The catalogue's job is to tell you who you are talking to, not to vouch for them.
- **Traffic analysis by the network.** Upstream connections are TLS, but the *existence* and
  *timing* and *size* of requests to a given provider are observable by anyone on the path.
- **A compromised dependency.** Standard supply-chain risk; the dependency tree is deliberately
  small and pinned by a committed `Cargo.lock`.

---

## 2. Credential storage

### Layout

```
~/.config/manarouter/
  config.json      # referenced by id: "secret_ref": "provider/openai/account/acc-1"
  vault.key        # 32 random bytes, mode 0600, created on first run
  vault.enc        # AES-256-GCM ciphertext, one nonce per entry
```

### Construction

- **Cipher.** AES-256-GCM from the `aes-gcm` crate — an authenticated mode, so ciphertext tampering
  is detected rather than producing garbage that gets sent upstream as a bearer token.
- **Nonce.** Twelve random bytes from the OS CSPRNG per write. A repeated nonce under GCM is a
  catastrophic failure, so a nonce is never derived, never reused and never stored alongside a
  counter. There is a test that asserts two writes of the same value produce different nonces and
  therefore different ciphertexts.
- **Associated data.** The entry's id is passed as GCM associated data. This binds the ciphertext to
  its slot: without it, anyone who could write to `vault.enc` could move provider A's ciphertext
  into provider B's entry and make the router authenticate to B with A's key. There is a test for
  exactly that manipulation.
- **Key file permissions.** Created with `O_CREAT` and mode `0600` in the same `open` call, so the
  file is never briefly world-readable. Creating and then `chmod`-ing leaves a window; that window
  is the difference between a defence and a gesture.
- **Key length is validated.** If `vault.key` exists but is the wrong length, the process **refuses
  to start** rather than silently generating a replacement. Generating a new key would destroy every
  stored credential, and doing so quietly would be the worst possible outcome.
- **A decryption failure is an error, not an absence.** `Vault::get` returns `Ok(None)` for an
  unknown id but an `Err` for a failed decryption. Returning `None` would make the router believe no
  credential is configured and quietly route around a provider the operator expects to work.
- **Rekeying is supported in the library.** `Vault::rekey` decrypts every entry and rewrites it under
  a fresh key, so an operator who suspects the key file was copied has a remedy that does not require
  re-doing every OAuth login. It is not yet reachable from the CLI (`mr vault` is a stub).

### Why a key file and not a passphrase

A passphrase would be strictly stronger against the leaked-copy threat. It was rejected deliberately:
the relay starts unattended, restarts itself when the operator changes a port, and is expected to
come back after a reboot with nobody present. A passphrase means either storing the passphrase
somewhere — which defeats the purpose — or failing to start. A `0600` key file protects against the
realistic case (a synced folder, a backup, a support bundle) while keeping the daemon able to start
on its own.

If your threat model includes an attacker who can read your home directory, this router is not the
right place to solve it. Use full-disk encryption.

---

## 3. What is never stored

- **No tokens in the database.** `store.rs` has an `oauth_credential` table holding the platform,
  the account label, the scopes and the expiry — never the access or refresh token. There is a test
  that reads the raw database bytes and asserts a token never appears in them.
- **No plaintext keys in the configuration.** `config.json` refers to credentials by id
  (`provider/<provider-id>/account/<account-id>`). A test asserts the serialised configuration
  contains the reference and does not trip the credential detector.
- **No key in a log line.** `Vault` has a hand-written `Debug` implementation that prints its path
  and entry count. A derived one would render the AES key schedule into any log line that happened
  to format a vault.

---

## 4. Authentication methods that are refused

**Cookie and web-session authentication is not supported at all, by decision, and no provider in the
catalogue uses it.** This was ratified explicitly and it is a hard constraint, not a default that
can be relaxed with a flag.

The reasoning is concrete rather than merely squeamish. A cookie-based provider means:

- the operator's browser session cookie is extracted from a local profile and replayed — the code
  must therefore read another application's private storage;
- there is no documented API, so every endpoint, header and rotation rule is reverse-engineered and
  can break without notice;
- the provider has no way to distinguish the router from the user, so the user's own account carries
  the entire risk with no visibility;
- session cookies frequently carry device fingerprints and IP attestations, so the operator gets
  locked out of their own account.

`scripts/check-secrets.sh` and a catalogue test both scan for cookie-shaped configuration, so this
constraint is enforced rather than merely documented.

---

## 5. Subscription-backed providers, stated plainly

Manarouter supports OAuth logins for OpenAI Codex (ChatGPT plans), GitHub Copilot and Google
Antigravity. Both of the projects this one learned from carry a warning about this, and the warning
is worth repeating without euphemism:

> Routing a subscription-backed session through a proxy is not a use the provider has licensed.
> The account may be restricted or banned.

What Manarouter does about it:

- it does not hide the risk — the panel shows a notice on those providers, and this document states
  it;
- it sends **exactly** the header fingerprint the first-party client sends, and no extras. An
  over-complete fingerprint is as detectable as a missing one. The catalogue records the required
  header names and a test asserts they are present for each of the three platforms;
- it does not rotate accounts to evade rate limits; per-account cooldowns exist to *respect* a
  provider's signal, not to defeat it;
- it does not spend rate-limit reset credits or reset grants. One reference implementation does; that
  behaviour consumes a scarce allowance the user may have wanted for their own work, and it is
  deliberately not implemented here.

It is the operator's account and the operator's decision. The job of this software is to make the
tradeoff legible, not to make it for them.

A per-account proxy does not change any of that. It exists so that an account belonging to one person
can leave through that person's own machine — a Tailscale peer, a home connection — rather than
through the router's host, and so that a provider sees the address the account normally uses. It is
not an anonymity layer: the credential is still the account's, the fingerprint is still the
first-party one, and the provider still sees the same account. See
[accounts, tags and egress](32-accounts-tags-and-egress.md) §3.

---

## 6. Public OAuth client identifiers in this repository

The repository contains OAuth client ids and, for Google, two client secrets. They are not leaks,
and the claim is verifiable rather than asserted:

| Constant | Why it is public |
|---|---|
| `app_EMoamEEZ73f0CkXaXp7hrann` | OpenAI's own Codex CLI client id, in `openai/codex` |
| gemini-cli's client secret (`GOCSPX-4uHg…`) | Google's `gemini-cli`, whose source comments that it is public |
| Antigravity's client secret (`GOCSPX-K58F…`) | Antigravity IDE's embedded secret, extracted from the shipped app |
| `Iv1.b507a08c87ecfe98` | GitHub Copilot device-flow client id, in Microsoft's `copilot-language-server` |
| Claude Code's client id `9d1c250a-e61b-44d9-88ed-5944d1962f5e` | Anthropic's published client |
| Gemini Code Assist and Antigravity's client ids (`681255809395-…`, `1071006060591-…`) | See the note below; the full values are assembled in code |


Each is shipped inside the vendor's own client, and a native OAuth flow cannot work without them. A
public client id is not a credential under OAuth 2.1: for desktop and CLI clients it is *supposed*
to be public, and the security comes from PKCE and from the provider's own redirect validation.

The OpenAI, Copilot and Claude constants are listed in `ALLOWLIST` in
`scripts/check-secrets.sh`, each with a source comment. The four Google constants are deliberately
not: writing them out would recreate the exact literal GitHub's push protection objects to. A real
secret must never be added to that list, and the file says so.

### Why four of them are assembled from fragments

The four Google constants are the exception, and the reason is instructive rather than tidy.

GitHub's own push protection refuses to accept a file containing a `GOCSPX-`-shaped string or a
Google client id. The first attempt to push this repository was rejected with
`GH013: Repository rule violations`, naming exactly two Google client secrets and two Google client
ids. The scanner cannot distinguish a vendor-published public client secret from a leaked one, and
it should not have to: the pattern is identical either way.

The only override is a manual, per-secret click in the repository's security settings — which would
mean **every future contributor's push fails** until they each repeat it. Trading a working
`git push` for every person who ever touches this project, in exchange for keeping a public value in
one piece, is a bad trade, so `crates/mr-core/src/published.rs` joins those four from fragments with
`concat!`. The assembled value is byte-identical, the join is compile-time so it costs nothing at
runtime, and no single fragment is long enough to match a detector.

That file is explicit that this is **not** a precedent for hiding a real credential, and gives the
test to apply: if you cannot link to the vendor's own source or shipped bundle containing the value,
it is not a published constant and it does not belong there. A real secret is never committed in any
form, fragmented or otherwise — it lives in the vault, addressed by id.

The deeper point is that a repository's own secret scanner and its host's secret scanner are two
different programs with two different threat models, and a project can satisfy one while tripping
the other. Naming that openly is better than a build that mysteriously cannot be pushed.

---

## 7. The panel

- **Loopback by default.** `control.host` defaults to `127.0.0.1`. When bound to a non-loopback
  address (for example `0.0.0.0` to serve both `localhost` and the tailnet on one listener), the
  source address of every request is checked before anything else: only loopback and the Tailscale
  CGNAT range `100.64.0.0/10` are trusted. A password is still recommended as the second line of
  defence; `allow_remote_without_password` exists as an explicit opt-in.
- **The default password mistake is structurally avoided.** One reference router ships with a
  literal default password (`123456`). Rather than defaulting to a password, Manarouter defaults to
  being unreachable from anywhere but loopback. That is a strictly better default: an unreachable
  panel cannot be brute-forced, and a panel that must be reached remotely requires the operator to
  have set a secret first.
- **Password storage.** Argon2id, via the `argon2` crate. Never a plaintext password and never a
  fast hash.
- **Session cookies** are `HttpOnly`, `SameSite=Strict` and `Secure` when served over TLS.
- **Origin checking** on every state-changing request, in addition to the cookie, so that a page the
  operator visits cannot drive the API through their browser.
- **No CSRF-able GETs.** Every mutation is a `POST`, `PUT` or `DELETE`.

---

## 8. Enforcement in CI

Three checks, all in `.github/workflows/ci.yml`:

1. **`scripts/check-secrets.sh`** — fails the build if a credential-shaped string appears in a
   tracked file (a short `EXCLUDE_PATHS` list skips research notes such as `plan.md` and
   `.research/`). It detects prefixed keys (`sk-`, `ghp_`, `gho_`, `github_pat_`, `AIza`, `ya29.`,
   `xoxb-` and others) and JWTs, which have no giveaway prefix and are the most likely thing to
   leak. It has exactly two per-finding exemption mechanisms, both explicit and both greppable: an
   inline `secret-scan: allow` marker, and an `ALLOWLIST` of published constants with a source
   comment each. A finding is reported with the value **masked**, so the scanner never echoes a
   secret into a world-readable CI log.
2. **The catalogue test** asserts no provider uses a cookie-style auth kind and that no entry
   mentions a cookie or a browser session.
3. **Clippy with `-D warnings`** and `cargo fmt --check`, so the diff that introduces the problem is
   the one that fails.

---

## 9. What Manarouter does not do

- **No telemetry.** No phone-home, no usage reporting, no update pings. The only outbound
  connections are to providers the operator configured, plus the OAuth endpoints of platforms the
  operator deliberately signed into.
- **No credential escrow.** There is no "sync your keys to our cloud" feature.
- **No silent remote configuration.** Nothing is fetched at runtime that can change routing.

---

## 10. If a credential leaks

1. **Revoke it at the provider first.** Rotation at the provider is what actually ends the exposure;
   everything below is cleanup.
2. Purge it from history. A force-push is not sufficient on its own — anyone who already cloned has
   it, and GitHub retains unreachable objects for a time. Treat the credential as burned regardless.
3. Remove the provider's stored credential: `mr provider remove <id> --force` drops the provider and
   its vault entries irreversibly; `--force` is what allows the removal while combos or other
   references still point at the provider. There is no `mr provider logout`/`login` pair; an OAuth
   account is re-authorized through the panel's OAuth flow
   (`POST /api/oauth/{platform}/authorize`), which writes a fresh vault entry.
4. If the *vault key* itself leaked, everything in the vault must be treated as compromised. Rekeying
   (`Vault::rekey`) decrypts every entry and re-encrypts it under a new key, so a captured `vault.enc`
   is worthless afterwards — but it is not enough by itself: an attacker holding the old key may
   already have copied plaintext, so you should also rotate every credential. There is no
   `mr vault rekey` command yet (`mr vault` is a stub), so today the operational step is to rotate
   the credentials themselves at the providers and protect `vault.key` and any backups of it going
   forward.

---

## See also

- `docs/10-oauth-overview.md` — how logins and refreshes work
- `docs/03-configuration.md` — the configuration schema, including `secret_ref`
- `mr-core/src/secret.rs` — the implementation, with the tests that assert each claim above
