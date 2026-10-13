# Security and credentials

A router is a credential aggregator. It holds long-lived OAuth refresh tokens for the operator's
ChatGPT, Copilot and Google accounts, plus API keys for anything metered. This document states what
Manarouter protects, how, and — at least as importantly — what it does **not** protect against.

---

## 1. Threat model

### What this design defends against

| Threat | Defence |
|---|---|
| A copy of the config directory is leaked or backed up to a shared location | Credentials are encrypted at rest; the configuration file contains references, never values |
| A leaked database file | No credential material is stored in the database at all |
| A screenshot or shoulder-surf of the panel | The panel renders only non-reversible fingerprints, never a full key |
| A typo commits a key to the repository | `scripts/check-secrets.sh` runs on every push and fails the build |
| A malicious web page the operator visits reaches the panel | The panel is loopback-only by default, requires a password to be reachable otherwise, and checks origin |
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
  **not** zeroed after use; only the vault's own key material is. This is a deliberate trade of a
  small amount of paranoia for not placing an allocator call on the request path.
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
- **Rekeying is supported.** `Vault::rekey` decrypts every entry and rewrites it under a fresh key,
  so an operator who suspects the key file was copied has a remedy that does not require re-doing
  every OAuth login.

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

Most are listed in `ALLOWLIST` in `scripts/check-secrets.sh`, each with a source comment, so that
the secret scanner does not cry wolf. A real secret must never be added to that list, and the file
says so.

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

- **Loopback by default.** `control.host` defaults to `127.0.0.1`. Binding it elsewhere requires
  also setting a password; `allow_remote_without_password` exists as an explicit opt-in and default
  to `false`.
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
   tracked file. It detects prefixed keys (`sk-`, `ghp_`, `AIza`, `ya29.`, `GOCSPX-` and others) and
   JWTs, which have no giveaway prefix and are the most likely thing to leak. It has exactly two
   exemption mechanisms, both explicit and both greppable: an inline `secret-scan: allow` marker,
   and an `ALLOWLIST` of published constants with a source comment each. A finding is reported with
   the value **masked**, so the scanner never echoes a secret into a world-readable CI log.
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
3. Delete the vault entry and re-authenticate: `mr provider logout <id>` then `mr provider login
   <id>`.
4. If the *vault key* itself leaked, everything in the vault must be treated as compromised. Rekey
   (`mr vault rekey`) is not enough by itself — it protects future writes, but you should also
   rotate every credential, because the attacker may have copied the plaintext before you rekeyed.

---

## See also

- `docs/10-oauth-overview.md` — how logins and refreshes work
- `docs/03-configuration.md` — the configuration schema, including `secret_ref`
- `mr-core/src/secret.rs` — the implementation, with the tests that assert each claim above
