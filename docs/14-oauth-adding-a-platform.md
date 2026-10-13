# OAuth: adding a platform

This is the worked example. It takes a provider that does not exist and adds it end to end: the trait
implementation, the registry entry, the catalogue entry, account creation, the panel card, and the
tests that stop the next person from breaking it.

Read `docs/10-oauth-overview.md` first. The trait below is the contract fixed there; this document
implements it.

> **Status.** The `OAuthPlatform` trait and the registry described here are the conductor's designed
> interface. The module is not in the tree at the time of writing (`crates/manarouter-conductor/src/`
> contains `auth.rs`, `lib.rs` and `main.rs`; `crates/mr-core/src/store.rs` is a stub). The path
> names, signatures and CLIs are the contract the implementation is written against, and the
> catalogue and test sections below are real — `catalog/providers.toml` and
> `crates/mr-core/src/catalog.rs` exist and the fictional entry follows their actual shape.

---

## 1. Where the code lives and how it is registered

```
crates/manarouter-conductor/src/oauth/
├── mod.rs          the trait, the shared machinery, the registry
├── codex.rs        openai-codex
├── antigravity.rs  antigravity (both Google tiers)
├── copilot.rs      github-copilot
└── halcyon.rs      the worked example below
```

The registry is a static slice of trait objects, and lookup is a linear scan by id. There are three
platforms; a hash map would be slower to construct and no faster to search.

```rust
// crates/manarouter-conductor/src/oauth/mod.rs
mod antigravity;
mod codex;
mod copilot;
mod halcyon;

static REGISTRY: &[&dyn OAuthPlatform] = &[
    &codex::Codex,
    &copilot::Copilot,
    &antigravity::Antigravity,
    &halcyon::Halcyon,
];

/// Look a platform up by the id that `AuthKind::OAuth { platform }` and
/// `oauth_platform` in the catalogue both name.
pub fn platform(id: &str) -> Option<&'static dyn OAuthPlatform> {
    REGISTRY.iter().copied().find(|p| p.id() == id)
}
```

Registration is one line in `REGISTRY` and one `mod` declaration. That is the whole cost of a new
platform in this file, which is the point of the design: nothing in the relay, the panel or the
dialect translators changes.

---

## 2. The trait, item by item

```rust
pub trait OAuthPlatform: Send + Sync {
    /// The stable id. Must equal `oauth_platform` in the catalogue entry and the
    /// `platform` field of `AuthKind::OAuth` in the account's configuration.
    fn id(&self) -> &'static str;

    /// Which half of the machinery to drive. `AuthorizationCode` needs a loopback
    /// listener; `DeviceCode` needs a user code shown to the operator.
    fn flow(&self) -> OAuthFlow;

    /// The loopback listener for an authorization-code flow, or `None` for a device flow.
    /// The host and path strings are sent to the provider verbatim and must not be normalised.
    fn redirect(&self) -> Option<RedirectSpec>;

    /// Build the authorize URL: PKCE challenge, state, scopes, and any platform-specific
    /// extra parameters. Returns the string to hand to the browser.
    fn authorize_url(&self, req: &AuthorizeRequest<'_>) -> Result<String>;

    /// Begin a device flow: return the `user_code` to display and the `device_code` to poll.
    /// Default: `Err(Unsupported)`.
    fn device_start(&self, http: &Http) -> Result<DeviceStart> { Err(Error::OAuthUnsupported) }

    /// Poll a device flow once and classify the result. Default: `Err(Unsupported)`.
    fn device_poll(&self, http: &Http, device_code: &str) -> Result<PollOutcome> {
        Err(Error::OAuthUnsupported)
    }

    /// Exchange an authorization code for a credential. `verifier` is `Some` only for a
    /// platform that used PKCE. This call's encoding is the platform's choice and is often
    /// not the same as `refresh`'s.
    fn exchange(
        &self,
        http: &Http,
        code: &str,
        verifier: Option<&str>,
        redirect_uri: &str,
    ) -> Result<Credential>;

    /// Refresh a credential. Same URL as `exchange` for most platforms; the encoding is
    /// frequently different, which is the most common trap in this subsystem.
    fn refresh(&self, http: &Http, cred: &Credential) -> Result<Credential>;

    /// Optional second exchange for a short-lived inference token minted from a long-lived
    /// one (Copilot). Default: `None`, meaning "the credential is the request credential".
    fn mint(&self, http: &Http, cred: &Credential) -> Result<Option<Session>> { Ok(None) }

    /// Turn a credential into the exact headers every inference request must carry. This is
    /// where a platform asserts its fingerprint, and where a missing mandatory header is
    /// introduced if the platform is written carelessly.
    fn apply(
        &self,
        cred: &Credential,
        session: Option<&Session>,
    ) -> Result<Vec<(HeaderName, HeaderValue)>>;

    /// Extract the stable account identity used as the multi-account key. Two logins with
    /// the same identity replace each other; different identities are different accounts.
    fn identity(&self, cred: &Credential) -> Result<AccountIdentity>;
}
```

Three of the items have defaults so that a device-flow platform does not have to write a loopback
listener, and a platform with no second exchange does not have to write a `mint`. Everything else is
required: a platform that cannot say how to build an authorize URL or how to turn a credential into
headers is not a platform.

`apply` is deliberately the last thing the shared machinery calls before a request is built. It is
also the item most worth testing, because its failure mode is a `400` from an upstream that reads
like a model or entitlement problem.

---

## 3. The worked example: Halcyon

**Halcyon is fictional.** Every host below is under the reserved `.example` TLD, the client id
`halcyon-desktop-public` is not a credential, and no implementation of this platform should ever be
shipped enabled. It exists so that the example is self-contained and cannot be mistaken for a real
integration.

Halcyon's invented properties are chosen to exercise the parts of the design that are easy to get
wrong: it uses authorization code + PKCE on a **fixed** loopback port, it wants a tenant header
derived from a JWT claim, it has a mandatory `originator` header, and its refresh is JSON while its
exchange is form-encoded.

### 3.1 The implementation

```rust
// crates/manarouter-conductor/src/oauth/halcyon.rs
//!
//! FICTIONAL. This provider does not exist; it is the worked example from
//! `docs/14-oauth-adding-a-platform.md`. Do not ship it enabled.

use http::header::{AUTHORIZATION, HeaderName, HeaderValue};

use super::{AccountIdentity, AuthorizeRequest, Credential, Http, OAuthFlow, OAuthPlatform,
            PortPolicy, RedirectSpec, Session, TokenResponse};
use crate::error::{Error, Result};

/// The platform id. It appears in three places and they must agree:
/// this constant, `oauth_platform` in the catalogue, and `platform` in the account's
/// `AuthKind::OAuth`.
pub const ID: &str = "halcyon";

const AUTHORIZE: &str = "https://auth.halcyon.example/oauth/authorize";
const TOKEN: &str = "https://auth.halcyon.example/oauth/token";

/// A public native client. Not a secret; PKCE and the registered redirect are the protection.
const CLIENT_ID: &str = "halcyon-desktop-public";

/// `offline_access` is here, not because it looks tidy, but because without it the token
/// endpoint returns no refresh token and the account needs an interactive login every hour.
const SCOPES: &[&str] = &["openid", "offline_access", "inference.read"];

/// 47820 is registered with Halcyon. An ephemeral port would be rejected as a
/// `redirect_uri` mismatch, so the port is fixed and there is no fallback.
const PORT: u16 = 47820;
const REDIRECT_PATH: &str = "/callback";

/// The JWT namespace claim carrying account identity, and the field within it.
const TENANT_CLAIM: &str = "https://api.halcyon.example/tenant";
const TENANT_FIELD: &str = "tenant_id";

pub struct Halcyon;

impl OAuthPlatform for Halcyon {
    fn id(&self) -> &'static str { ID }

    fn flow(&self) -> OAuthFlow { OAuthFlow::AuthorizationCode }

    fn redirect(&self) -> Option<RedirectSpec> {
        Some(RedirectSpec {
            host: "127.0.0.1",
            port: PortPolicy::Fixed(PORT),
            path: REDIRECT_PATH,
        })
    }

    fn authorize_url(&self, req: &AuthorizeRequest<'_>) -> Result<String> {
        let mut url = url::Url::parse(AUTHORIZE).expect("constant URL");
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", CLIENT_ID)
            .append_pair("redirect_uri", &req.redirect_uri)
            // The challenge is the S256 of the verifier, base64url, no padding.
            // Sending the verifier itself here is the classic PKCE mistake.
            .append_pair("code_challenge", &req.pkce.challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &req.state)
            .append_pair("scope", &SCOPES.join(" "));
        Ok(url.into())
    }

    fn exchange(
        &self,
        http: &Http,
        code: &str,
        verifier: Option<&str>,
        redirect_uri: &str,
    ) -> Result<Credential> {
        let verifier = verifier.ok_or(Error::OAuthMissingVerifier)?;
        // FORM encoding for the code exchange.
        let token: TokenResponse = http
            .post_form(TOKEN, &[
                ("grant_type", "authorization_code"),
                ("client_id", CLIENT_ID),
                ("code", code),
                ("redirect_uri", redirect_uri),
                ("code_verifier", verifier),
            ])?
            .json()?;
        Credential::from_token_response(token, self)
    }

    fn refresh(&self, http: &Http, cred: &Credential) -> Result<Credential> {
        // JSON encoding for the refresh, on the SAME URL as the form-encoded exchange.
        // This asymmetry is Halcyon's most important property for the example: getting it
        // wrong fails in a way that looks like a revoked grant.
        let body = serde_json::json!({
            "grant_type": "refresh_token",
            "client_id": CLIENT_ID,
            "refresh_token": cred.refresh_token(),
        });
        let token: TokenResponse = http.post_json(TOKEN, &body)?.json()?;
        Credential::from_token_response(token, self)
    }

    fn apply(
        &self,
        cred: &Credential,
        _session: Option<&Session>,
    ) -> Result<Vec<(HeaderName, HeaderValue)>> {
        let tenant = cred.identity_field(TENANT_CLAIM, TENANT_FIELD)?;
        Ok(vec![
            (AUTHORIZATION, cred.bearer()?),
            (HeaderName::from_static("halcyon-tenant"), tenant.parse()?),
            // Mandatory, and part of the fingerprint. Exactly this value, nothing more.
            (HeaderName::from_static("originator"), HeaderValue::from_static("halcyon_cli")),
        ])
    }

    fn identity(&self, cred: &Credential) -> Result<AccountIdentity> {
        // Identity comes from the access token's JWT, decoded base64url WITHOUT padding.
        // The account id is the multi-account key, not a display label.
        cred.claim(TENANT_CLAIM, TENANT_FIELD)
    }
}
```

The shared helpers — `Http`, `Credential`, `AuthorizeRequest`, `PortPolicy`, `AccountIdentity` — live
in `oauth/mod.rs` so that a new platform does not reimplement PKCE, the listener, the vault write or
the single-flight guard. A platform file should be able to stay under 150 lines; if it is not, the
missing abstraction belongs in `mod.rs`, not in the platform.

### 3.2 The catalogue entry

The catalogue is seed data compiled into `mr-core`; it is what `mr providers add halcyon` reads to
create a correctly-configured provider in one step. The entry goes in `catalog/providers.toml`:

```toml
[[providers]]
id = "halcyon"
label = "Halcyon (fictional worked example)"
dialect = "open-ai"
base_url = "https://api.halcyon.example/v1"
auth = { type = "oauth", platform = "halcyon" }
oauth_platform = "halcyon"
capabilities = ["responses", "tool-calling"]
key_strategy = "priority"
note = "FICTIONAL. The worked example from docs/14-oauth-adding-a-platform.md. Halcyon does not exist; do not enable this provider."
docs_url = "https://example.invalid/halcyon/docs"
oauth_authorize_url = "https://auth.halcyon.example/oauth/authorize"
oauth_token_url = "https://auth.halcyon.example/oauth/token"
oauth_client_id = "halcyon-desktop-public"
oauth_scopes = "openid offline_access inference.read"
oauth_redirect_port = 47820
oauth_redirect_path = "/callback"
oauth_pkce = "S256"

[providers.headers]
"Halcyon-Tenant" = "${tenant_id}"
"originator" = "halcyon_cli"
```

Three things about this entry are load-bearing:

- **`oauth_platform` and `auth.platform` must be the same string**, and both must equal
  `Halcyon::id()`. `ProviderTemplate::validate()` in `crates/mr-core/src/catalog.rs` reports a
  disagreement as a catalogue problem, and the test
  `oauth_platform_and_auth_shape_agree_everywhere` fails the build if any entry gets it wrong.
- **A header value of the form `${name}` is a template the OAuth layer fills**, not a literal to
  send. `"${tenant_id}"` means "ask `apply` for the tenant id"; a literal `"tenant_id"` would be sent
  as the header value and rejected.
- **The entry records the fingerprint** even though `apply` also produces it. The catalogue is the
  declarative statement of what the upstream requires, and it is what a test can assert without
  running a login. The two must agree; a test in §5 checks that they do.

For a real platform, `base_url` and `docs_url` would point at the vendor. `.example` and
`.invalid` are reserved TLDs, which is exactly why the example uses them.

### 3.3 Creating an account

Adding the provider and logging in are two steps, because a provider can exist with zero accounts —
that is the normal state after `mr providers add` and before the first login.

1. **Add the provider** from the catalogue: `mr providers add halcyon`. This copies the template into
   `~/.config/manarouter/config.json` as an ordinary provider with `auth = { type = "oauth",
   platform = "halcyon" }` and `accounts = []`.
2. **Log in**: `mr auth login halcyon`. The conductor looks up `platform("halcyon")`, drives the flow
   from `flow()`, and on success:
   - extracts the identity with `identity()`;
   - writes the credential to the vault at `provider/halcyon/account/<tenant-id>` using
     `mr_core::secret::account_secret_id` (never into `config.json`, never into the database — see
     `docs/10-oauth-overview.md` §6);
   - appends an account to the provider in `config.json` holding the **reference** to that vault id,
     the identity, and the granted scopes.
3. **Log in again** for a second tenant. A different identity produces a second account with its own
   vault entry and its own single-flight refresh guard. The same identity replaces the existing
   credential in place.

If the operator re-runs `mr auth login halcyon` for an identity that already exists, the account count
does not change. That is the behaviour `docs/10-oauth-overview.md` §9 describes, and it is what stops
a re-login from silently doubling an account.

### 3.4 The panel card

The panel gains a card for the new platform without any new UI code, because it renders whatever the
catalogue and the configured providers contain:

| Element | Source |
|---|---|
| Platform name | `label` from the catalogue entry |
| Plan / tier line | the credential's non-secret metadata (for Copilot, `sku`; for Halcyon, the tier claim) |
| Account list | one row per configured account, each showing label, identity and `AccountState` |
| Sign-in action | `mr auth login halcyon` behind the panel's API |
| ToS notice | shown for every `AuthKind::OAuth` provider, per `docs/21-security.md` §5 |

The panel renders **fingerprints and identities only**, never a token — the same rule as the
credential table. A card that has a signed-in account shows a masked account identity, a state and a
re-login button; it never has a code path that reads the vault value into a response body.

> The panel document (`docs/15-web-panel.md`) is not written yet. Until it is, the contract above is
> the one the OAuth module exposes: the panel needs a platform label, an account list with states, and
> two actions (log in, sign out). Anything more means the panel knows about a specific platform,
> which is the coupling this design removes.

---

## 4. The fingerprint-header problem

Some platforms require an exact first-party client fingerprint. Send one header fewer and the request
is refused; send one header more and the request is refused, or worse, is quietly attributed to a
fallback integrator that has a smaller model allowlist. The failure is upstream and reads like a
model or entitlement problem, so it is expensive to diagnose.

The design answer is to make the fingerprint **data**, so it can be asserted without running a login:

1. The catalogue records the required header names and values, with `${…}` for the ones that come
   from the credential.
2. The platform's `apply` produces the same set.
3. A test asserts the catalogue records the names.

The Codex entry is the canonical example — `originator`, `chatgpt-account-id` and (UNCERTAIN)
`OpenAI-Beta` — and `crates/mr-core/src/catalog.rs` already has the test that enforces it:

```rust
#[test]
fn the_three_self_implemented_oauth_platforms_carry_their_fingerprints() {
    let catalog = catalog();

    let codex = catalog.get("openai-codex").expect("openai-codex is shipped");
    assert_eq!(
        codex.headers.get("originator").map(String::as_str),
        Some("codex_cli_rs"),
        "Codex requires originator: codex_cli_rs"
    );
    assert!(
        codex.headers.contains_key("chatgpt-account-id"),
        "the relay injects chatgpt-account-id from the JWT; the requirement belongs in the data"
    );

    let copilot = catalog.get("github-copilot").expect("github-copilot is shipped");
    assert!(
        copilot.headers.contains_key("Editor-Version"),
        "a missing Editor-Version is a hard 400"
    );
    assert!(
        copilot.headers.contains_key("Copilot-Integration-Id"),
        "Copilot-Integration-Id selects the integrator model policy"
    );

    // ...and the antigravity entry.
}
```

Adding Halcyon to the catalogue without adding it to a test leaves the fingerprint unprotected: the
next refactor can drop a header and no test fails until someone logs in against the real upstream.
The example's test is in §5.

Two rules that the fingerprint data does not enforce by itself, and that a reviewer has to check:

- **Do not add headers the vendor's own client does not send.** A router-specific request id, a
  tracing header from an HTTP library, or an account header added "for debugging" is exactly the
  over-complete fingerprint `docs/21-security.md` §5 warns about.
- **Do not send a mandatory header on some paths only.** If `apply` returns the header, every
  request that goes through `apply` gets it; the mistake to avoid is a code path that builds headers
  by hand and skips `apply`.

---

## 5. The tests

Three static tests and one behavioural test, in increasing order of cost:

```rust
#[test]
fn halcyon_is_registered_under_the_id_the_catalogue_names() {
    let p = oauth::platform("halcyon").expect("registered");
    assert_eq!(p.id(), "halcyon");
    assert_eq!(p.flow(), OAuthFlow::AuthorizationCode);
    // The redirect is the string the provider registered, to the byte.
    let r = p.redirect().expect("an authorization-code flow has a redirect");
    assert_eq!((r.host, r.path), ("127.0.0.1", "/callback"));
}

#[test]
fn halcyon_carries_its_fingerprint_in_the_catalogue() {
    let p = mr_core::catalog::catalog().get("halcyon").expect("shipped");
    assert_eq!(p.oauth_platform.as_deref(), Some("halcyon"));
    assert!(p.headers.contains_key("Halcyon-Tenant"), "the tenant header is mandatory");
    assert_eq!(p.headers.get("originator").map(String::as_str), Some("halcyon_cli"));
}

#[test]
fn the_authorize_url_uses_s256_and_the_registered_redirect() {
    // Asserts the challenge is NOT the verifier, which is the mistake this test exists to catch.
    let url = Halcyon.authorize_url(&req).unwrap();
    assert!(url.contains("code_challenge_method=S256"));
    assert!(!url.contains(&req.pkce.verifier));
    assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A47820%2Fcallback"));
}
```

The fourth is the behavioural one and it runs against the mock upstream (§7): the mock refuses any
request without `Halcyon-Tenant` and `originator`, and asserts the router's request carries both. That
test is what actually proves the fingerprint reaches the wire; the catalogue test only proves the data
is recorded.

---

## 6. The checklist

In order. Skipping a step produces a failure at the next one, which is why the order matters.

1. **Implement the trait** in `crates/manarouter-conductor/src/oauth/<platform>.rs`. Only the
   platform-specific parts: URLs, client id, scopes, redirect, encodings, `apply`, `identity`.
2. **Register it** in the `REGISTRY` slice and add the `mod` declaration in `oauth/mod.rs`.
3. **Add the catalogue entry** to `catalog/providers.toml`: `auth`, `oauth_platform`, the OAuth URLs
   and constants, and the `[providers.headers]` fingerprint. All three platform-id spellings must
   match.
4. **Add the tests**: registration, catalogue fingerprint, authorize-URL shape, and a mock-upstream
   behavioural test.
5. **Document it.** A new platform gets a document in this set, like `docs/11-oauth-codex.md`, and a
   `## See also` link to and from `docs/10-oauth-overview.md`.
6. **Record it in `docs/LLM_INDEX.md`** if a new file was added, so the LLM-facing index stays a
   complete map of the documentation. The five OAuth documents already have entries there; a sixth
   document does not.
7. **Run the gate**: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D
   warnings`, `scripts/check-secrets.sh`, `cargo test`. The link check in CI resolves every relative
   Markdown link in `docs/`, so a `## See also` entry pointing at a document that does not exist
   fails the build.

---

## 7. Common mistakes

| Mistake | What happens | Why it is easy to make |
|---|---|---|
| The PKCE challenge is the verifier, or standard base64 | `invalid_grant` at the exchange, after the user has completed the browser step | the verifier "works" until the server compares it |
| The `redirect_uri` differs by a host alias or port | `redirect_uri_mismatch` at authorize; the user sees a provider error | `localhost` and `127.0.0.1` are the same interface but not the same string; the provider compares strings |
| A single-use refresh token is treated as reusable | the second use is classified as token theft; the grant is revoked and only a re-login recovers it | nothing marks the token as consumed, and a retry after a lost response looks harmless |
| `offline_access` (or `access_type=offline`) is omitted | login succeeds, no refresh token is returned, the account dies within the hour | the token response is "valid"; the missing field is not an error |
| A response field is assumed because another platform has it | a deserialiser fails on every response, or a default is silently used | the three platforms' token responses look similar and are not |
| The header fingerprint is over-complete | the upstream refuses, or attributes the caller to a fallback integrator with fewer models | adding a header feels like diligence, and the vendors do not document the reject list |
| The refresh uses the exchange's encoding | refresh fails, the account goes to `auth-failed`, and the operator re-logs in and never learns why | one URL with two encodings is unusual and the difference is invisible in a diff |

The last two are worth restating in the positive, because they are the failures that survive code
review:

- **Send exactly the fingerprint the first-party client sends.** Not one header more. An extra
  header is as identifying as a missing one.
- **Read the encoding off the first-party source, not off the shape of the last platform you
  implemented.** Codex's exchange is form and its refresh is JSON; Halcyon's is the same asymmetry
  for the same reason. Copilot has no refresh at all.

---

## 8. Verifying without spending money

A platform implementation can be verified completely offline, and it should be, because the
alternative is a live account and sometimes a real bill.

### The mock upstream

`tools/mock-upstream` is a deterministic, fault-injecting fake provider. It is the same tool the
latency benchmarks run against (`docs/16-latency-engineering.md`), and for OAuth it serves the whole
protocol surface: a device-code endpoint, a token endpoint, a refresh endpoint, and an inference
endpoint that enforces the fingerprint.

The properties that make it useful here:

- **Deterministic.** A given sequence of requests always produces the same responses, so a failure is
  reproducible rather than a race.
- **Strict about headers.** It returns the same `400` the real upstream returns when a mandatory
  header is missing, which is how the fingerprint test in §5 verifies the wire and not only the data.
- **Able to inject faults.** It can return `invalid_grant` on the first refresh and success on the
  second, which is how the 401 repair path and the single-flight guard are tested without a real
  account.
- **Able to enforce encoding.** It can reject a form-encoded refresh on a platform whose refresh is
  JSON, so the trap in §7 is a failing test rather than a support ticket.

> `tools/mock-upstream` is the planned location (`docs/01-architecture.md` §7 and
> `docs/LLM_INDEX.md` reference it); the directory is not in the tree at the time of writing. Until
> it exists, the same tests can run against a fixture server built from the recorded responses in
> `.research/oauth.md`.

### The opt-in live smoke test

A real login is the only thing that proves a client id, a registered redirect and a fingerprint
against the actual provider. That test is:

- **opt-in**, gated behind an environment variable, and skipped by default;
- **never in CI**, because CI has no account and a live test there would either be skipped forever or
  fail the build for reasons unrelated to the change;
- **manual in its cost**, because it may spend a subscription quota and can trigger a provider's
  abuse detection if it runs in a loop;
- **documented as such** wherever it appears, so nobody mistakes a skipped smoke test for a passing
  one.

`docs/LLM_INDEX.md` records the same separation: CI runs mock tests, and the live smoke test is the
opt-in exception.

---

## See also

- [OAuth overview](docs/10-oauth-overview.md) — the shared machinery the trait sits in front of
- [OpenAI Codex](docs/11-oauth-codex.md) — a real authorization-code platform with the form/JSON split
- [Google Antigravity](docs/12-oauth-antigravity.md) — two client tiers and the published constants
- [GitHub Copilot](docs/13-oauth-copilot.md) — a real device-flow platform and the `mint` exchange
- [Security and credentials](docs/21-security.md) — §5 the fingerprint and ToS posture, §6 the constants
- [Architecture](docs/01-architecture.md) — where the conductor and the mock upstream sit
- [providers.toml](catalog/providers.toml) — the catalogue whose entry shape the example follows
- [catalog.rs](crates/mr-core/src/catalog.rs) — the fingerprint test and the template validator
