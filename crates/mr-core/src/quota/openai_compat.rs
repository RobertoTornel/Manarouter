//! The generic fallback: a quota endpoint the operator declares on the account.
//!
//! The Rust port of OmniRoute's `open-sse/services/usage/openaiCompatible.ts` (MIT,
//! © 2026 diegosouzapw), and the fuller version of the placeholder B6 shipped in
//! `quota/registry.rs`. Every other fetcher hard-codes one upstream's URL, auth and response shape,
//! which works because those providers are known. An OpenAI-compatible connection can point at
//! anything, so the shape has to come from the configuration: the operator says where the quota
//! lives and how to read it, and this maps the answer onto [`UsageQuota`].
//!
//! The endpoint is declared under `quotaEndpoint` ([`QUOTA_ENDPOINT_KEY`]) in the account's extras:
//!
//! ```json
//! {
//!   "url": "https://api.example.com/v1/credits",
//!   "method": "GET",
//!   "auth": "bearer",
//!   "headers": { "X-Org": "acme" },
//!   "plan": "$.data.plan_name",
//!   "quotas": {
//!     "credits": {
//!       "used": "$.data.used_usd",
//!       "total": "$.data.limit_usd",
//!       "remaining": "$.data.remaining_usd",
//!       "resetAt": "$.data.renews_at",
//!       "currency": "USD",
//!       "displayName": "Credits"
//!     }
//!   }
//! }
//! ```
//!
//! # The rule that matters most
//!
//! **If neither `total` nor `remaining` resolves for a quota entry, no quota is emitted for it.**
//! An absent `total` is derived as `remaining + used`; a missing `used` defaults to
//! `total - remaining`. A `total`-only entry — a limit with no consumption reported in either
//! direction — is emitted with `fraction_reported = false`, the framework's sentinel, because
//! deriving `used = total - 0` there would forge an exhausted account. A payload in which *nothing*
//! resolves is an [`Error::NotFound`], never a `0/0` row: `0/0` renders as exhausted and an
//! operator would act on it.
//!
//! # Deliberate choices
//!
//! * **No window inference.** These are operator-named budgets, not upstream-declared rolling
//!   windows, so nothing here goes through [`crate::quota::window::WindowFamily::from_seconds`].
//!   That is the one fetcher in this module that does not, and it is intentional.
//! * **One account line, one row per name.** The trait returns a single [`UsageQuota`], so the
//!   resolved entries are summed into the headline (`used`, `total`) with the earliest reset, and
//!   each name is attached as a [`QuotaDetail`]. With a single declared quota the headline *is*
//!   that quota. `currency` is parsed and validated but has no field to travel in, so it is
//!   dropped rather than invented; a single entry's `displayName` (or the `plan` path) labels the
//!   line.
//! * **`$`-prefixed dot/bracket paths, not JSONPath.** `$.data.used_usd`, `data.used`, `a.b[0].c`
//!   and `a.b.0.c` all work; anything else resolves to nothing.
//! * **A bare URL still works.** When `quotaEndpoint` is a plain string, or an object with no
//!   `quotas`, the reader falls back to the conventional key names at the top level or under
//!   `data`, which is exactly what B6's [`crate::quota::registry::GenericOpenAiCompat`] did.
//!
//! The credential never appears in an error message or a log line: only the upstream's status and
//! the provider id do.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use http::Method;
use serde_json::Value;

use crate::dialect::Dialect;
use crate::error::{Error, Result};
use crate::quota::http::QuotaRequest;
use crate::quota::registry::{QuotaAccount, QuotaFetchContext, QuotaFetcher, QUOTA_ENDPOINT_KEY};
use crate::quota::types::{QuotaDetail, UsageQuota};

/// `QUOTA_ENDPOINT_TIMEOUT_MS` in the JavaScript source: an operator-declared endpoint can point at
/// anything, including a host that never answers, and one such connection must never hang a panel
/// refresh.
pub const QUOTA_ENDPOINT_TIMEOUT: Duration = Duration::from_secs(15);

/// Keys the conventional (B6-compatible) path reads a consumed amount from.
const USED_KEYS: [&str; 4] = ["used", "used_tokens", "usage", "total_used"];
/// Keys the conventional path reads a limit from.
const TOTAL_KEYS: [&str; 4] = ["total", "limit", "total_granted", "quota"];
/// Keys the conventional path reads a reset instant from. Both spellings, because upstreams send
/// both.
const RESET_KEYS: [&str; 6] = [
    "reset_at",
    "resetAt",
    "resets_at",
    "resetsAt",
    "renew_at",
    "renewAt",
];

/// The generic operator-declared fallback.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenAiCompat;

impl OpenAiCompat {
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

/// One operator-declared quota: the paths that locate its numbers.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct QuotaMapping {
    used: Option<String>,
    total: Option<String>,
    remaining: Option<String>,
    reset_at: Option<String>,
    currency: Option<String>,
    display_name: Option<String>,
}

/// The parsed `quotaEndpoint` declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EndpointSpec {
    url: String,
    method: Method,
    auth: AuthMode,
    headers: Vec<(String, String)>,
    plan: Option<String>,
    /// In the operator's declared order, which `preserve_order` keeps.
    quotas: Vec<(String, QuotaMapping)>,
}

/// The account's declared endpoint, if any.
fn declared_endpoint(account: &QuotaAccount) -> Option<&Value> {
    account.extra.get(QUOTA_ENDPOINT_KEY)
}

/// One optional string field of a declaration, with an alternate spelling accepted.
///
/// A non-string is a configuration error, not a silent absence — an operator who wrote a number
/// where a path belongs wants to hear about it.
fn string_field(map: &serde_json::Map<String, Value>, keys: &[&str]) -> Result<Option<String>> {
    for key in keys {
        match map.get(*key) {
            None | Some(Value::Null) => continue,
            Some(Value::String(text)) => {
                let text = text.trim();
                if text.is_empty() {
                    return Ok(None);
                }
                return Ok(Some(text.to_owned()));
            }
            Some(_) => {
                return Err(Error::Config(format!(
                    "the account's `{QUOTA_ENDPOINT_KEY}` `{key}` must be a string"
                )))
            }
        }
    }
    Ok(None)
}

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
                    method: Method::GET,
                    auth: AuthMode::Bearer,
                    headers: Vec::new(),
                    plan: None,
                    quotas: Vec::new(),
                }))
            }
            Value::Object(map) => Self::from_object(map).map(Some),
            _ => Err(Error::Config(format!(
                "the account's `{QUOTA_ENDPOINT_KEY}` must be a url string or an object with a `url`"
            ))),
        }
    }

    fn from_object(map: &serde_json::Map<String, Value>) -> Result<Self> {
        let url = string_field(map, &["url"])?.ok_or_else(|| {
            Error::Config(format!(
                "the account's `{QUOTA_ENDPOINT_KEY}` object needs a non-empty `url` string"
            ))
        })?;

        let method = match string_field(map, &["method"])? {
            None => Method::GET,
            Some(text) => {
                Method::from_bytes(text.to_ascii_uppercase().as_bytes()).map_err(|_| {
                    Error::Config(format!(
                        "the account's `{QUOTA_ENDPOINT_KEY}` has an invalid `method`"
                    ))
                })?
            }
        };

        let auth = match string_field(map, &["auth"])?.as_deref() {
            None | Some("bearer") => AuthMode::Bearer,
            Some("x-api-key") => AuthMode::ApiKey,
            Some("none") => AuthMode::None,
            Some(_) => {
                return Err(Error::Config(format!(
                    "the account's `{QUOTA_ENDPOINT_KEY}` has an unknown `auth` (use `bearer`, \
                     `x-api-key` or `none`)"
                )))
            }
        };

        let mut headers = Vec::new();
        match map.get("headers") {
            None | Some(Value::Null) => {}
            Some(Value::Object(declared)) => {
                for (name, value) in declared {
                    let Some(value) = value.as_str() else {
                        return Err(Error::Config(format!(
                            "the account's `{QUOTA_ENDPOINT_KEY}` has a `headers` entry that is not \
                             a string"
                        )));
                    };
                    headers.push((name.clone(), value.to_owned()));
                }
            }
            Some(_) => {
                return Err(Error::Config(format!(
                    "the account's `{QUOTA_ENDPOINT_KEY}` `headers` must be an object"
                )))
            }
        }

        let mut quotas = Vec::new();
        match map.get("quotas") {
            None | Some(Value::Null) => {}
            Some(Value::Object(declared)) => {
                for (name, mapping) in declared {
                    let Some(mapping) = mapping.as_object() else {
                        return Err(Error::Config(format!(
                            "the account's `{QUOTA_ENDPOINT_KEY}` `quotas.{name}` must be an object"
                        )));
                    };
                    quotas.push((
                        name.clone(),
                        QuotaMapping {
                            used: string_field(mapping, &["used"])?,
                            total: string_field(mapping, &["total"])?,
                            remaining: string_field(mapping, &["remaining"])?,
                            reset_at: string_field(mapping, &["resetAt", "reset_at"])?,
                            currency: string_field(mapping, &["currency"])?,
                            display_name: string_field(mapping, &["displayName", "display_name"])?,
                        },
                    ));
                }
            }
            Some(_) => {
                return Err(Error::Config(format!(
                    "the account's `{QUOTA_ENDPOINT_KEY}` `quotas` must be an object"
                )))
            }
        }

        Ok(Self {
            url,
            method,
            auth,
            headers,
            plan: string_field(map, &["plan"])?,
            quotas,
        })
    }
}

/// `$.a.b[0].c` / `a.b.0.c` → the value, or `None` if any hop is missing.
///
/// This is the dot/bracket subset OmniRoute parses, **not** JSONPath: no wildcards, filters or
/// slicing. A missing hop, a non-object hop, a non-integer index and a scalar hop all come back
/// `None`, and the caller must treat that as "absent", never as zero.
pub fn resolve_path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    if path.is_empty() {
        return None;
    }
    let cleaned = path
        .strip_prefix("$.")
        .or_else(|| path.strip_prefix('$'))
        .unwrap_or(path);
    let normalised = normalise_brackets(cleaned);

    let mut cursor = root;
    for segment in normalised.split('.').filter(|segment| !segment.is_empty()) {
        cursor = match cursor {
            Value::Array(items) => items.get(segment.parse::<usize>().ok()?)?,
            Value::Object(map) => map.get(segment)?,
            // A scalar or `null` has no members, so the path cannot go on.
            _ => return None,
        };
    }
    Some(cursor)
}

/// `a.b[0].c` and `a.b[12].c` become `a.b.0.c` / `a.b.12.c`, exactly as the source's
/// `replace(/\[(\d+)\]/g, ".$1")` does. A bracket that is not an index is left literal, so it
/// simply fails to resolve rather than being silently dropped.
fn normalise_brackets(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut rest = path;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find(']') {
            Some(close) => {
                let index = &after[..close];
                if !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit()) {
                    out.push('.');
                    out.push_str(index);
                } else {
                    out.push('[');
                    out.push_str(index);
                    out.push(']');
                }
                rest = &after[close + 1..];
            }
            None => {
                out.push('[');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// A JSON number or a numeric string, else `None`. Never coerces `null`/`true` to zero.
fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(value) => value.as_f64().filter(|value| value.is_finite()),
        Value::String(text) => text
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite()),
        _ => None,
    }
}

/// The number a declared path points at, when both the path and the value resolve.
fn resolved_number(body: &Value, path: Option<&str>) -> Option<f64> {
    number(resolve_path(body, path?))
}

/// One declared quota, resolved against the payload.
#[derive(Debug, Clone, PartialEq)]
struct MappedQuota {
    name: String,
    quota: UsageQuota,
    display_name: Option<String>,
    currency: Option<String>,
}

/// Resolve one declared mapping. `None` means the payload says nothing this entry can use, and the
/// caller emits no quota for it — never a `0/0` row.
fn map_quota(name: &str, mapping: &QuotaMapping, body: &Value) -> Option<MappedQuota> {
    let total_value = resolved_number(body, mapping.total.as_deref());
    let remaining_value = resolved_number(body, mapping.remaining.as_deref());
    let used_value = resolved_number(body, mapping.used.as_deref());

    // The rule that matters most: without a denominator there is no quota to report.
    if total_value.is_none() && remaining_value.is_none() {
        return None;
    }

    let (used, total, fraction_reported) = match (total_value, remaining_value, used_value) {
        (Some(total), Some(remaining), used) => (used.unwrap_or(total - remaining), total, true),
        (Some(total), None, Some(used)) => (used, total, true),
        // A limit with no consumption in either direction: report the limit and say the fraction was
        // not reported. `total - 0` here would forge an exhausted account.
        (Some(total), None, None) => (total, total, false),
        (None, Some(remaining), used) => {
            let used = used.unwrap_or(0.0);
            (used, remaining + used, true)
        }
        (None, None, _) => return None,
    };

    let reset = mapping
        .reset_at
        .as_deref()
        .and_then(|path| resolve_path(body, path))
        .cloned()
        .unwrap_or(Value::Null);
    let quota = if fraction_reported {
        UsageQuota::from_usage(used, total, &reset)
    } else {
        UsageQuota::from_usage_unreported(used, total, &reset)
    };

    Some(MappedQuota {
        name: name.to_owned(),
        quota,
        display_name: mapping.display_name.clone(),
        currency: mapping.currency.clone(),
    })
}

/// Fold the resolved entries into the single quota the trait returns.
///
/// The headline counts are the sum of the entries and the earliest reset among them, because there
/// is one account line; each name survives as a [`QuotaDetail`]. With one declared quota the
/// headline is that quota exactly.
fn assemble(
    mut mapped: Vec<MappedQuota>,
    plan: Option<String>,
    source: &'static str,
) -> UsageQuota {
    let any_unreported = mapped.iter().any(|entry| !entry.quota.fraction_reported);
    let used: f64 = mapped.iter().map(|entry| entry.quota.used).sum();
    let total: f64 = mapped.iter().map(|entry| entry.quota.total).sum();
    let reset_at = mapped.iter().filter_map(|entry| entry.quota.reset_at).min();

    let mut quota = if any_unreported {
        UsageQuota::from_counts_unreported(used, total, reset_at)
    } else {
        UsageQuota::from_counts(used, total, reset_at)
    }
    .with_source(source);

    if let Some(plan) = plan {
        quota = quota.with_display_name(plan);
    } else if mapped.len() == 1 {
        if let Some(display_name) = mapped[0].display_name.take() {
            quota = quota.with_display_name(display_name);
        }
    }

    for entry in mapped {
        quota = quota.with_detail(QuotaDetail {
            name: entry.name,
            used: entry.quota.used,
        });
    }
    quota
}

/// The first key present with a finite numeric value (a JSON number or a numeric string).
fn number_at(scope: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| number(scope.get(*key)))
}

/// B6's conventional reading: a bare URL declaration with no `quotas`, whose payload carries the
/// usual key names either at the top level or under `data`.
fn conventional_quota(body: &Value, provider_id: &str) -> Result<UsageQuota> {
    // `data` is the conventional envelope; anything else is read from the top level.
    let scope = match body.get("data") {
        Some(data @ Value::Object(_)) => data,
        _ => body,
    };

    let used = number_at(scope, &USED_KEYS).ok_or_else(|| {
        Error::NotFound(format!(
            "the quota endpoint for provider {provider_id:?} reported no amount used (looked for {})",
            USED_KEYS.join(", ")
        ))
    })?;
    let total = number_at(scope, &TOTAL_KEYS).ok_or_else(|| {
        Error::NotFound(format!(
            "the quota endpoint for provider {provider_id:?} reported no limit (looked for {})",
            TOTAL_KEYS.join(", ")
        ))
    })?;
    let reset = RESET_KEYS
        .iter()
        .find_map(|key| scope.get(*key))
        .cloned()
        .unwrap_or(Value::Null);

    Ok(UsageQuota::from_usage(used, total, &reset).with_source(OpenAiCompat::ID))
}

#[async_trait]
impl QuotaFetcher for OpenAiCompat {
    fn id(&self) -> &'static str {
        Self::ID
    }

    /// Always true: the fallback is the last tier, so claiming everything is what turns "no fetcher
    /// for this provider" into a clear reason instead of silence. The account is still read, so an
    /// operator-declared endpoint is *recognised* here as well as in [`Self::fetch`].
    fn supports(&self, _dialect: Dialect, _provider_id: &str, account: &QuotaAccount) -> bool {
        if declared_endpoint(account).is_some() {
            return true;
        }
        true
    }

    async fn fetch(&self, ctx: &QuotaFetchContext) -> Result<UsageQuota> {
        let Some(spec) = EndpointSpec::from_extra(&ctx.account.extra)? else {
            return Err(Error::NotFound(format!(
                "provider {:?} has no dedicated quota fetcher and the account declares no \
                 `{QUOTA_ENDPOINT_KEY}`",
                ctx.provider_id
            )));
        };

        let credential = ctx
            .credential
            .as_ref()
            .filter(|credential| !credential.is_empty());
        let mut request = QuotaRequest::get(spec.url.clone()).method(spec.method.clone());
        match spec.auth {
            AuthMode::Bearer => match credential {
                Some(credential) => request = request.bearer(credential.expose()),
                None => {
                    return Err(Error::Config(format!(
                        "the `{QUOTA_ENDPOINT_KEY}` for provider {:?} expects a bearer credential \
                         and the account has none",
                        ctx.provider_id
                    )))
                }
            },
            AuthMode::ApiKey => match credential {
                Some(credential) => request = request.header("x-api-key", credential.expose()),
                None => {
                    return Err(Error::Config(format!(
                        "the `{QUOTA_ENDPOINT_KEY}` for provider {:?} expects an API key and the \
                         account has none",
                        ctx.provider_id
                    )))
                }
            },
            AuthMode::None => {}
        }
        request = request.header("accept", "application/json");
        // The operator's headers go on last, so for a repeated name theirs is the one an upstream
        // honouring the last value wins with.
        for (name, value) in &spec.headers {
            request = request.header(name.clone(), value.clone());
        }
        request = request.timeout(QUOTA_ENDPOINT_TIMEOUT);

        let response = ctx.http.request(request).await?;
        if !response.is_success() {
            return Err(Error::InvalidState(format!(
                "provider {:?} answered {} at its quota endpoint",
                ctx.provider_id, response.status
            )));
        }
        let body = response.json()?;

        let plan = spec
            .plan
            .as_deref()
            .and_then(|path| resolve_path(&body, path))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|plan| !plan.is_empty())
            .map(str::to_owned);

        // No `quotas` block: the operator declared only a URL, which is the declaration B6's
        // placeholder understood.
        if spec.quotas.is_empty() {
            let mut quota = conventional_quota(&body, &ctx.provider_id)?;
            if let Some(plan) = plan {
                quota = quota.with_display_name(plan);
            }
            return Ok(quota);
        }

        let mapped: Vec<MappedQuota> = spec
            .quotas
            .iter()
            .filter_map(|(name, mapping)| map_quota(name, mapping, &body))
            .collect();
        // `currency` is parsed and validated above (a non-string is a configuration error), but the
        // normalised `UsageQuota` has no field for it, so it is surfaced here rather than dropped
        // without a trace.
        for entry in &mapped {
            if let Some(currency) = entry.currency.as_deref() {
                tracing::debug!(
                    provider = %ctx.provider_id,
                    quota = %entry.name,
                    currency = %currency,
                    "operator-declared quota currency is not carried by the normalised shape"
                );
            }
        }
        if mapped.is_empty() {
            return Err(Error::NotFound(format!(
                "provider {:?}: none of the {} declared `{QUOTA_ENDPOINT_KEY}` quotas resolved a \
                 `total` or a `remaining`, so no quota is reported (a 0/0 row would read as \
                 exhausted)",
                ctx.provider_id,
                spec.quotas.len()
            )));
        }

        Ok(assemble(mapped, plan, Self::ID))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use bytes::Bytes;
    use serde_json::json;

    use super::*;
    use crate::quota::http::{QuotaHttp, QuotaResponse};

    /// A captured-shaped reply: three separately named numbers under a `data` envelope.
    const MAPPED: &str = r#"{
        "data": {
            "used_usd": 12.5,
            "limit_usd": 50,
            "remaining_usd": 37.5,
            "renews_at": "2026-11-01T00:00:00Z",
            "plan_name": "Pro",
            "nested": [ { "c": 7, "cap": 9 } ]
        }
    }"#;

    const CONVENTIONAL: &str = r#"{"used":"12.5","total":50,"resetAt":"2026-11-01T00:00:00Z"}"#;

    /// A transport double: one canned reply and the requests it saw.
    #[derive(Debug)]
    struct FakeHttp {
        status: u16,
        body: &'static str,
        seen: Mutex<Vec<QuotaRequest>>,
    }

    impl FakeHttp {
        fn serving(body: &'static str) -> Arc<Self> {
            Arc::new(Self {
                status: 200,
                body,
                seen: Mutex::new(Vec::new()),
            })
        }

        fn failing(status: u16) -> Arc<Self> {
            Arc::new(Self {
                status,
                body: "{}",
                seen: Mutex::new(Vec::new()),
            })
        }

        fn requests(&self) -> Vec<QuotaRequest> {
            self.seen.lock().expect("not poisoned").clone()
        }

        fn only(&self) -> QuotaRequest {
            let requests = self.requests();
            assert_eq!(requests.len(), 1, "one endpoint, one call");
            requests[0].clone()
        }
    }

    #[async_trait]
    impl QuotaHttp for FakeHttp {
        async fn request(&self, request: QuotaRequest) -> Result<QuotaResponse> {
            self.seen
                .lock()
                .expect("not poisoned")
                .push(request.clone());
            Ok(QuotaResponse {
                status: self.status,
                headers: vec![("content-type".to_owned(), "application/json".to_owned())],
                body: Bytes::from(self.body),
            })
        }
    }

    fn http_double(fake: &Arc<FakeHttp>) -> Arc<dyn QuotaHttp> {
        Arc::clone(fake) as Arc<dyn QuotaHttp>
    }

    fn account(declared: Value) -> QuotaAccount {
        QuotaAccount::new().with_extra(QUOTA_ENDPOINT_KEY, declared)
    }

    fn context(fake: &Arc<FakeHttp>, account: QuotaAccount) -> QuotaFetchContext {
        QuotaFetchContext::new("openai-clone", Dialect::OpenAi, http_double(fake))
            .with_account(account)
            .with_credential("endpoint-secret")
    }

    fn header<'a>(request: &'a QuotaRequest, name: &str) -> Option<&'a str> {
        request
            .headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    async fn fetch(fake: &Arc<FakeHttp>, declared: Value) -> Result<UsageQuota> {
        OpenAiCompat::new()
            .fetch(&context(fake, account(declared)))
            .await
    }

    #[tokio::test]
    async fn a_fully_resolved_quota_keeps_used_total_and_remaining() {
        let fake = FakeHttp::serving(MAPPED);
        let quota = fetch(
            &fake,
            json!({
                "url": "https://api.example.test/quota",
                "quotas": {
                    "credits": {
                        "used": "$.data.used_usd",
                        "total": "$.data.limit_usd",
                        "remaining": "$.data.remaining_usd",
                        "resetAt": "$.data.renews_at",
                        "currency": "USD",
                        "displayName": "Credits"
                    }
                }
            }),
        )
        .await
        .expect("all three numbers resolve");

        assert_eq!(quota.used, 12.5);
        assert_eq!(quota.total, 50.0);
        assert_eq!(quota.remaining, 37.5);
        assert!(quota.fraction_reported);
        assert!(!quota.unlimited);
        assert_eq!(quota.source.as_deref(), Some(OpenAiCompat::ID));
        assert_eq!(quota.display_name.as_deref(), Some("Credits"));
        assert!(quota.reset_at.is_some());
        assert_eq!(
            quota.details,
            vec![QuotaDetail {
                name: "credits".to_owned(),
                used: 12.5
            }]
        );

        let request = fake.only();
        assert_eq!(request.method, Method::GET);
        assert_eq!(request.url, "https://api.example.test/quota");
        assert_eq!(
            header(&request, "authorization"),
            Some("Bearer endpoint-secret")
        );
        assert_eq!(header(&request, "accept"), Some("application/json"));
        assert_eq!(request.timeout, Some(QUOTA_ENDPOINT_TIMEOUT));
    }

    #[tokio::test]
    async fn an_absent_total_is_derived_from_remaining_plus_used() {
        let fake = FakeHttp::serving(MAPPED);
        let quota = fetch(
            &fake,
            json!({
                "url": "https://api.example.test/quota",
                "quotas": {
                    "credits": { "used": "$.data.used_usd", "remaining": "$.data.remaining_usd" }
                }
            }),
        )
        .await
        .expect("remaining and used are enough");

        assert_eq!(quota.used, 12.5);
        assert_eq!(quota.total, 50.0);
        assert_eq!(quota.remaining, 37.5);
    }

    #[tokio::test]
    async fn a_missing_used_is_derived_from_total_minus_remaining() {
        let fake = FakeHttp::serving(MAPPED);
        let quota = fetch(
            &fake,
            json!({
                "url": "https://api.example.test/quota",
                "quotas": {
                    "credits": { "total": "$.data.limit_usd", "remaining": "$.data.remaining_usd" }
                }
            }),
        )
        .await
        .expect("total and remaining are enough");

        assert_eq!(quota.used, 12.5);
        assert_eq!(quota.total, 50.0);
        assert_eq!(quota.remaining, 37.5);
    }

    #[tokio::test]
    async fn a_total_only_mapping_reports_an_unreported_fraction_not_an_exhausted_one() {
        let fake = FakeHttp::serving(MAPPED);
        let quota = fetch(
            &fake,
            json!({
                "url": "https://api.example.test/quota",
                "quotas": { "credits": { "total": "$.data.limit_usd" } }
            }),
        )
        .await
        .expect("the limit alone is reportable");

        assert_eq!(quota.total, 50.0);
        // Deriving `used = total - 0` would have claimed the account was exhausted.
        assert!(!quota.fraction_reported);
        assert_eq!(quota.remaining_fraction(), None);
        assert!(!quota.is_exhausted());
    }

    #[tokio::test]
    async fn an_unresolvable_path_emits_no_quota_rather_than_zero_over_zero() {
        let fake = FakeHttp::serving(MAPPED);
        let error = fetch(
            &fake,
            json!({
                "url": "https://api.example.test/quota",
                "quotas": {
                    "credits": { "total": "$.data.nope", "remaining": "$.data.also_nope" }
                }
            }),
        )
        .await
        .expect_err("nothing resolved, so there is no quota");

        assert!(matches!(error, Error::NotFound(_)), "{error:?}");
        let message = error.to_string();
        assert!(message.contains("0/0"), "{message}");
        assert!(!message.contains("endpoint-secret"), "no credential leaked");
    }

    #[tokio::test]
    async fn bearer_x_api_key_and_none_set_exactly_one_kind_of_credential_header() {
        let declared = json!({
            "url": "https://api.example.test/quota",
            "quotas": { "credits": { "used": "used", "total": "total" } }
        });

        // bearer is the default.
        let fake = FakeHttp::serving(r#"{"used":1,"total":2}"#);
        fetch(&fake, declared.clone()).await.expect("bearer");
        let request = fake.only();
        assert_eq!(
            header(&request, "authorization"),
            Some("Bearer endpoint-secret")
        );
        assert_eq!(header(&request, "x-api-key"), None);

        let mut with_key = declared.clone();
        with_key["auth"] = json!("x-api-key");
        let fake = FakeHttp::serving(r#"{"used":1,"total":2}"#);
        fetch(&fake, with_key).await.expect("x-api-key");
        let request = fake.only();
        assert_eq!(header(&request, "x-api-key"), Some("endpoint-secret"));
        assert_eq!(header(&request, "authorization"), None);

        let mut with_none = declared;
        with_none["auth"] = json!("none");
        let fake = FakeHttp::serving(r#"{"used":1,"total":2}"#);
        fetch(&fake, with_none).await.expect("none");
        let request = fake.only();
        assert_eq!(header(&request, "authorization"), None);
        assert_eq!(header(&request, "x-api-key"), None);
        assert_eq!(header(&request, "accept"), Some("application/json"));
    }

    #[tokio::test]
    async fn a_credential_requiring_auth_mode_with_no_credential_is_a_configuration_error() {
        let fake = FakeHttp::serving(MAPPED);
        let ctx = QuotaFetchContext::new("openai-clone", Dialect::OpenAi, http_double(&fake))
            .with_account(account(json!({
                "url": "https://api.example.test/quota",
                "auth": "x-api-key",
                "quotas": { "credits": { "used": "used", "total": "total" } }
            })));
        let error = OpenAiCompat::new()
            .fetch(&ctx)
            .await
            .expect_err("an API key mode needs a key");
        assert!(matches!(error, Error::Config(_)), "{error:?}");
        assert!(fake.requests().is_empty());
    }

    #[tokio::test]
    async fn operator_headers_are_merged_last_so_they_win() {
        let fake = FakeHttp::serving(MAPPED);
        fetch(
            &fake,
            json!({
                "url": "https://api.example.test/quota",
                "headers": { "accept": "application/vnd.acme+json", "x-org": "acme" },
                "quotas": { "credits": { "used": "$.data.used_usd", "total": "$.data.limit_usd" } }
            }),
        )
        .await
        .expect("headers merge");

        let request = fake.only();
        let accepts: Vec<&str> = request
            .headers
            .iter()
            .filter(|(key, _)| key == "accept")
            .map(|(_, value)| value.as_str())
            .collect();
        // Ours first, the operator's last: for a repeated name, last wins.
        assert_eq!(
            accepts,
            vec!["application/json", "application/vnd.acme+json"]
        );
        assert_eq!(header(&request, "x-org"), Some("acme"));
    }

    #[tokio::test]
    async fn bracket_index_paths_and_dot_indices_agree() {
        let fake = FakeHttp::serving(MAPPED);
        let bracketed = fetch(
            &fake,
            json!({
                "url": "https://api.example.test/quota",
                "quotas": {
                    "credits": { "used": "$.data.nested[0].c", "total": "$.data.nested[0].cap" }
                }
            }),
        )
        .await
        .expect("bracket index");

        let fake = FakeHttp::serving(MAPPED);
        let dotted = fetch(
            &fake,
            json!({
                "url": "https://api.example.test/quota",
                "quotas": {
                    "credits": { "used": "data.nested.0.c", "total": "data.nested.0.cap" }
                }
            }),
        )
        .await
        .expect("dot index");

        assert_eq!(bracketed, dotted);
        assert_eq!(bracketed.used, 7.0);
        assert_eq!(bracketed.total, 9.0);
    }

    #[test]
    fn resolve_path_handles_the_dot_and_bracket_subset() {
        let root = json!({
            "data": { "used": 3, "items": [ { "c": 7 }, { "c": 8 } ], "nested": { "deep": 1 } },
            "a": { "b": [ { "c": 9 } ] },
            "scalar": 4,
            "nil": null
        });

        // The `$.` form and the bare form agree.
        assert_eq!(resolve_path(&root, "$.data.used"), Some(&json!(3)));
        assert_eq!(resolve_path(&root, "data.used"), Some(&json!(3)));
        assert_eq!(resolve_path(&root, "$.a.b[0].c"), Some(&json!(9)));
        assert_eq!(resolve_path(&root, "a.b[0].c"), Some(&json!(9)));
        assert_eq!(resolve_path(&root, "a.b.0.c"), Some(&json!(9)));
        assert_eq!(resolve_path(&root, "$.data.items[1].c"), Some(&json!(8)));
        assert_eq!(resolve_path(&root, "data.nested.deep"), Some(&json!(1)));
        // A lone `$` is the root, exactly as the source's slice leaves it.
        assert_eq!(resolve_path(&root, "$"), Some(&root));

        // Anything unresolvable is absent, never zero.
        assert_eq!(resolve_path(&root, ""), None);
        assert_eq!(resolve_path(&root, "$.data.missing"), None);
        assert_eq!(resolve_path(&root, "data.items[9]"), None);
        assert_eq!(resolve_path(&root, "data.items[-1]"), None);
        assert_eq!(resolve_path(&root, "data.items[0].c.deeper"), None);
        assert_eq!(resolve_path(&root, "scalar.deeper"), None);
        assert_eq!(resolve_path(&root, "nil.deeper"), None);
        assert_eq!(resolve_path(&root, "data.items.notanindex"), None);
        // A bracket that is not an index stays literal and resolves to nothing.
        assert_eq!(resolve_path(&root, "data.items[x].c"), None);
    }

    #[tokio::test]
    async fn a_bare_url_still_reads_the_conventional_keys() {
        // B6's placeholder declaration: a plain string, no `quotas`, numbers at the top level.
        let fake = FakeHttp::serving(CONVENTIONAL);
        let quota = fetch(&fake, json!("https://api.example.test/quota"))
            .await
            .expect("conventional keys");

        assert_eq!(quota.used, 12.5);
        assert_eq!(quota.total, 50.0);
        assert_eq!(quota.remaining, 37.5);
        assert!(quota.reset_at.is_some());
        assert_eq!(quota.source.as_deref(), Some(OpenAiCompat::ID));

        // And the same object with auth/headers but no `quotas`, reading under `data`.
        let fake = FakeHttp::serving(r#"{"data":{"used":2,"limit":8}}"#);
        let quota = fetch(
            &fake,
            json!({
                "url": "https://api.example.test/quota",
                "auth": "x-api-key",
                "headers": { "x-org": "acme" }
            }),
        )
        .await
        .expect("data envelope");
        assert_eq!(quota.used, 2.0);
        assert_eq!(quota.total, 8.0);
        assert_eq!(header(&fake.only(), "x-api-key"), Some("endpoint-secret"));
    }

    #[tokio::test]
    async fn a_bare_url_with_no_recognisable_counts_is_an_error() {
        let fake = FakeHttp::serving("{}");
        let error = fetch(&fake, json!("https://api.example.test/quota"))
            .await
            .expect_err("nothing to read");
        assert!(matches!(error, Error::NotFound(_)), "{error:?}");
    }

    #[tokio::test]
    async fn several_declared_quotas_become_detail_rows_on_one_account_line() {
        let fake = FakeHttp::serving(MAPPED);
        let quota = fetch(
            &fake,
            json!({
                "url": "https://api.example.test/quota",
                "plan": "$.data.plan_name",
                "quotas": {
                    "credits": { "used": "$.data.used_usd", "total": "$.data.limit_usd" },
                    "requests": { "used": "$.data.nested[0].c", "total": "$.data.nested[0].cap" }
                }
            }),
        )
        .await
        .expect("both entries resolve");

        // Summed headline, earliest reset, and one row per operator name in declared order.
        assert_eq!(quota.used, 19.5);
        assert_eq!(quota.total, 59.0);
        assert_eq!(
            quota
                .details
                .iter()
                .map(|detail| (detail.name.as_str(), detail.used))
                .collect::<Vec<_>>(),
            vec![("credits", 12.5), ("requests", 7.0)]
        );
        assert_eq!(quota.display_name.as_deref(), Some("Pro"));
    }

    #[tokio::test]
    async fn a_missing_declaration_and_an_upstream_failure_are_both_errors() {
        let fake = FakeHttp::serving(MAPPED);
        let error = OpenAiCompat::new()
            .fetch(&context(&fake, QuotaAccount::new()))
            .await
            .expect_err("nothing declared");
        assert!(matches!(error, Error::NotFound(_)), "{error:?}");

        let fake = FakeHttp::failing(401);
        let error = fetch(
            &fake,
            json!({
                "url": "https://api.example.test/quota",
                "quotas": { "credits": { "used": "used", "total": "total" } }
            }),
        )
        .await
        .expect_err("401 is not a quota");
        assert!(matches!(error, Error::InvalidState(_)), "{error:?}");
    }

    #[test]
    fn a_malformed_declaration_is_a_configuration_error() {
        let empty = BTreeMap::new();
        assert!(EndpointSpec::from_extra(&empty)
            .expect("absent is fine")
            .is_none());

        let bad = |declared: Value| {
            let mut extra = BTreeMap::new();
            extra.insert(QUOTA_ENDPOINT_KEY.to_owned(), declared);
            EndpointSpec::from_extra(&extra)
        };
        for declared in [
            json!(""),
            json!(7),
            json!({}),
            json!({ "url": "" }),
            json!({ "url": "https://x.test", "auth": "basic" }),
            json!({ "url": "https://x.test", "method": "not a method" }),
            json!({ "url": "https://x.test", "headers": { "x-org": 3 } }),
            json!({ "url": "https://x.test", "quotas": { "credits": { "used": 3 } } }),
        ] {
            let error = bad(declared.clone()).expect_err("malformed declaration");
            assert!(matches!(error, Error::Config(_)), "{declared}: {error:?}");
        }
    }

    #[test]
    fn supports_claims_everything_but_recognises_a_declared_endpoint() {
        let plain = QuotaAccount::new();
        assert!(declared_endpoint(&plain).is_none());
        assert!(OpenAiCompat::new().supports(Dialect::OpenAi, "anything", &plain));

        let declared = account(json!("https://api.example.test/quota"));
        assert!(declared_endpoint(&declared).is_some());
        assert!(OpenAiCompat::new().supports(Dialect::Anthropic, "x", &declared));
    }
}
