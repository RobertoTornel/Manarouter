//! Which fetcher answers for which account, and everything a fetcher is handed.
//!
//! The port: OmniRoute's `getUsageForProvider` is a `switch` over ~45 provider ids in
//! `open-sse/services/usage/` (MIT, © 2026 diegosouzapw). A `switch` cannot be extended from
//! another module, so the port replaces it with [`QuotaRegistry`] and one [`QuotaFetcher`] per
//! provider family.
//!
//! # Lookup order
//!
//! Three tiers, asked in this order, and within a tier in registration order:
//!
//! 1. [`QuotaTier::Provider`] — a fetcher for one exact provider id (`openrouter`, `codex`, …).
//! 2. [`QuotaTier::Dialect`] — a fetcher for a whole wire dialect (`anthropic`, `google`, …).
//! 3. [`QuotaTier::Fallback`] — the generic OpenAI-compatible reader, which reads an endpoint the
//!    operator declared on the account.
//!
//! A look-up for a provider with no dedicated fetcher therefore *stops* at a dialect fetcher if one
//! claims that dialect; only a dialect no fetcher claims reaches the fallback. The first fetcher in
//! the winning tier whose [`QuotaFetcher::supports`] returns `true` wins — a fetcher decides for
//! itself, because "does this account have a quota endpoint declared" is not knowable from a
//! provider id alone.
//!
//! # The Phase C seam
//!
//! A per-provider fetcher becomes live with **one call**:
//!
//! ```ignore
//! use std::sync::Arc;
//! use mr_core::quota::registry::{QuotaRegistry, QuotaTier};
//!
//! let mut registry = QuotaRegistry::with_generic_fallback();
//! registry.register(QuotaTier::Provider, Arc::new(codex::Codex::new()));
//! registry.register(QuotaTier::Provider, Arc::new(claude::Claude::new()));
//! registry.register(QuotaTier::Dialect, Arc::new(openai_compat::OpenAiCompat::new()));
//! ```
//!
//! Nothing else changes: no match arm to add, no constructor to thread through. Phase C's
//! `openai_compat.rs` can replace the built-in placeholder with
//! [`QuotaRegistry::set_fallback`], and the conductor (task E1) builds the registry once and shares
//! it behind an `Arc`.
//!
//! # Secrets
//!
//! [`QuotaFetchContext`] carries the account credential because a fetcher cannot authenticate
//! without it, and [`QuotaCredential`] has a hand-written [`std::fmt::Debug`] that prints a length
//! instead of the value and zeroes its buffer on drop. A `{:?}` of a context — in a log line, an
//! error, a test failure — never contains the credential. Nothing else in this module logs a
//! credential either.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::dialect::Dialect;
use crate::error::{Error, Result};
use crate::ids::AccountId;
use crate::model::Account;
use crate::quota::http::{QuotaHttp, QuotaRequest};
use crate::quota::types::UsageQuota;

/// A credential a fetcher may present upstream.
///
/// The wrapper exists for two reasons: [`QuotaCredential::expose`] is the single, greppable place a
/// secret is read, and the [`std::fmt::Debug`] implementation below cannot leak it.
#[derive(Clone, PartialEq, Eq)]
pub struct QuotaCredential(String);

impl QuotaCredential {
    /// Wrap a credential value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret itself. Never log, debug-print or include the result in an error message.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether the credential is empty (an account with a reference but no stored secret).
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<String> for QuotaCredential {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for QuotaCredential {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl std::fmt::Debug for QuotaCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuotaCredential")
            .field("length", &self.0.len())
            .field("value", &"<redacted>")
            .finish()
    }
}

impl Drop for QuotaCredential {
    fn drop(&mut self) {
        use zeroize::Zeroize as _;
        self.0.zeroize();
    }
}

/// The account facts a fetcher may use to decide whether it applies.
///
/// Deliberately *not* the whole [`Account`]: vault references are dropped, because a fetcher's job
/// is to read a quota, not to resolve credentials — that is done once, by the caller, and handed
/// over as [`QuotaFetchContext::credential`]. `extra` is carried through because upstreams declare
/// their quota endpoint there.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QuotaAccount {
    /// Stable account identifier, when the caller knows it.
    pub id: Option<AccountId>,
    /// Human label, for example an email.
    pub label: Option<String>,
    /// The account's `identity` (an email, a workspace id) — never a secret.
    pub identity: Option<String>,
    /// True when the account authenticates through an OAuth credential rather than an API key.
    pub oauth: bool,
    /// Provider-specific non-secret fields from the account's configuration.
    pub extra: BTreeMap<String, Value>,
}

impl QuotaAccount {
    /// An account view with nothing known about it yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Copy the non-secret parts of a configured [`Account`].
    pub fn from_account(account: &Account) -> Self {
        Self {
            id: Some(account.id.clone()),
            label: account.label.clone(),
            identity: account.identity.clone(),
            oauth: account.oauth_ref.is_some(),
            extra: account.extra.clone(),
        }
    }

    /// Attach one provider-specific field (the tests' and the conductor's convenience).
    pub fn with_extra(mut self, key: impl Into<String>, value: Value) -> Self {
        self.extra.insert(key.into(), value);
        self
    }
}

/// Everything one quota fetch needs.
///
/// Hand-written [`std::fmt::Debug`], because the derived one would print the credential.
#[derive(Clone)]
pub struct QuotaFetchContext {
    /// The provider this account belongs to, as configured (`openrouter`, `command-code`).
    pub provider_id: String,
    /// The wire dialect the provider speaks.
    pub dialect: Dialect,
    /// The account being read.
    pub account: QuotaAccount,
    /// The upstream's base URL, when the caller knows it. A fetcher may derive an endpoint from it
    /// or ignore it in favour of a hard-coded provider URL.
    pub base_url: Option<String>,
    /// The account's credential, when there is one. Required by some upstreams, unused by others.
    pub credential: Option<QuotaCredential>,
    /// The transport. A test injects a fake here and no socket is touched.
    pub http: Arc<dyn QuotaHttp>,
}

impl QuotaFetchContext {
    /// A context for one provider and dialect, with an injected transport.
    pub fn new(provider_id: impl Into<String>, dialect: Dialect, http: Arc<dyn QuotaHttp>) -> Self {
        Self {
            provider_id: provider_id.into(),
            dialect,
            account: QuotaAccount::new(),
            base_url: None,
            credential: None,
            http,
        }
    }

    /// Set the account being read.
    pub fn with_account(mut self, account: QuotaAccount) -> Self {
        self.account = account;
        self
    }

    /// Set the upstream base URL.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = Some(base_url.into());
        self
    }

    /// Set the credential the fetcher should present.
    pub fn with_credential(mut self, credential: impl Into<QuotaCredential>) -> Self {
        self.credential = Some(credential.into());
        self
    }
}

impl std::fmt::Debug for QuotaFetchContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuotaFetchContext")
            .field("provider_id", &self.provider_id)
            .field("dialect", &self.dialect)
            .field("account", &self.account)
            .field("base_url", &self.base_url)
            .field(
                "credential",
                &self.credential.as_ref().map(|_| "<redacted>"),
            )
            .field("http", &self.http)
            .finish()
    }
}

/// One provider family's quota reader.
///
/// Implementations live one per file in Phase C (`codex.rs`, `claude.rs`, `openai_compat.rs`, …).
/// The trait is object-safe (via `async_trait`), so the registry stores `Arc<dyn QuotaFetcher>`.
#[async_trait]
pub trait QuotaFetcher: Send + Sync + std::fmt::Debug {
    /// A stable identifier, used in logs and in the `source` field of the quota it produces.
    fn id(&self) -> &'static str;

    /// Whether this fetcher should serve `account`.
    ///
    /// Usually an exact provider-id match; a dialect fetcher checks `dialect`; the fallback returns
    /// `true` for anything, which is safe because it is only consulted after the other two tiers.
    /// `account` is available so a fetcher can recognise an operator-declared endpoint.
    fn supports(&self, dialect: Dialect, provider_id: &str, account: &QuotaAccount) -> bool;

    /// Read the quota, or fail.
    ///
    /// An unreachable or unrecognised upstream is an `Err`, never a fabricated `Ok(UsageQuota)`:
    /// a made-up `0/0` renders as an exhausted account and an operator would act on it.
    async fn fetch(&self, ctx: &QuotaFetchContext) -> Result<UsageQuota>;
}

/// Which tier a fetcher is registered in — that is, how specific its claim is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum QuotaTier {
    /// Claims one exact provider id.
    Provider,
    /// Claims a whole wire dialect.
    Dialect,
    /// Claims anything left over (the generic OpenAI-compatible reader).
    Fallback,
}

/// Provider id → fetcher, with the tiered lookup described in the module docs.
///
/// [`QuotaRegistry::with_generic_fallback`] (and therefore [`QuotaRegistry::default`]) starts with
/// the built-in [`GenericOpenAiCompat`] in the fallback tier, so a registry is usable before Phase C
/// registers anything. [`QuotaRegistry::empty`] has no fetchers at all, which is what a test that
/// wants to prove a lookup misses should build.
#[derive(Debug)]
pub struct QuotaRegistry {
    provider: Vec<Arc<dyn QuotaFetcher>>,
    dialect: Vec<Arc<dyn QuotaFetcher>>,
    fallback: Vec<Arc<dyn QuotaFetcher>>,
}

impl QuotaRegistry {
    /// A registry with no fetchers: every look-up misses.
    pub fn empty() -> Self {
        Self {
            provider: Vec::new(),
            dialect: Vec::new(),
            fallback: Vec::new(),
        }
    }

    /// A registry holding only the built-in generic fallback, ready for Phase C to extend.
    pub fn with_generic_fallback() -> Self {
        let mut registry = Self::empty();
        registry.register(QuotaTier::Fallback, Arc::new(GenericOpenAiCompat::new()));
        registry
    }

    /// Register one fetcher in one tier — **the single call Phase C's per-provider files need**.
    ///
    /// Registering a second fetcher with the same [`QuotaFetcher::id`] in the same tier is allowed
    /// (a test may want it) but logs a warning: both are asked, in order, so the earlier one wins
    /// for any account it claims.
    pub fn register(&mut self, tier: QuotaTier, fetcher: Arc<dyn QuotaFetcher>) {
        let id = fetcher.id();
        if self.tier(tier).iter().any(|existing| existing.id() == id) {
            tracing::warn!(
                fetcher = id,
                ?tier,
                "a quota fetcher with this id is already registered in this tier; the earlier one wins"
            );
        }
        self.tier_mut(tier).push(fetcher);
    }

    /// [`QuotaRegistry::register`] with [`QuotaTier::Provider`].
    pub fn register_provider(&mut self, fetcher: Arc<dyn QuotaFetcher>) {
        self.register(QuotaTier::Provider, fetcher);
    }

    /// [`QuotaRegistry::register`] with [`QuotaTier::Dialect`].
    pub fn register_dialect(&mut self, fetcher: Arc<dyn QuotaFetcher>) {
        self.register(QuotaTier::Dialect, fetcher);
    }

    /// Replace the whole fallback tier with one fetcher.
    ///
    /// This is how Phase C's fuller `openai_compat.rs` takes over from
    /// [`GenericOpenAiCompat`] without leaving the placeholder in the tier ahead of it.
    pub fn set_fallback(&mut self, fetcher: Arc<dyn QuotaFetcher>) {
        self.fallback = vec![fetcher];
    }

    /// The fetchers registered in one tier, in registration order.
    pub fn tier(&self, tier: QuotaTier) -> &[Arc<dyn QuotaFetcher>] {
        match tier {
            QuotaTier::Provider => &self.provider,
            QuotaTier::Dialect => &self.dialect,
            QuotaTier::Fallback => &self.fallback,
        }
    }

    fn tier_mut(&mut self, tier: QuotaTier) -> &mut Vec<Arc<dyn QuotaFetcher>> {
        match tier {
            QuotaTier::Provider => &mut self.provider,
            QuotaTier::Dialect => &mut self.dialect,
            QuotaTier::Fallback => &mut self.fallback,
        }
    }

    /// The fetcher that should serve this account: exact provider id, then dialect, then fallback.
    pub fn lookup(
        &self,
        dialect: Dialect,
        provider_id: &str,
        account: &QuotaAccount,
    ) -> Option<Arc<dyn QuotaFetcher>> {
        for tier in [QuotaTier::Provider, QuotaTier::Dialect, QuotaTier::Fallback] {
            for fetcher in self.tier(tier) {
                if fetcher.supports(dialect, provider_id, account) {
                    return Some(Arc::clone(fetcher));
                }
            }
        }
        None
    }

    /// Every registered fetcher id, for diagnostics and the panel.
    pub fn ids(&self) -> Vec<&'static str> {
        [QuotaTier::Provider, QuotaTier::Dialect, QuotaTier::Fallback]
            .into_iter()
            .flat_map(|tier| self.tier(tier).iter().map(|fetcher| fetcher.id()))
            .collect()
    }

    /// Look up and fetch in one step, mapping "nothing claims this account" onto
    /// [`Error::NotFound`].
    pub async fn fetch(&self, ctx: &QuotaFetchContext) -> Result<UsageQuota> {
        let Some(fetcher) = self.lookup(ctx.dialect, &ctx.provider_id, &ctx.account) else {
            return Err(Error::NotFound(format!(
                "no quota fetcher is registered for provider {:?} (dialect {:?})",
                ctx.provider_id, ctx.dialect
            )));
        };
        tracing::debug!(
            fetcher = fetcher.id(),
            provider = %ctx.provider_id,
            "fetching provider quota"
        );
        fetcher.fetch(ctx).await
    }
}

impl Default for QuotaRegistry {
    fn default() -> Self {
        Self::with_generic_fallback()
    }
}

/// The registry the control plane uses: every built-in provider fetcher, in one place.
///
/// This is task C6's wiring. It is kept **separate** from [`QuotaRegistry::with_generic_fallback`]
/// (and therefore from `Default`) on purpose: that constructor is documented as the minimal,
/// placeholder-only seam that a test or an early caller can rely on, and folding the twelve Phase C
/// fetchers into it would make every `QuotaRegistry::default()` pull the whole provider list in.
/// A caller that wants the live registry asks for it explicitly:
///
/// ```ignore
/// let registry = Arc::new(mr_core::quota::registry::default_registry());
/// ```
///
/// All provider fetchers go in [`QuotaTier::Provider`], keyed by their exact `id`. No two of them
/// share one, which matters because [`QuotaRegistry::register`] only warns on a duplicate and lets
/// the earlier registration win.
///
/// The generic reader is installed with [`QuotaRegistry::set_fallback`], not registered *after* the
/// placeholder: [`GenericOpenAiCompat`] and [`openai_compat::OpenAiCompat`] both claim the id
/// `openai-compatible`, and `set_fallback` replaces the tier wholesale so the placeholder is gone
/// rather than sitting ahead of the fuller reader.
///
/// Each implemented provider reader is registered once; the operator-declared generic reader
/// remains the final fallback. `qwen-cloud-token-plan` is the quota reader's id and its exact
/// configured provider id, matching OmniRoute's personal Token Plan connection type.
pub fn default_registry() -> QuotaRegistry {
    use crate::quota::antigravity::Antigravity;
    use crate::quota::claude::Claude;
    use crate::quota::codex::Codex;
    use crate::quota::commandcode::CommandCode;
    use crate::quota::cursor::Cursor;
    use crate::quota::deepseek::DeepSeek;
    use crate::quota::github::GitHub;
    use crate::quota::glm::Glm;
    use crate::quota::grok::Grok;
    use crate::quota::minimax::MiniMax;
    use crate::quota::moonshot::Moonshot;
    use crate::quota::openai_compat::OpenAiCompat;
    use crate::quota::openrouter::OpenRouter;
    use crate::quota::qwen::Qwen;

    let mut registry = QuotaRegistry::empty();
    // Provider tier: one exact-provider fetcher per Phase C file that exports one. The order is
    // fixed so `ids()` (panel diagnostics) is stable; lookup does not depend on it.
    registry.register_provider(Arc::new(Antigravity::new()));
    registry.register_provider(Arc::new(Claude::new()));
    registry.register_provider(Arc::new(Codex::new()));
    registry.register_provider(Arc::new(CommandCode::new()));
    registry.register_provider(Arc::new(Cursor::new()));
    registry.register_provider(Arc::new(DeepSeek::new()));
    registry.register_provider(Arc::new(GitHub::new()));
    registry.register_provider(Arc::new(Glm::new()));
    registry.register_provider(Arc::new(Grok::new()));
    registry.register_provider(Arc::new(MiniMax::new()));
    registry.register_provider(Arc::new(Moonshot::new()));
    registry.register_provider(Arc::new(OpenRouter::new()));
    registry.register_provider(Arc::new(Qwen::new()));
    // Fallback tier: the fuller openai_compat reader replaces the built-in placeholder outright.
    registry.set_fallback(Arc::new(OpenAiCompat::new()));
    registry
}

/// The account field that declares where an operator-configured quota lives. Mirrors OmniRoute's
/// `providerSpecificData.quotaEndpoint`.
pub const QUOTA_ENDPOINT_KEY: &str = "quotaEndpoint";

/// The built-in OpenAI-compatible fallback: reads an endpoint the operator declared.
///
/// Deliberately small, and it is the *seam's* placeholder: it accepts `quotaEndpoint` as a URL
/// string or as an object `{ "url", "auth", "headers" }`, `GET`s it with the account credential,
/// and reads the two counts from the conventional key names (top level, or under `data`). Phase C's
/// `openai_compat.rs` ports OmniRoute's fuller version — operator-declared dot-paths for
/// `used`/`total`/`resetAt`, currency and plan labels — and installs itself with
/// [`QuotaRegistry::set_fallback`] or [`QuotaRegistry::register`].
///
/// It never invents a number: if either count is missing from the payload, the fetch fails with
/// [`Error::NotFound`] rather than reporting `0/0`, because `0/0` renders as an exhausted account.
#[derive(Debug, Clone, Copy, Default)]
pub struct GenericOpenAiCompat;

impl GenericOpenAiCompat {
    /// The fetcher id, also stamped onto the `source` field of what it returns.
    pub const ID: &str = "openai-compatible";

    /// A fallback reader with no configuration of its own.
    pub fn new() -> Self {
        Self
    }
}

/// How the fallback authenticates to the declared endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthMode {
    /// `Authorization: Bearer <credential>` — the default.
    Bearer,
    /// `x-api-key: <credential>`.
    ApiKey,
    /// No credential header at all.
    None,
}

/// The parsed `quotaEndpoint` declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EndpointSpec {
    url: String,
    auth: AuthMode,
    headers: Vec<(String, String)>,
}

/// Keys the fallback reads a consumed amount from.
const USED_KEYS: [&str; 4] = ["used", "used_tokens", "usage", "total_used"];
/// Keys the fallback reads a limit from.
const TOTAL_KEYS: [&str; 4] = ["total", "limit", "total_granted", "quota"];
/// Keys the fallback reads a reset instant from. Both spellings, because upstreams send both.
const RESET_KEYS: [&str; 6] = [
    "reset_at",
    "resetAt",
    "resets_at",
    "resetsAt",
    "renew_at",
    "renewAt",
];

impl EndpointSpec {
    /// Parse `quotaEndpoint`, or report that it is malformed. `Ok(None)` means it is not declared.
    ///
    /// Error messages never echo the declaration: a URL can carry a token in its query string.
    fn from_extra(extra: &BTreeMap<String, Value>) -> Result<Option<Self>> {
        let Some(declared) = extra.get(QUOTA_ENDPOINT_KEY) else {
            return Ok(None);
        };
        match declared {
            Value::String(url) => {
                let url = url.trim();
                if url.is_empty() {
                    return Err(Error::Config(format!(
                        "the account's `{QUOTA_ENDPOINT_KEY}` is an empty url"
                    )));
                }
                Ok(Some(Self {
                    url: url.to_owned(),
                    auth: AuthMode::Bearer,
                    headers: Vec::new(),
                }))
            }
            Value::Object(map) => {
                let url = map
                    .get("url")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|url| !url.is_empty())
                    .ok_or_else(|| {
                        Error::Config(format!(
                            "the account's `{QUOTA_ENDPOINT_KEY}` object needs a non-empty `url` string"
                        ))
                    })?;
                let auth = match map.get("auth").and_then(Value::as_str) {
                    None | Some("bearer") => AuthMode::Bearer,
                    Some("x-api-key") => AuthMode::ApiKey,
                    Some("none") => AuthMode::None,
                    Some(_) => {
                        return Err(Error::Config(format!(
                            "the account's `{QUOTA_ENDPOINT_KEY}` has an unknown `auth` (use `bearer`, `x-api-key` or `none`)"
                        )))
                    }
                };
                let headers = match map.get("headers") {
                    None => Vec::new(),
                    Some(Value::Object(headers)) => headers
                        .iter()
                        .map(|(name, value)| {
                            value
                                .as_str()
                                .map(|value| (name.clone(), value.to_owned()))
                        })
                        .collect::<Option<Vec<_>>>()
                        .ok_or_else(|| {
                            Error::Config(format!(
                                "the account's `{QUOTA_ENDPOINT_KEY}` has a `headers` entry that is not a string"
                            ))
                        })?,
                    Some(_) => {
                        return Err(Error::Config(format!(
                            "the account's `{QUOTA_ENDPOINT_KEY}` `headers` must be an object"
                        )))
                    }
                };
                Ok(Some(Self {
                    url: url.to_owned(),
                    auth,
                    headers,
                }))
            }
            _ => Err(Error::Config(format!(
                "the account's `{QUOTA_ENDPOINT_KEY}` must be a url string or an object with a `url`"
            ))),
        }
    }
}

#[async_trait]
impl QuotaFetcher for GenericOpenAiCompat {
    fn id(&self) -> &'static str {
        Self::ID
    }

    /// Always true: the fallback is the last tier, so claiming everything is what makes a provider
    /// with no dedicated fetcher still report honestly (it fails with a clear reason instead of
    /// silently reporting nothing).
    fn supports(&self, _dialect: Dialect, _provider_id: &str, _account: &QuotaAccount) -> bool {
        true
    }

    async fn fetch(&self, ctx: &QuotaFetchContext) -> Result<UsageQuota> {
        let Some(spec) = EndpointSpec::from_extra(&ctx.account.extra)? else {
            return Err(Error::NotFound(format!(
                "provider {:?} has no dedicated quota fetcher and the account declares no `{QUOTA_ENDPOINT_KEY}`",
                ctx.provider_id
            )));
        };

        let credential = ctx
            .credential
            .as_ref()
            .filter(|credential| !credential.is_empty());
        let mut request = QuotaRequest::get(spec.url.clone());
        match spec.auth {
            AuthMode::Bearer => match credential {
                Some(credential) => request = request.bearer(credential.expose()),
                None => {
                    return Err(Error::Config(format!(
                        "the `{QUOTA_ENDPOINT_KEY}` for provider {:?} expects a bearer credential and the account has none",
                        ctx.provider_id
                    )))
                }
            },
            AuthMode::ApiKey => match credential {
                Some(credential) => {
                    request = request.header("x-api-key", credential.expose());
                }
                None => {
                    return Err(Error::Config(format!(
                        "the `{QUOTA_ENDPOINT_KEY}` for provider {:?} expects an API key and the account has none",
                        ctx.provider_id
                    )))
                }
            },
            AuthMode::None => {}
        }
        for (name, value) in &spec.headers {
            request = request.header(name.clone(), value.clone());
        }

        let response = ctx.http.request(request).await?;
        if !response.is_success() {
            return Err(Error::InvalidState(format!(
                "provider {:?} answered {} at its quota endpoint",
                ctx.provider_id, response.status
            )));
        }
        let body = response.json()?;
        // `data` is the conventional envelope; anything else is read from the top level.
        let scope = match body.get("data") {
            Some(data @ Value::Object(_)) => data,
            _ => &body,
        };

        let used = number_at(scope, &USED_KEYS).ok_or_else(|| {
            Error::NotFound(format!(
                "the quota endpoint for provider {:?} reported no amount used (looked for {})",
                ctx.provider_id,
                USED_KEYS.join(", ")
            ))
        })?;
        let total = number_at(scope, &TOTAL_KEYS).ok_or_else(|| {
            Error::NotFound(format!(
                "the quota endpoint for provider {:?} reported no limit (looked for {})",
                ctx.provider_id,
                TOTAL_KEYS.join(", ")
            ))
        })?;
        let reset = RESET_KEYS
            .iter()
            .find_map(|key| scope.get(*key))
            .cloned()
            .unwrap_or(Value::Null);

        Ok(UsageQuota::from_usage(used, total, &reset).with_source(Self::ID))
    }
}

/// The first key present with a finite numeric value (a JSON number or a numeric string).
fn number_at(scope: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| match scope.get(*key)? {
        Value::Number(number) => number.as_f64().filter(|value| value.is_finite()),
        Value::String(text) => text
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota::http::QuotaResponse;
    use bytes::Bytes;
    use std::sync::Mutex;

    /// A fetcher that claims exactly one provider id.
    #[derive(Debug)]
    struct ProviderFetcher {
        id: &'static str,
        provider: &'static str,
    }

    /// A fetcher that claims one dialect.
    #[derive(Debug)]
    struct DialectFetcher {
        id: &'static str,
        dialect: Dialect,
    }

    /// A fetcher that claims everything.
    #[derive(Debug)]
    struct AnyFetcher {
        id: &'static str,
    }

    #[async_trait]
    impl QuotaFetcher for ProviderFetcher {
        fn id(&self) -> &'static str {
            self.id
        }
        fn supports(&self, _dialect: Dialect, provider_id: &str, _account: &QuotaAccount) -> bool {
            provider_id == self.provider
        }
        async fn fetch(&self, _ctx: &QuotaFetchContext) -> Result<UsageQuota> {
            Ok(UsageQuota::from_counts(1.0, 10.0, None).with_source(self.id))
        }
    }

    #[async_trait]
    impl QuotaFetcher for DialectFetcher {
        fn id(&self) -> &'static str {
            self.id
        }
        fn supports(&self, dialect: Dialect, _provider_id: &str, _account: &QuotaAccount) -> bool {
            dialect == self.dialect
        }
        async fn fetch(&self, _ctx: &QuotaFetchContext) -> Result<UsageQuota> {
            Ok(UsageQuota::from_counts(2.0, 10.0, None).with_source(self.id))
        }
    }

    #[async_trait]
    impl QuotaFetcher for AnyFetcher {
        fn id(&self) -> &'static str {
            self.id
        }
        fn supports(&self, _dialect: Dialect, _provider_id: &str, _account: &QuotaAccount) -> bool {
            true
        }
        async fn fetch(&self, _ctx: &QuotaFetchContext) -> Result<UsageQuota> {
            Ok(UsageQuota::from_counts(3.0, 10.0, None).with_source(self.id))
        }
    }

    /// A transport double: replays one reply and records the requests it saw.
    #[derive(Debug)]
    struct FakeHttp {
        status: u16,
        body: &'static str,
        seen: Mutex<Vec<QuotaRequest>>,
    }

    impl FakeHttp {
        fn serving(body: &'static str) -> Self {
            Self {
                status: 200,
                body,
                seen: Mutex::new(Vec::new()),
            }
        }

        fn failing(status: u16) -> Self {
            Self {
                status,
                body: "{}",
                seen: Mutex::new(Vec::new()),
            }
        }

        fn requests(&self) -> Vec<QuotaRequest> {
            self.seen.lock().expect("not poisoned").clone()
        }
    }

    #[async_trait]
    impl QuotaHttp for FakeHttp {
        async fn request(&self, request: QuotaRequest) -> Result<QuotaResponse> {
            self.seen.lock().expect("not poisoned").push(request);
            Ok(QuotaResponse {
                status: self.status,
                headers: vec![("content-type".to_owned(), "application/json".to_owned())],
                body: Bytes::from(self.body),
            })
        }
    }

    fn account_with_endpoint(url: &str) -> QuotaAccount {
        QuotaAccount::new().with_extra(QUOTA_ENDPOINT_KEY, Value::String(url.to_owned()))
    }

    fn http_double(fake: &Arc<FakeHttp>) -> Arc<dyn QuotaHttp> {
        let clone: Arc<FakeHttp> = Arc::clone(fake);
        clone
    }

    #[test]
    fn lookup_prefers_provider_then_dialect_then_fallback() {
        let mut registry = QuotaRegistry::empty();
        registry.register(
            QuotaTier::Dialect,
            Arc::new(DialectFetcher {
                id: "openai-dialect",
                dialect: Dialect::OpenAi,
            }),
        );
        registry.register(QuotaTier::Fallback, Arc::new(AnyFetcher { id: "fallback" }));
        registry.register(
            QuotaTier::Provider,
            Arc::new(ProviderFetcher {
                id: "codex-provider",
                provider: "codex",
            }),
        );

        let account = QuotaAccount::new();
        let found = |dialect, provider: &str| {
            registry
                .lookup(dialect, provider, &account)
                .map(|fetcher| fetcher.id())
                .map(str::to_owned)
        };
        // Exact provider id wins over the dialect fetcher, even though the dialect also claims it.
        assert_eq!(
            found(Dialect::OpenAi, "codex"),
            Some("codex-provider".to_owned())
        );
        // No provider fetcher: the dialect fetcher answers.
        assert_eq!(
            found(Dialect::OpenAi, "unknown"),
            Some("openai-dialect".to_owned())
        );
        // No provider fetcher and no dialect fetcher: the fallback answers.
        assert_eq!(
            found(Dialect::Anthropic, "unknown"),
            Some("fallback".to_owned())
        );

        // Registration order inside a tier decides between two fetchers that both claim a match.
        let mut ordered = QuotaRegistry::empty();
        ordered.register(
            QuotaTier::Dialect,
            Arc::new(DialectFetcher {
                id: "first",
                dialect: Dialect::OpenAi,
            }),
        );
        ordered.register(
            QuotaTier::Dialect,
            Arc::new(DialectFetcher {
                id: "second",
                dialect: Dialect::OpenAi,
            }),
        );
        assert_eq!(
            ordered
                .lookup(Dialect::OpenAi, "anything", &account)
                .expect("dialect match")
                .id(),
            "first"
        );
    }

    #[test]
    fn an_empty_registry_misses_and_the_default_holds_the_fallback() {
        let empty = QuotaRegistry::empty();
        assert!(empty
            .lookup(Dialect::OpenAi, "anything", &QuotaAccount::new())
            .is_none());
        assert!(empty.ids().is_empty());

        let default = QuotaRegistry::default();
        assert_eq!(default.ids(), vec![GenericOpenAiCompat::ID]);
        assert_eq!(
            default
                .lookup(Dialect::OpenAi, "anything", &QuotaAccount::new())
                .expect("fallback claims everything")
                .id(),
            GenericOpenAiCompat::ID
        );
    }

    #[test]
    fn set_fallback_replaces_the_placeholder() {
        let mut registry = QuotaRegistry::with_generic_fallback();
        registry.set_fallback(Arc::new(AnyFetcher { id: "mine" }));
        assert_eq!(registry.ids(), vec!["mine"]);
        assert_eq!(
            registry
                .lookup(Dialect::Google, "x", &QuotaAccount::new())
                .expect("fallback")
                .id(),
            "mine"
        );
    }

    #[test]
    fn account_view_keeps_the_non_secret_fields() {
        let mut account = Account::new("work");
        account.identity = Some("someone@example.test".to_owned());
        account.secret_ref = Some("vault/acc-1".to_owned());
        account
            .extra
            .insert("plan".to_owned(), Value::String("pro".to_owned()));
        let view = QuotaAccount::from_account(&account);
        assert_eq!(view.id.as_ref(), Some(&account.id));
        assert_eq!(view.label.as_deref(), Some("work"));
        assert_eq!(view.identity.as_deref(), Some("someone@example.test"));
        assert!(!view.oauth);
        assert_eq!(view.extra["plan"], Value::String("pro".to_owned()));
        // The vault reference is not part of what a fetcher sees.
        assert!(!format!("{view:?}").contains("vault/acc-1"));
    }

    #[test]
    fn credentials_and_contexts_never_debug_print_the_secret() {
        let credential = QuotaCredential::new("sk-live-supersecret");
        assert_eq!(credential.expose(), "sk-live-supersecret");
        let printed = format!("{credential:?}");
        assert!(
            !printed.contains("sk-live-supersecret"),
            "leaked: {printed}"
        );
        assert!(printed.contains("length"));

        let fake = Arc::new(FakeHttp::serving("{}"));
        let ctx = QuotaFetchContext::new("openrouter", Dialect::OpenAi, http_double(&fake))
            .with_credential("sk-live-supersecret");
        let printed = format!("{ctx:?}");
        assert!(
            !printed.contains("sk-live-supersecret"),
            "leaked: {printed}"
        );
        assert!(printed.contains("openrouter"));
        assert!(printed.contains("redacted"));
    }

    #[tokio::test]
    async fn fetch_reports_a_miss_rather_than_guessing() {
        let fake = Arc::new(FakeHttp::serving("{}"));
        let ctx = QuotaFetchContext::new("nobody", Dialect::OpenAi, http_double(&fake));
        let error = QuotaRegistry::empty()
            .fetch(&ctx)
            .await
            .expect_err("no fetcher");
        assert!(matches!(error, Error::NotFound(_)), "got {error:?}");
        assert!(fake.requests().is_empty(), "no request may be made");
    }

    #[tokio::test]
    async fn the_registry_routes_to_the_winning_fetcher() {
        let fake = Arc::new(FakeHttp::serving("{}"));
        let mut registry = QuotaRegistry::with_generic_fallback();
        registry.register(
            QuotaTier::Dialect,
            Arc::new(DialectFetcher {
                id: "openai-dialect",
                dialect: Dialect::OpenAi,
            }),
        );
        let ctx = QuotaFetchContext::new("unknown", Dialect::OpenAi, http_double(&fake));
        let quota = registry.fetch(&ctx).await.expect("dialect fetcher answers");
        assert_eq!(quota.source.as_deref(), Some("openai-dialect"));
        assert_eq!(quota.used, 2.0);
        // The fallback would have made a request; the dialect fetcher did not.
        assert!(fake.requests().is_empty());
    }

    #[tokio::test]
    async fn the_generic_fallback_reads_a_declared_endpoint() {
        // A derived payload in the conventional `data` envelope; no network, no upstream.
        let fake = Arc::new(FakeHttp::serving(
            r#"{"data":{"used":30,"total":100,"reset_at":1767323045}}"#,
        ));
        let ctx = QuotaFetchContext::new("some-openai-clone", Dialect::OpenAi, http_double(&fake))
            .with_account(account_with_endpoint("https://api.example.test/v1/credits"))
            .with_credential("sk-test");

        let quota = QuotaRegistry::default()
            .fetch(&ctx)
            .await
            .expect("fallback reads the payload");
        assert_eq!(quota.used, 30.0);
        assert_eq!(quota.total, 100.0);
        assert_eq!(quota.remaining, 70.0);
        assert_eq!(quota.reset_at, Some(1_767_323_045_000));
        assert!(quota.fraction_reported());
        assert_eq!(quota.source.as_deref(), Some(GenericOpenAiCompat::ID));

        let requests = fake.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url, "https://api.example.test/v1/credits");
        assert_eq!(requests[0].method, http::Method::GET);
        assert_eq!(
            requests[0].headers,
            vec![("authorization".to_owned(), "Bearer sk-test".to_owned())]
        );
    }

    #[tokio::test]
    async fn the_generic_fallback_honours_the_declared_endpoint_object() {
        let fake = Arc::new(FakeHttp::serving(r#"{"used":"12.5","limit":"50"}"#));
        let mut account = QuotaAccount::new();
        account.extra.insert(
            QUOTA_ENDPOINT_KEY.to_owned(),
            serde_json::json!({
                "url": "https://api.example.test/quota",
                "auth": "x-api-key",
                "headers": { "x-org": "acme" }
            }),
        );
        let ctx = QuotaFetchContext::new("some-openai-clone", Dialect::OpenAi, http_double(&fake))
            .with_account(account)
            .with_credential("key-123");

        let quota = QuotaRegistry::default()
            .fetch(&ctx)
            .await
            .expect("fallback reads numeric strings");
        assert_eq!(quota.used, 12.5);
        assert_eq!(quota.total, 50.0);

        let requests = fake.requests();
        assert_eq!(
            requests[0].headers,
            vec![
                ("x-api-key".to_owned(), "key-123".to_owned()),
                ("x-org".to_owned(), "acme".to_owned()),
            ]
        );

        // `auth: none` sends no credential header at all, and an unauthenticated read needs none.
        let fake = Arc::new(FakeHttp::serving(r#"{"used":1,"total":2}"#));
        let mut account = QuotaAccount::new();
        account.extra.insert(
            QUOTA_ENDPOINT_KEY.to_owned(),
            serde_json::json!({ "url": "http://127.0.0.1:9/quota", "auth": "none" }),
        );
        let ctx = QuotaFetchContext::new("local", Dialect::OpenAi, http_double(&fake))
            .with_account(account);
        QuotaRegistry::default()
            .fetch(&ctx)
            .await
            .expect("no credential required");
        assert!(fake.requests()[0].headers.is_empty());
    }

    #[tokio::test]
    async fn the_generic_fallback_refuses_to_invent_numbers() {
        let http = Arc::new(FakeHttp::serving("{}"));

        // No endpoint declared: a clear miss, not a fabricated 0/0.
        let ctx = QuotaFetchContext::new("some-openai-clone", Dialect::OpenAi, http_double(&http))
            .with_credential("sk-test");
        let error = QuotaRegistry::default()
            .fetch(&ctx)
            .await
            .expect_err("no endpoint");
        assert!(matches!(error, Error::NotFound(_)), "got {error:?}");

        // A declared endpoint that reports only one count is not a quota.
        let half = Arc::new(FakeHttp::serving(r#"{"data":{"used":30}}"#));
        let ctx = QuotaFetchContext::new("some-openai-clone", Dialect::OpenAi, http_double(&half))
            .with_account(account_with_endpoint("https://api.example.test/quota"))
            .with_credential("sk-test");
        let error = QuotaRegistry::default()
            .fetch(&ctx)
            .await
            .expect_err("no limit");
        assert!(matches!(error, Error::NotFound(_)), "got {error:?}");

        // A bearer endpoint with no credential cannot be read successfully; say so instead of
        // sending an unauthenticated request.
        let ctx = QuotaFetchContext::new("some-openai-clone", Dialect::OpenAi, http_double(&http))
            .with_account(account_with_endpoint("https://api.example.test/quota"));
        let error = QuotaRegistry::default()
            .fetch(&ctx)
            .await
            .expect_err("no credential");
        assert!(matches!(error, Error::Config(_)), "got {error:?}");
        assert!(http.requests().is_empty());

        // A malformed declaration is a configuration error, and its text does not echo the value.
        let secret_url = "https://api.example.test/quota?token=sk-live-supersecret";
        let mut account = QuotaAccount::new();
        account
            .extra
            .insert(QUOTA_ENDPOINT_KEY.to_owned(), Value::Number(7.into()));
        let ctx = QuotaFetchContext::new("some-openai-clone", Dialect::OpenAi, http_double(&http))
            .with_account(account);
        let error = QuotaRegistry::default()
            .fetch(&ctx)
            .await
            .expect_err("malformed declaration");
        assert!(matches!(error, Error::Config(_)), "got {error:?}");
        assert!(!format!("{error}").contains("token"));

        // An upstream failure is reported as its status, not as a quota of zeros.
        let failing = Arc::new(FakeHttp::failing(401));
        let ctx =
            QuotaFetchContext::new("some-openai-clone", Dialect::OpenAi, http_double(&failing))
                .with_account(account_with_endpoint(secret_url))
                .with_credential("sk-test");
        let error = QuotaRegistry::default()
            .fetch(&ctx)
            .await
            .expect_err("upstream failure");
        assert!(matches!(error, Error::InvalidState(_)), "got {error:?}");
        assert!(!format!("{error}").contains("sk-live-supersecret"));
    }

    // --- C6: the full, wired registry -------------------------------------------------------

    /// The provider ids [`default_registry`] must contain: one per Phase C file that exports a
    /// fetcher. Written out rather than derived, so adding a fetcher forces this list to be
    /// updated and a failure names the missing or colliding id.
    const EXPECTED_PROVIDER_IDS: [&str; 13] = [
        "antigravity",
        "claude",
        "codex",
        "command-code-go",
        "cursor",
        "deepseek",
        "github",
        "glm",
        "minimax",
        "moonshot",
        "openrouter",
        "qwen-cloud-token-plan",
        "xai-oauth",
    ];

    #[test]
    fn default_registry_registers_every_fetcher_without_id_collisions() {
        let registry = default_registry();
        let providers = registry.tier(QuotaTier::Provider);
        assert_eq!(
            providers.len(),
            EXPECTED_PROVIDER_IDS.len(),
            "provider tier: {:?}",
            registry.ids()
        );

        let mut seen: Vec<&'static str> = Vec::new();
        for fetcher in providers {
            let id = fetcher.id();
            assert!(
                !seen.contains(&id),
                "duplicate provider id {id:?}: the earlier registration would win silently"
            );
            seen.push(id);
        }
        for expected in EXPECTED_PROVIDER_IDS {
            assert!(
                seen.contains(&expected),
                "no Provider-tier fetcher registered for {expected:?}"
            );
        }
    }

    #[test]
    fn default_registry_lookup_prefers_the_provider_then_falls_back() {
        let registry = default_registry();
        let account = QuotaAccount::new();

        // Each fetcher decides via `supports`; these are the exact ids and the aliases they claim.
        for (provider_id, expected) in [
            ("cursor", "cursor"),
            ("qwen-cloud-token-plan", "qwen-cloud-token-plan"),
            ("openrouter", "openrouter"),
            ("codex", "codex"),
            ("chatgpt", "codex"),
            ("github-copilot", "github"),
            ("glm-cn", "glm"),
            ("xao", "xai-oauth"),
            ("command-code", "command-code-go"),
        ] {
            assert_eq!(
                registry
                    .lookup(Dialect::OpenAi, provider_id, &account)
                    .unwrap_or_else(|| panic!("no fetcher for {provider_id:?}"))
                    .id(),
                expected,
                "provider id {provider_id:?}"
            );
        }

        let unknown = registry
            .lookup(Dialect::OpenAi, "definitely-not-a-provider", &account)
            .expect("the fallback claims everything");
        assert_eq!(unknown.id(), crate::quota::openai_compat::OpenAiCompat::ID);
        assert!(
            !format!("{unknown:?}").contains("GenericOpenAiCompat"),
            "the placeholder must not be the fallback: {unknown:?}"
        );
    }

    #[test]
    fn default_registry_fallback_is_the_full_openai_compat() {
        let registry = default_registry();
        let fallback = registry.tier(QuotaTier::Fallback);
        assert_eq!(
            fallback.len(),
            1,
            "one fallback, not the placeholder plus the real one"
        );
        assert_eq!(
            fallback[0].id(),
            crate::quota::openai_compat::OpenAiCompat::ID
        );

        // `GenericOpenAiCompat` and `OpenAiCompat` share an id, so the id alone cannot tell them
        // apart; the derived `Debug` name can.
        let debug = format!("{:?}", fallback[0]);
        assert!(debug.contains("OpenAiCompat"), "got {debug}");
        assert!(
            !debug.contains("GenericOpenAiCompat"),
            "the placeholder is still the winning fallback: {debug}"
        );
    }
}
