# OAuth: adding a platform

This is the worked example. It takes a provider that does not exist and adds it end to end: the trait
implementation, the registry entry, the catalogue entry, account creation, the panel card, and the
tests that stop the next person from breaking it.

Read `docs/10-oauth-overview.md` first. The trait below is the contract fixed there; this document
implements it.

> **Status.** The `OAuthPlatform` trait, the shared machinery and the registry **exist**, in
> `crates/mr-core/src/oauth/` — `mod.rs` (trait, registry), `flow.rs`, `refresh.rs`, `session.rs`,
> `codex.rs`, `copilot.rs`, `antigravity.rs`. They are **not** in the conductor:
> `crates/manarouter-conductor/src/` has no `oauth/` directory and drives the trait through `api.rs`.
> `crates/mr-core/src/store.rs` is fully implemented. Section §3 uses a fictional `halcyon`
> platform to show the shape of a new adapter; a real adapter goes in
> `crates/mr-core/src/oauth/<platform>.rs` and is registered in `PLATFORMS` in
> `crates/mr-core/src/oauth/mod.rs`. The catalogue and test sections are real —
> `catalog/providers.toml` and `crates/mr-core/src/catalog.rs` exist and the fictional entry follows
> their actual shape.

---

## 1. Where the code lives and how it is registered

```
crates/mr-core/src/oauth/
├── mod.rs          the trait, the shared machinery, the registry
├── flow.rs         PKCE, state, the loopback listener, the device driver
├── refresh.rs      single-flight refresh and the 401 repair path
├── session.rs      short-lived session tokens (Copilot)
├── reference.rs    reading the OmniRoute reference database
├── migrate.rs      `mr migrate`
├── codex.rs        openai-codex (loopback, plus a device-code fallback)
├── antigravity.rs  antigravity (both Google tiers)
├── copilot.rs      github-copilot (device code)
└── (halcyon.rs)    not in the tree; the worked example below is fictional
```

The registry is a static array of trait objects, and lookup is a linear scan by id. There are three
shipped platforms, plus the fictional Halcyon below; a hash map would be slower to construct and no
faster to search.

```rust
// crates/mr-core/src/oauth/mod.rs, with the fictional Halcyon added
mod antigravity;
mod codex;
mod copilot;
mod halcyon;

static CODEX: Codex = Codex;
static COPILOT: Copilot = Copilot;
static ANTIGRAVITY: Antigravity = Antigravity::ide();
static HALCYON: Halcyon = Halcyon;

static PLATFORMS: [&'static dyn OAuthPlatform; 4] = [
    &CODEX,
    &COPILOT,
    &ANTIGRAVITY,
    &HALCYON,
];

/// Look a platform up by the id that `AuthKind::OAuth { platform }` and
/// `oauth_platform` in the catalogue both name.
pub fn platform(id: &str) -> Option<&'static dyn OAuthPlatform> {
    platforms().iter().copied().find(|p| p.id() == id)
}
```

Registration is one line in `PLATFORMS` and one `mod` declaration. That is the whole cost of a new
platform in this file, which is the point of the design: nothing in the relay, the panel or the
dialect translators changes.

---

## 2. The trait, item by item

```rust
#[async_trait]
pub trait OAuthPlatform: Send + Sync {
    /// Stable machine id, matching `oauth_platform` in `catalog/providers.toml` and the
    /// `platform` field of `AuthKind::OAuth` in the account's configuration.
    fn id(&self) -> &'static str;
    /// Short human label and one-line description, shown next to it in the panel.
    fn label(&self) -> &'static str;
    fn description(&self) -> &'static str;

    /// The platform's **default** flow: `true` binds a loopback listener and opens the browser,
    /// `false` means the device-code flow.
    fn begins_with_loopback(&self) -> bool;
    /// Whether the platform can *also* be logged in with a device code. Defaults to
    /// `!begins_with_loopback()`; Codex overrides it to `true` while still defaulting to loopback.
    fn supports_device_flow(&self) -> bool { !self.begins_with_loopback() }

    /// The host/port/path a browser is sent back to; device platforms return an inert value.
    /// The host and path strings are sent to the provider verbatim and must not be normalised.
    fn redirect_ports(&self) -> RedirectPorts;
    /// Whether the flow uses PKCE (S256).
    fn requires_pkce(&self) -> bool;
    /// Whether the bound port must be checked against `redirect_ports` before the redirect URI
    /// is published.
    fn validates_redirect_port(&self) -> bool;

    /// Build the authorize URL for a login whose listener is already bound to `port`: scopes,
    /// PKCE challenge, state and any platform-specific extra parameters. The redirect URI is
    /// derived from `port`, never from a constant.
    fn authorization_url(&self, state: &str, pkce: Option<&Pkce>, port: u16) -> Result<String>;
    /// Redeem the authorization code the loopback listener received, with this platform's
    /// encoding (often not the same as `refresh`'s).
    async fn exchange_code(&self, code: &str, pkce: Option<&Pkce>, port: u16) -> Result<TokenSet>;

    /// Exchange a refresh token for a fresh token set. Single-use refresh tokens mean the caller
    /// (`refresh::Refresher`) owns serialisation and persistence; implementations must not persist.
    async fn refresh(&self, refresh_token: &str) -> Result<TokenSet>;
    /// Refresh with the prior provider context; defaults to `refresh`.
    async fn refresh_with_context(&self, refresh_token: &str, previous: &TokenSet) -> Result<TokenSet>;

    /// Resolve a token set to the stable account identity used as the multi-account key.
    async fn account_identity(&self, tokens: &TokenSet) -> Result<AccountIdentity>;
    /// Fetch provider-owned post-login metadata (for example Antigravity's Cloud Code project)
    /// before credentials are persisted. Default: no-op.
    async fn complete_login(&self, tokens: &mut TokenSet) -> Result<()>;
    /// Test seam for the same step; defaults to calling `complete_login`.
    async fn complete_login_with(&self, tokens: &mut TokenSet, http: &dyn flow::OAuthHttp) -> Result<()>;

    /// Credential-derived request headers: the bearer, plus any provider-specific identity
    /// headers. Returning an owned list keeps the fingerprint testable.
    fn apply_credential(&self, tokens: &TokenSet) -> Vec<(HeaderName, HeaderValue)>;
    /// The exact static name/value pairs this client's upstream expects, for the golden-header
    /// test against the catalogue.
    fn fingerprint_headers(&self) -> &'static [(&'static str, &'static str)];
    /// How long before `expires_at` a proactive refresh runs.
    fn refresh_lead(&self) -> Duration;

    /// Begin the device-code flow: the `user_code` to display and the `device_code` to poll.
    /// Default: `Err` — a loopback-only platform does not implement it.
    async fn device_start(&self) -> Result<DeviceAuthorization>;
    /// One poll of the device-code endpoint, taking the whole `DeviceAuthorization` because
    /// providers differ in what a poll must present (GitHub: device code only; Codex: device id
    /// *and* user code). Default: `Err`.
    async fn device_poll(&self, authorization: &DeviceAuthorization) -> Result<DevicePoll>;
}
```

Five items have defaults so that a device-flow platform does not have to write a loopback listener
and a loopback-only platform does not have to write a device pair: `supports_device_flow`,
`refresh_with_context`, `complete_login`/`complete_login_with`, `device_start` and `device_poll`.
Everything else is required: a platform that cannot say how to build an authorize URL or how to turn
a credential into headers is not a platform.

`apply_credential` is deliberately the last thing the shared machinery calls before a request is
built. It is also the item most worth testing, because its failure mode is a `400` from an upstream
that reads like a model or entitlement problem.

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
// crates/mr-core/src/oauth/halcyon.rs
//!
//! FICTIONAL. This provider does not exist; it is the worked example from
//! `docs/14-oauth-adding-a-platform.md`. Do not ship it enabled.
//! The method names and signatures match the real `OAuthPlatform` trait; the helper calls are
//! abbreviated for readability.

use http::header::{AUTHORIZATION, HeaderName, HeaderValue};

use super::{AccountIdentity, OAuthPlatform, Pkce, RedirectPorts, Result, TokenSet};

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

    fn label(&self) -> &'static str { "Halcyon (fictional worked example)" }

    fn description(&self) -> &'static str { "FICTIONAL. The worked example from docs/14." }

    fn begins_with_loopback(&self) -> bool { true }

    fn redirect_ports(&self) -> RedirectPorts {
        RedirectPorts { host: "127.0.0.1", primary: PORT, fallback: None, path: REDIRECT_PATH }
    }

    fn requires_pkce(&self) -> bool { true }

    fn validates_redirect_port(&self) -> bool { true }

    fn authorization_url(&self, state: &str, pkce: Option<&Pkce>, port: u16) -> Result<String> {
        // The redirect URI is derived from the bound `port`, never from a constant, so the
        // shared machinery cannot publish a URI the listener is not actually on.
        let redirect_uri = format!("http://127.0.0.1:{port}{REDIRECT_PATH}");
        let mut url = url::Url::parse(AUTHORIZE).expect("constant URL");
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", CLIENT_ID)
            .append_pair("redirect_uri", &redirect_uri)
            // The challenge is the S256 of the verifier, base64url, no padding.
            // Sending the verifier itself here is the classic PKCE mistake.
            .append_pair("code_challenge", pkce.expect("PKCE required").challenge())
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", state)
            .append_pair("scope", &SCOPES.join(" "));
        Ok(url.into())
    }

    async fn exchange_code(&self, code: &str, pkce: Option<&Pkce>, port: u16) -> Result<TokenSet> {
        let verifier = pkce.expect("PKCE required");
        // FORM encoding for the code exchange.
        let redirect_uri = format!("http://127.0.0.1:{port}{REDIRECT_PATH}");
        let token = Http::shared()?.post_form(TOKEN, &[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("redirect_uri", &redirect_uri),
            ("code_verifier", verifier.verifier()),
        ]).await?;
        TokenSet::from_token_response(token, self)
    }

    async fn refresh(&self, refresh_token: &str) -> Result<TokenSet> {
        // JSON encoding for the refresh, on the SAME URL as the form-encoded exchange.
        // This asymmetry is Halcyon's most important property for the example: getting it
        // wrong fails in a way that looks like a revoked grant.
        let body = serde_json::json!({
            "grant_type": "refresh_token",
            "client_id": CLIENT_ID,
            "refresh_token": refresh_token,
        });
        let token = Http::shared()?.post_json(TOKEN, &body).await?;
        TokenSet::from_token_response(token, self)
    }

    fn apply_credential(&self, tokens: &TokenSet) -> Vec<(HeaderName, HeaderValue)> {
        let tenant = tokens.claim(TENANT_CLAIM, TENANT_FIELD);
        vec![
            (AUTHORIZATION, tokens.bearer_header()),
            (HeaderName::from_static("halcyon-tenant"), tenant.parse().expect("validated")),
            // Mandatory, and part of the fingerprint. Exactly this value, nothing more.
            (HeaderName::from_static("originator"), HeaderValue::from_static("halcyon_cli")),
        ]
    }

    async fn account_identity(&self, tokens: &TokenSet) -> Result<AccountIdentity> {
        // Identity comes from the access token's JWT, decoded base64url WITHOUT padding.
        // The account id is the multi-account key, not a display label.
        AccountIdentity::from_claim(tokens, TENANT_CLAIM, TENANT_FIELD)
    }
}
```

The shared machinery lives beside the platform files — `flow.rs` (PKCE, state, the loopback
listener, the device driver), `refresh.rs` (single-flight refresh and the 401 repair), `session.rs`
(Copilot's short-lived session token) and `mod.rs` itself — so that a new platform does not
reimplement the listener, the vault write or the single-flight guard. A platform file should be able
to stay under 150 lines; if it is not, the missing abstraction belongs in `mod.rs`, not in the
platform.

### 3.2 The catalogue entry

The catalogue is seed data compiled into `mr-core`; it is what `mr provider add halcyon` reads to
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
  send. `"${tenant_id}"` means "ask `apply_credential` for the tenant id"; a literal `"tenant_id"` would be sent
  as the header value and rejected.
- **The entry records the fingerprint** even though `apply_credential` also produces it. The catalogue is the
  declarative statement of what the upstream requires, and it is what a test can assert without
  running a login. The two must agree; a test in §5 checks that they do.

For a real platform, `base_url` and `docs_url` would point at the vendor. `.example` and
`.invalid` are reserved TLDs, which is exactly why the example uses them.

### 3.3 Creating an account

Adding the provider and logging in are two steps, because a provider can exist with zero accounts —
that is the normal state after `mr provider add` and before the first login.

1. **Add the provider** from the catalogue: `mr provider add halcyon`. This copies the template into
   `~/.config/manarouter/config.json` as an ordinary provider with `auth = { type = "oauth",
   platform = "halcyon" }` and `accounts = []`.
2. **Log in** through the panel's Connect action, which is the conductor's OAuth API
   (`POST /api/oauth/halcyon/authorize`, then the session poll/complete pair). The conductor looks up
   `platform("halcyon")`, drives the flow from `begins_with_loopback()`/`supports_device_flow()`, and
   on success:
   - extracts the identity with `account_identity()`;
   - writes the credential to the vault at `provider/halcyon/account/<account-id>` using
     `mr_core::secret::account_secret_id` (never into `config.json`, never into the database — see
     `docs/10-oauth-overview.md` §6);
   - appends an account to the provider in `config.json` holding the **reference** to that vault id,
     the identity, and the granted scopes.
3. **Log in again** for a second tenant. A different identity produces a second account with its own
   vault entry and its own single-flight refresh guard. The same identity replaces the existing
   credential in place.

If the operator repeats the login for an identity that already exists, the account count does not
change. That is the behaviour `docs/10-oauth-overview.md` §9 describes, and it is what stops
a re-login from silently doubling an account.

### 3.4 The panel card

The panel gains a card for the new platform without any new UI code, because it renders whatever the
catalogue and the configured providers contain:

| Element | Source |
|---|---|
| Platform name | `label` from the catalogue entry |
| Plan / tier line | the credential's non-secret metadata (for Copilot, `sku`; for Halcyon, the tier claim) |
| Account list | one row per configured account, each showing label, identity and `AccountState` |
| Sign-in action | the panel's Connect button, backed by the conductor's OAuth API |
| ToS notice | shown for every `AuthKind::OAuth` provider, per `docs/21-security.md` §5 |

The panel renders **fingerprints and identities only**, never a token — the same rule as the
credential table. A card that has a signed-in account shows a masked account identity, a state and a
re-login button; it never has a code path that reads the vault value into a response body.

> The panel document is `docs/15-web-panel.md`. The contract the OAuth module exposes to it is the
> one above: the panel needs a platform label, an account list with states, and two actions (log in,
> sign out). Anything more means the panel knows about a specific platform, which is the coupling
> this design removes.

---

## 4. The fingerprint-header problem

Some platforms require an exact first-party client fingerprint. Send one header fewer and the request
is refused; send one header more and the request is refused, or worse, is quietly attributed to a
fallback integrator that has a smaller model allowlist. The failure is upstream and reads like a
model or entitlement problem, so it is expensive to diagnose.

The design answer is to make the fingerprint **data**, so it can be asserted without running a login:

1. The catalogue records the required header names and values, with `${…}` for the ones that come
   from the credential.
2. The platform's `apply_credential` produces the same set.
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
- **Do not send a mandatory header on some paths only.** If `apply_credential` returns the header,
  every request that goes through `apply_credential` gets it; the mistake to avoid is a code path that
  builds headers by hand and skips `apply_credential`.

---

## 5. The tests

Three static tests and one behavioural test, in increasing order of cost:

```rust
#[test]
fn halcyon_is_registered_under_the_id_the_catalogue_names() {
    let p = oauth::platform("halcyon").expect("registered");
    assert_eq!(p.id(), "halcyon");
    assert!(p.begins_with_loopback());
    // The redirect is the string the provider registered, to the byte.
    let r = p.redirect_ports();
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
    let url = Halcyon.authorization_url("state", Some(&pkce), 47820).unwrap();
    assert!(url.contains("code_challenge_method=S256"));
    assert!(!url.contains(pkce.verifier()));
    assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A47820%2Fcallback"));
}
```

The fourth is the behavioural one and it needs a mock upstream that rejects a request without
`Halcyon-Tenant` and `originator` (§8). `tools/mock-upstream` does not do that yet, so until the route
exists this test runs against the fixture server and is what actually proves the fingerprint reaches
the wire; the catalogue test only proves the data is recorded.

---

## 6. The checklist

In order. Skipping a step produces a failure at the next one, which is why the order matters.

1. **Implement the trait** in `crates/mr-core/src/oauth/<platform>.rs`. Only the platform-specific
   parts: URLs, client id, scopes, redirect, encodings, `apply_credential`, `account_identity`.
2. **Register it** in the `PLATFORMS` array and add the `mod` declaration in
   `crates/mr-core/src/oauth/mod.rs`.
3. **Add the catalogue entry** to `catalog/providers.toml`: `auth`, `oauth_platform`, the OAuth URLs
   and constants, and the `[providers.headers]` fingerprint. All three platform-id spellings must
   match.
4. **Add the tests**: registration, catalogue fingerprint, authorize-URL shape, and a mock-upstream
   behavioural test.
5. **Document it.** A new platform gets a document in this set, like `docs/11-oauth-codex.md`, and a
   `## See also` link to and from `docs/10-oauth-overview.md`.
6. **Record it in `docs/LLM_INDEX.md`** if a new file was added, so the LLM-facing index stays a
   complete map of the documentation. The OAuth documents already have entries there (10–14 and
   `omniroute-reference.md`); a new document needs its own line.
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
  for the same reason. Copilot's refresh re-mints from the long-lived GitHub token instead of using the OAuth grant.

---

## 8. Verifying without spending money

A platform implementation can be verified completely offline, and it should be, because the
alternative is a live account and sometimes a real bill.

### The mock upstream

`tools/mock-upstream` is a deterministic, fault-injecting fake **inference** provider, and it exists
in the tree. It is the same tool the latency benchmarks run against
(`docs/16-latency-engineering.md`): it serves the OpenAI- and Anthropic-shaped inference surfaces,
can inject faults and delays, and echoes request headers. It does **not** yet serve the OAuth
protocol surface (device-code, token and refresh endpoints), and it does not enforce a
mandatory-header fingerprint the way the real upstreams do. OAuth tests that need those routes run
against a fixture server built from the recorded responses in `.research/oauth.md`, or against the
mock once the routes are added.

The properties that make it useful here:

- **Deterministic.** A given sequence of requests always produces the same responses, so a failure is
  reproducible rather than a race.
- **Strict about malformed input.** It returns the upstream's own `400` shape for a body that is not
  JSON, which is how wire fidelity is checked rather than only the data. A missing mandatory header
  is **not** rejected today, so a fingerprint test against it would need the route to be built first.
- **Able to inject faults.** It can return faults on a chosen request, which is how the 401 repair
  path and the single-flight guard can be tested without a real account.
- **Able to enforce encoding.** It can be pointed at a platform whose refresh is JSON while the
  exchange is form-encoded, so the trap in §7 becomes a failing test rather than a support ticket.

> `tools/mock-upstream` **is** in the tree (`docs/01-architecture.md` §7 and `docs/LLM_INDEX.md`
> reference it), but its OAuth endpoints are still the gap: the OAuth tests run against a fixture
> server built from the recorded responses in `.research/oauth.md` until those routes exist.

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

- [OAuth overview](10-oauth-overview.md) — the shared machinery the trait sits in front of
- [OpenAI Codex](11-oauth-codex.md) — a real authorization-code platform with the form/JSON split
- [Google Antigravity](12-oauth-antigravity.md) — two client tiers and the published constants
- [GitHub Copilot](13-oauth-copilot.md) — a real device-flow platform and its token re-mint
- [Security and credentials](21-security.md) — §5 the fingerprint and ToS posture, §6 the constants
- [Architecture](01-architecture.md) — where the conductor and the mock upstream sit
- [providers.toml](../catalog/providers.toml) — the catalogue whose entry shape the example follows
- [catalog.rs](../crates/mr-core/src/catalog.rs) — the fingerprint test and the template validator
