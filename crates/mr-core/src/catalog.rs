//! The declarative provider catalogue: compiled-in seed data, not runtime configuration.
//!
//! ## What this is for
//!
//! `catalog/providers.toml` at the repository root lists every upstream provider Manarouter knows
//! how to talk to: a base URL, a wire [`Dialect`], an [`AuthKind`], the routes it can serve and —
//! for the three OAuth platforms this project implements itself — the exact header fingerprint the
//! upstream demands. It exists so that `mr providers add <id>` and the panel's "add provider"
//! screen can create a correctly-configured provider in one step, rather than asking an operator to
//! already know a base URL, a dialect and an auth style.
//!
//! ## Why compiled in, and not a file read at startup
//!
//! The catalogue is *seed data*, not configuration. An operator never edits it, the panel never
//! writes it, and no upgrade path ever needs to merge it: it is the list of providers that shipped
//! with this build. Compiling it in with [`include_str!`] makes that literal — there is no path to
//! discover, no relative-to-what question, no file that can go missing at runtime and silently
//! downgrade `mr providers add` to an empty list. The separate, operator-owned file is
//! `~/.config/manarouter/config.json` ([`crate::config`]); once a template is instantiated the
//! result is an ordinary [`Provider`] there, and the catalogue is not consulted again.
//!
//! ## Why it is not on the request path
//!
//! Nothing in the relay reads this module. The compiler in [`crate::snapshot`] receives authored
//! `Provider` values, not templates, so a catalogue entry has no representation in the compiled
//! snapshot and cannot be reached by a request. That is deliberate: the request path pays for the
//! providers the operator actually configured, not for the several dozen that exist as
//! suggestions.
//!
//! ## Malformed catalogue: fail loudly, never panic
//!
//! A template data file that does not parse is a build-time defect that must not be shipped and
//! must not be mistaken for "there are no providers". But [`catalog()`] hands out a
//! `&'static Catalog` and is therefore on the panel's and the CLI's path, where a panic would take
//! down an unrelated command. The two requirements are reconciled by *recording* rather than
//! propagating:
//!
//! * parsing happens once inside a [`OnceLock`];
//! * on success the parsed catalogue is the value of that cell, and the error slot stays empty;
//! * on failure the cell holds an empty catalogue and the error string is kept in a second
//!   [`OnceLock`], retrievable with [`load_error()`] so the panel and the CLI can print it.
//!
//! So the failure is loud (the shipped test `a_malformed_shipped_catalogue_would_be_recorded`
//! asserts the real file parses, and every consumer can call [`load_error()`]) but never fatal to
//! an unrelated request. The alternative — `expect()` in `catalog()` — would turn a bad data file
//! into a panicking panel; silently returning empty without recording anything would hide the
//! defect entirely. This module deliberately does neither.
//!
//! ## Conventions the data file follows
//!
//! * `dialect` uses the kebab-case serde spelling: `open-ai`, `anthropic`, `google`, `codex`,
//!   `copilot`, `antigravity`, `command-code`.
//! * `auth` is [`AuthKind`]'s own internally-tagged serde shape, so it is always an inline table:
//!   `{ type = "bearer" }`, `{ type = "api-key-header", header = "x-api-key" }`,
//!   `{ type = "query-param", param = "key" }` or `{ type = "oauth", platform = "openai-codex" }`.
//!   The OAuth tag is pinned to `oauth` by an explicit `rename` on the variant, because the
//!   container's kebab-case rule would otherwise split the acronym into `o-auth`.
//! * A header value of the form `${name}` is a **template the OAuth layer fills from the
//!   credential at request time**, not a literal to send. It is recorded because a missing header
//!   is a hard upstream 400, so the requirement belongs in the data even when the value is not
//!   static.
//! * `base_url` is the prefix the dialect appends its paths to; a per-route deviation is expressed
//!   in `route_overrides` keyed by the kebab-case [`RouteKind`] (`chat-completions`, `responses`,
//!   `messages`, …). `api_key_url` and `docs_url` are omitted wherever the research dumps did not
//!   record one, because inventing a URL is worse than leaving the panel to say "consult the
//!   provider".
//! * `extra` holds provider-specific facts (OAuth constants, an alternate dialect host, a known
//!   path quirk). It is the escape hatch, so an unknown key is *not* diagnosed as an error — see
//!   [`ProviderTemplate::validate`].

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use serde::Deserialize;

use crate::dialect::{AuthKind, Capability, Dialect, KeyStrategy, RouteKind};
use crate::model::{Provider, QuotaWindow};

/// The catalogue, compiled into the binary.
///
/// The path is relative to *this file*, so `../../../` walks `crates/mr-core/src` → `crates/mr-core`
/// → `crates` → the repository root. A test asserts the string is non-empty precisely so a future
/// move of either file fails in CI rather than producing an empty catalogue in a release build.
const CATALOG_TOML: &str = include_str!("../../../catalog/providers.toml");

/// The parse result, resolved exactly once.
static CATALOG: OnceLock<Catalog> = OnceLock::new();

/// Why the compiled-in catalogue failed to parse, if it did. Empty on success.
static LOAD_ERROR: OnceLock<String> = OnceLock::new();

/// One catalogue entry: everything needed to create a working provider except a credential.
///
/// It mirrors [`Provider`] field for field wherever the value is a property of the *upstream*
/// rather than of this installation. Ids, labels, notes and organisation are omitted because they
/// belong to the operator's configuration, and accounts are omitted because the credential is the
/// one thing the catalogue must never contain.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ProviderTemplate {
    /// Stable slug, used as the provider id and as the model-name prefix (`openadapter/…`).
    pub id: String,
    /// Human label for the panel.
    pub label: String,
    /// Which wire API the base URL speaks.
    pub dialect: Dialect,
    /// The prefix every route path is appended to. `https://` unless the provider is loopback.
    pub base_url: String,
    /// How the credential is attached to a request.
    pub auth: AuthKind,
    /// Which client routes this provider can serve.
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    /// How to pick among this provider's accounts once several are attached.
    #[serde(default)]
    pub key_strategy: KeyStrategy,
    /// The OAuth platform id, when this provider logs in rather than taking a pasted key. Kept
    /// separate from `auth` so the panel can answer "which providers can I sign in to?" without
    /// pattern-matching the auth enum.
    #[serde(default)]
    pub oauth_platform: Option<String>,
    /// Fingerprint headers that must accompany every request. A missing one is typically a hard
    /// upstream 400, which is why they live in the data rather than in a comment.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Path segments that override the dialect default, keyed by kebab-case
    /// [`RouteKind`] id.
    #[serde(default)]
    pub route_overrides: BTreeMap<String, String>,
    /// Quota windows the provider is known to enforce. Only windows the research actually records
    /// appear here, and they carry `reset_only = true` when the length is known but the limit is
    /// not.
    #[serde(default)]
    pub quota: Vec<QuotaWindow>,
    /// Where an operator obtains a key, when the research recorded it.
    #[serde(default)]
    pub api_key_url: Option<String>,
    /// A short, sourced caveat: an uncertain endpoint, a partially reverse-engineered protocol, or
    /// a platform this build does not implement yet.
    #[serde(default)]
    pub note: Option<String>,
    /// The provider's own documentation, when the research recorded it.
    #[serde(default)]
    pub docs_url: Option<String>,
    /// Provider-specific facts that have no first-class field: OAuth client ids and endpoints,
    /// alternate dialect hosts, path quirks. Preserved verbatim so a migration can read them back.
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl ProviderTemplate {
    /// Build an authored [`Provider`] from this template, ready for a credential.
    ///
    /// The result is a provider the panel could have produced by hand: the template's dialect, base
    /// URL, auth, capabilities, headers and quota are copied across, and `accounts` is left empty
    /// because only the operator has a credential. Adding an account and saving is all that remains
    /// — see the test `to_provider_passes_validation_once_an_account_is_added`.
    pub fn to_provider(&self) -> Provider {
        let mut provider = Provider::new(self.id.clone(), self.dialect, self.base_url.clone());
        provider.label = Some(self.label.clone());
        provider.auth = self.auth.clone();
        provider.capabilities = self.capabilities.clone();
        provider.key_strategy = self.key_strategy;
        provider.headers = self.headers.clone();
        provider.route_overrides = self.route_overrides.clone();
        provider.quota = self.quota.clone();
        provider.note = self.note.clone();
        provider.extra = self.extra.clone();
        provider
    }

    /// Problems with this entry, for a self-check test.
    ///
    /// This is deliberately *stricter* than [`Provider::validate`]: it checks the invariants the
    /// seed data must hold on its own (identity, a routable base URL, no duplicated capability, an
    /// OAuth template whose two OAuth fields agree) plus the dialect/capability rule that the model
    /// already owns, so a template that would create a misconfigured provider is caught before it
    /// is ever offered to an operator. It also enforces the binding project decision that this
    /// project does not ship providers that authenticate by browser cookie or by scraping a web
    /// session: a marker word in `extra`, a `Cookie` header or a note is reported.
    pub fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();

        if self.id.trim().is_empty() {
            problems.push("id must not be empty".to_string());
        } else if self.id.contains('/') || self.id.contains(char::is_whitespace) {
            problems.push(format!(
                "id {:?} must not contain '/' or whitespace, because it is used as a model-name \
                 prefix",
                self.id
            ));
        }
        if self.label.trim().is_empty() {
            problems.push("label must not be empty".to_string());
        }
        if !is_routable_base_url(&self.base_url) {
            problems.push(format!(
                "base_url {:?} must be an https:// URL or a loopback http:// URL",
                self.base_url
            ));
        }
        if self.capabilities.is_empty() {
            problems.push("capabilities must not be empty".to_string());
        }
        let mut seen = BTreeSet::new();
        for capability in &self.capabilities {
            if !seen.insert(*capability) {
                problems.push(format!(
                    "capability {} is listed more than once",
                    capability.as_str()
                ));
            }
        }

        match (&self.oauth_platform, &self.auth) {
            (Some(declared), AuthKind::OAuth { platform }) => {
                if declared != platform {
                    problems.push(format!(
                        "oauth_platform {declared:?} disagrees with the auth platform {platform:?}"
                    ));
                }
            }
            (Some(declared), _) => problems.push(format!(
                "oauth_platform {declared:?} is set but auth is not an OAuth shape"
            )),
            (None, AuthKind::OAuth { platform }) => problems.push(format!(
                "auth is OAuth (platform {platform:?}) but oauth_platform is not set"
            )),
            (None, _) => {}
        }

        // Reuse the model's own rule so a template and a hand-built provider cannot disagree about
        // what a dialect can serve.
        problems.extend(self.to_provider().dialect_capability_mismatches());

        problems.extend(forbidden_auth_markers(
            &self.extra,
            &self.headers,
            self.note.as_deref(),
        ));

        problems
    }
}

/// The whole catalog, parsed once.
///
/// Constructed only by this module; [`catalog()`] is the sole way to obtain one. The by-id index is
/// a `BTreeMap` rather than a `HashMap` so iteration and diagnostics are deterministic, which
/// matters more here than the lookup constant: this is a panel-facing list, not a hot path.
#[derive(Debug, Default, Clone)]
pub struct Catalog {
    version: u32,
    providers: Vec<ProviderTemplate>,
    by_id: BTreeMap<String, usize>,
}

impl Catalog {
    /// Parse a file that has already been read. Fails on a duplicate id rather than hiding it:
    /// two templates with one id would make `get` depend on file order.
    fn from_file(file: CatalogFile) -> Result<Self, String> {
        let mut by_id = BTreeMap::new();
        for (index, provider) in file.providers.iter().enumerate() {
            if let Some(previous) = by_id.insert(provider.id.clone(), index) {
                return Err(format!(
                    "duplicate provider id {:?} at entries {previous} and {index}",
                    provider.id
                ));
            }
        }
        Ok(Self {
            version: file.version,
            providers: file.providers,
            by_id,
        })
    }

    /// Every template, in file order.
    pub fn all(&self) -> &[ProviderTemplate] {
        &self.providers
    }

    /// Look a template up by its exact id. Use [`Catalog::search`] for the panel's free-text box.
    pub fn get(&self, id: &str) -> Option<&ProviderTemplate> {
        self.by_id
            .get(id)
            .and_then(|index| self.providers.get(*index))
    }

    /// Case-insensitive search over id, label and note — what the panel's "add provider" box calls.
    ///
    /// An empty query returns everything, so the box shows a full list before the user types.
    pub fn search(&self, query: &str) -> Vec<&ProviderTemplate> {
        let needle = query.trim().to_lowercase();
        if needle.is_empty() {
            return self.providers.iter().collect();
        }
        self.providers
            .iter()
            .filter(|provider| {
                provider.id.to_lowercase().contains(&needle)
                    || provider.label.to_lowercase().contains(&needle)
                    || provider
                        .note
                        .as_deref()
                        .is_some_and(|note| note.to_lowercase().contains(&needle))
            })
            .collect()
    }

    /// How many templates the catalogue ships.
    pub fn len(&self) -> usize {
        self.providers.len()
    }

    /// Whether the catalogue is empty. An empty catalogue in a release build means the compiled-in
    /// file was lost or unparseable — check [`load_error()`].
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    /// The data file's format version, so a migration can tell one revision from the next.
    pub fn version(&self) -> u32 {
        self.version
    }

    /// Templates that support an OAuth login rather than a pasted key.
    pub fn oauth_capable(&self) -> Vec<&ProviderTemplate> {
        self.providers
            .iter()
            .filter(|provider| provider.oauth_platform.is_some())
            .collect()
    }

    /// Templates that can serve a given route.
    pub fn supporting(&self, route: RouteKind) -> Vec<&ProviderTemplate> {
        let needed = Capability::for_route(route);
        self.providers
            .iter()
            .filter(|provider| provider.capabilities.contains(&needed))
            .collect()
    }
}

/// The whole catalog, parsed once.
///
/// The first call parses [`CATALOG_TOML`]; every later call returns the same reference. On failure
/// the returned catalogue is empty and the reason is recorded for [`load_error()`] — see the module
/// documentation for why this is neither a panic nor a silent empty list.
pub fn catalog() -> &'static Catalog {
    CATALOG.get_or_init(|| match parse_shipped() {
        Ok(catalog) => catalog,
        Err(error) => {
            // `set` can only fail if something already stored a reason, which cannot happen before
            // the first parse; ignore the result rather than panicking over an unreachable branch.
            let _ = LOAD_ERROR.set(error);
            Catalog::default()
        }
    })
}

/// Why the compiled-in catalogue failed to parse, if it did.
///
/// Returns `None` after a successful parse. The panel and the CLI call this to explain an empty
/// provider list instead of showing one as if it were the whole truth.
pub fn load_error() -> Option<&'static str> {
    // Make sure the one-shot parse has happened before reporting on it.
    let _ = catalog();
    LOAD_ERROR.get().map(String::as_str)
}

/// Parse the compiled-in data file. Kept separate from [`catalog()`] so tests can assert on the
/// real file's contents, and so the failure path is a plain `Result` rather than a `OnceLock`.
fn parse_shipped() -> Result<Catalog, String> {
    if CATALOG_TOML.trim().is_empty() {
        return Err(
            "catalog/providers.toml is empty: the include_str! path resolved to nothing"
                .to_string(),
        );
    }
    let file: CatalogFile =
        toml::from_str(CATALOG_TOML).map_err(|e| format!("catalog/providers.toml: {e}"))?;
    Catalog::from_file(file)
}

/// The on-disk shape of `catalog/providers.toml`.
#[derive(Debug, Deserialize)]
struct CatalogFile {
    /// Data-file format version. Required, so a file that predates versioning fails loudly instead
    /// of being read as version 0.
    version: u32,
    /// Every template, in the order the file lists them.
    providers: Vec<ProviderTemplate>,
}

/// Whether a base URL is one this project is willing to dial: `https://` anywhere, or `http://`
/// only when it points at the loopback interface, so a plaintext credential can never be sent to a
/// remote host by a typo in the catalogue.
fn is_routable_base_url(url: &str) -> bool {
    if let Some(rest) = url.strip_prefix("https://") {
        return !rest.trim_matches('/').is_empty();
    }
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or("");
    let host = match authority.rsplit_once(':') {
        // Keep the host only when what follows the last colon is a port, so `[::1]` is not split.
        Some((host, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => host,
        _ => authority,
    };
    matches!(host, "127.0.0.1" | "localhost" | "[::1]" | "::1")
}

/// Marker words that mean an entry crosses the project's binding "no cookie, no web session"
/// line. Matched case-insensitively against extra keys and values, header names, and the note.
const FORBIDDEN_AUTH_MARKERS: [&str; 3] = ["cookie", "session_token", "scrap"];

/// Report any marker word found in the fields that describe a provider's auth. Returns one problem
/// per offending field so the message names the field rather than only the word.
fn forbidden_auth_markers(
    extra: &BTreeMap<String, serde_json::Value>,
    headers: &BTreeMap<String, String>,
    note: Option<&str>,
) -> Vec<String> {
    let mut problems = Vec::new();
    let mut check = |field: String, text: &str| {
        let lower = text.to_ascii_lowercase();
        if let Some(marker) = FORBIDDEN_AUTH_MARKERS
            .into_iter()
            .find(|marker| lower.contains(marker))
        {
            problems.push(format!(
                "{field} mentions {marker:?}: this project does not ship providers that \
                 authenticate by browser cookie or by scraping a web session"
            ));
        }
    };
    for (key, value) in extra {
        check(format!("extra key {key:?}"), key);
        check(format!("extra value for {key:?}"), &value.to_string());
    }
    for (name, value) in headers {
        check(format!("header name {name:?}"), name);
        check(format!("header {name:?} value"), value);
    }
    if let Some(note) = note {
        check("note".to_string(), note);
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::WindowKind;
    use crate::ids::{AccountId, OAuthCredentialId};
    use crate::model::Account;

    /// Parse the shipped file and unwrap it, printing the parse error as the panic message so a
    /// broken data file reports the offending line rather than just "None".
    fn shipped() -> Catalog {
        parse_shipped().unwrap_or_else(|e| panic!("the shipped catalogue must parse: {e}"))
    }

    #[test]
    fn a_non_empty_shipped_file_is_compiled_in() {
        // This is the test that catches a wrong `include_str!` depth: the path is resolved at
        // compile time, but an empty or truncated file would otherwise sail through as an empty
        // catalogue.
        assert!(
            !CATALOG_TOML.trim().is_empty(),
            "catalog/providers.toml is empty: the include_str! path is wrong or the file is gone"
        );
    }

    #[test]
    fn the_shipped_catalogue_parses_and_is_not_empty() {
        let catalog = shipped();
        assert!(
            catalog.len() >= 50,
            "expected the full shipped catalogue, found {} entries",
            catalog.len()
        );
        assert!(!catalog.is_empty());
        assert_eq!(catalog.version(), 1);
        assert_eq!(catalog.all().len(), catalog.len());
        assert_eq!(
            load_error(),
            None,
            "the shipped catalogue must parse cleanly"
        );
    }

    #[test]
    fn every_entry_has_identity_and_a_routable_base_url() {
        for provider in catalog().all() {
            assert!(!provider.id.trim().is_empty(), "an entry has an empty id");
            assert!(
                !provider.label.trim().is_empty(),
                "{} has an empty label",
                provider.id
            );
            assert!(
                is_routable_base_url(&provider.base_url),
                "{} has base_url {:?}, which is neither https:// nor loopback http://",
                provider.id,
                provider.base_url
            );
            assert!(
                !provider.capabilities.is_empty(),
                "{} declares no capabilities, so it can serve nothing",
                provider.id
            );
        }
    }

    #[test]
    fn provider_ids_are_unique() {
        let catalog = catalog();
        let ids: BTreeSet<&str> = catalog.all().iter().map(|p| p.id.as_str()).collect();
        assert_eq!(
            ids.len(),
            catalog.len(),
            "provider ids must be unique across the catalogue"
        );
    }

    #[test]
    fn oauth_platform_and_auth_shape_agree_everywhere() {
        for provider in catalog().all() {
            match (&provider.oauth_platform, &provider.auth) {
                (Some(declared), AuthKind::OAuth { platform }) => {
                    assert_eq!(declared, platform, "{}: platform mismatch", provider.id)
                }
                (Some(_), other) => {
                    panic!("{} sets oauth_platform but auth is {other:?}", provider.id)
                }
                (None, AuthKind::OAuth { platform }) => panic!(
                    "{} has OAuth auth ({platform}) but no oauth_platform",
                    provider.id
                ),
                (None, _) => {}
            }
        }
    }

    #[test]
    fn the_three_self_implemented_oauth_platforms_carry_their_fingerprints() {
        let catalog = catalog();

        let codex = catalog
            .get("openai-codex")
            .expect("openai-codex is shipped");
        assert_eq!(codex.dialect, Dialect::Codex);
        assert_eq!(codex.oauth_platform.as_deref(), Some("openai-codex"));
        assert_eq!(
            codex.headers.get("originator").map(String::as_str),
            Some("codex_cli_rs"),
            "Codex requires originator: codex_cli_rs"
        );
        assert!(
            codex.headers.contains_key("chatgpt-account-id"),
            "the relay injects chatgpt-account-id from the JWT; the requirement belongs in the data"
        );
        // Unknown keys must survive the `serde(flatten)` escape hatch, or OAuth constants recorded
        // in the data file would be silently dropped.
        assert_eq!(
            codex.extra.get("oauth_client_id").and_then(|v| v.as_str()),
            Some("app_EMoamEEZ73f0CkXaXp7hrann"),
            "extra must round-trip unknown keys"
        );

        let copilot = catalog
            .get("github-copilot")
            .expect("github-copilot is shipped");
        assert_eq!(copilot.dialect, Dialect::Copilot);
        assert_eq!(copilot.oauth_platform.as_deref(), Some("github-copilot"));
        assert!(
            copilot.headers.contains_key("Editor-Version"),
            "a missing Editor-Version is a hard 400"
        );
        assert!(
            copilot.headers.contains_key("Copilot-Integration-Id"),
            "Copilot-Integration-Id selects the integrator model policy"
        );

        let antigravity = catalog.get("antigravity").expect("antigravity is shipped");
        assert_eq!(antigravity.dialect, Dialect::Antigravity);
        assert_eq!(antigravity.oauth_platform.as_deref(), Some("antigravity"));
        assert!(
            antigravity.headers.contains_key("X-Goog-Api-Client"),
            "Antigravity requires the X-Goog-Api-Client fingerprint"
        );
        assert!(
            antigravity.headers.contains_key("Client-Metadata"),
            "Antigravity requires the Client-Metadata JSON header"
        );

        let oauth_ids: BTreeSet<&str> = catalog
            .oauth_capable()
            .iter()
            .map(|p| p.id.as_str())
            .collect();
        for id in ["openai-codex", "github-copilot", "antigravity"] {
            assert!(oauth_ids.contains(id), "{id} must be oauth_capable");
        }
    }

    #[test]
    fn no_entry_authenticates_by_cookie_or_browser_session() {
        for provider in catalog().all() {
            // The auth enum has no cookie variant, but an arbitrary header could smuggle one in.
            if let AuthKind::Header { name, .. } = &provider.auth {
                let lower = name.to_ascii_lowercase();
                for marker in FORBIDDEN_AUTH_MARKERS {
                    assert!(
                        !lower.contains(marker),
                        "{} authenticates through header {name:?}",
                        provider.id
                    );
                }
            }
            for (key, value) in &provider.extra {
                let key = key.to_ascii_lowercase();
                let value = value.to_string().to_ascii_lowercase();
                for marker in FORBIDDEN_AUTH_MARKERS {
                    assert!(
                        !key.contains(marker) && !value.contains(marker),
                        "{}: extra {key:?} = {value} mentions {marker:?}",
                        provider.id
                    );
                }
            }
            assert!(
                forbidden_auth_markers(
                    &provider.extra,
                    &provider.headers,
                    provider.note.as_deref()
                )
                .is_empty(),
                "{} carries a cookie/session marker",
                provider.id
            );
        }
    }

    #[test]
    fn to_provider_preserves_the_template_and_passes_validation_once_an_account_is_added() {
        for template in catalog().all() {
            let mut provider = template.to_provider();
            assert_eq!(provider.id.as_str(), template.id);
            assert_eq!(provider.dialect, template.dialect, "{}", template.id);
            assert_eq!(provider.base_url, template.base_url, "{}", template.id);
            assert_eq!(provider.headers, template.headers, "{}", template.id);
            assert_eq!(
                provider.capabilities, template.capabilities,
                "{}",
                template.id
            );
            assert_eq!(provider.route_overrides, template.route_overrides);
            assert_eq!(provider.quota, template.quota);
            assert_eq!(provider.extra, template.extra);
            assert!(
                provider.accounts.is_empty(),
                "{}: to_provider must not invent an account",
                template.id
            );

            let mut account = Account::new("test");
            account.id = AccountId::from_raw("test-account");
            if template.auth.is_oauth() {
                account.oauth_ref = Some(OAuthCredentialId::from_raw("test-oauth"));
            } else {
                account.secret_ref = Some("test-secret".to_string());
            }
            provider.accounts.push(account);

            assert!(
                provider.validate().is_empty(),
                "{} produced an invalid provider: {:?}",
                template.id,
                provider.validate()
            );
        }
    }

    #[test]
    fn search_is_case_insensitive_over_id_and_label() {
        let catalog = catalog();
        let by_id = catalog.search("OPENAI-CODEX");
        assert!(by_id.iter().any(|p| p.id == "openai-codex"));

        let by_label = catalog.search("github copilot");
        assert!(
            by_label.iter().any(|p| p.id == "github-copilot"),
            "search must match the human label"
        );

        // A note match is what lets "antigravity" find the entry whose label differs.
        assert!(!catalog.search("antigravity").is_empty());

        assert_eq!(
            catalog.search("   ").len(),
            catalog.len(),
            "an empty query lists everything"
        );
        assert!(catalog.search("no-such-provider-xyz").is_empty());
    }

    #[test]
    fn get_finds_a_known_id_and_misses_an_unknown_one() {
        let catalog = catalog();
        let openai = catalog.get("openai").expect("openai is shipped");
        assert_eq!(openai.dialect, Dialect::OpenAi);
        assert_ne!(openai.base_url, "");
        assert!(catalog.get("definitely-not-a-provider").is_none());
    }

    #[test]
    fn supporting_finds_chat_providers_and_excludes_embeddings_only_ones() {
        let catalog = catalog();
        let chat: BTreeSet<&str> = catalog
            .supporting(RouteKind::ChatCompletions)
            .iter()
            .map(|p| p.id.as_str())
            .collect();
        for id in ["openai", "groq", "openrouter", "deepseek", "ollama"] {
            assert!(chat.contains(id), "{id} must serve chat completions");
        }
        // Voyage and Jina are embeddings-only in this catalogue, so they must not appear.
        assert!(!chat.contains("voyage"));
        assert!(!chat.contains("jina"));

        let embeddings: BTreeSet<&str> = catalog
            .supporting(RouteKind::Embeddings)
            .iter()
            .map(|p| p.id.as_str())
            .collect();
        assert!(embeddings.contains("voyage"));
        assert!(embeddings.contains("jina"));
        assert!(!embeddings.contains("openai-codex"));

        let messages: BTreeSet<&str> = catalog
            .supporting(RouteKind::Messages)
            .iter()
            .map(|p| p.id.as_str())
            .collect();
        assert!(messages.contains("anthropic"));
    }

    #[test]
    fn the_shipped_entries_all_validate_cleanly() {
        for provider in catalog().all() {
            let problems = provider.validate();
            assert!(
                problems.is_empty(),
                "{} failed its own self-check: {problems:?}",
                provider.id
            );
        }
    }

    #[test]
    fn validate_reports_a_constructed_bad_template() {
        let good = catalog().get("openai").expect("openai is shipped").clone();

        let mut bad = good.clone();
        bad.id = String::new();
        bad.label = String::new();
        bad.base_url = "api.example.com".to_string();
        bad.capabilities = vec![Capability::Chat, Capability::Chat];
        let problems = bad.validate();
        assert!(problems.iter().any(|p| p.contains("id must not be empty")));
        assert!(problems
            .iter()
            .any(|p| p.contains("label must not be empty")));
        assert!(problems.iter().any(|p| p.contains("base_url")));
        assert!(
            problems.iter().any(|p| p.contains("more than once")),
            "a duplicated capability must be reported: {problems:?}"
        );

        // The OAuth pair must agree in both directions.
        let mut bad = good.clone();
        bad.oauth_platform = Some("ghost-platform".to_string());
        assert!(bad
            .validate()
            .iter()
            .any(|p| p.contains("not an OAuth shape")));
        let mut bad = good;
        bad.auth = AuthKind::OAuth {
            platform: "ghost-platform".to_string(),
        };
        assert!(bad
            .validate()
            .iter()
            .any(|p| p.contains("oauth_platform is not set")));
    }

    #[test]
    fn validate_enforces_the_no_cookie_policy() {
        let mut bad = catalog().get("openai").unwrap().clone();
        bad.extra.insert(
            "session_token".to_string(),
            serde_json::Value::String("borrowed from a browser".to_string()),
        );
        assert!(bad
            .validate()
            .iter()
            .any(|p| p.contains("session_token") || p.contains("browser cookie")));
    }

    #[test]
    fn base_url_accepts_https_and_loopback_and_rejects_the_rest() {
        assert!(is_routable_base_url("https://api.openai.com/v1"));
        assert!(is_routable_base_url("http://127.0.0.1:11434/v1"));
        assert!(is_routable_base_url("http://localhost:8080/v1"));
        assert!(is_routable_base_url("http://[::1]:8000/v1"));
        assert!(!is_routable_base_url("http://api.example.com/v1"));
        assert!(!is_routable_base_url("api.example.com"));
        assert!(!is_routable_base_url("https://"));
        assert!(!is_routable_base_url(""));
    }

    #[test]
    fn a_parse_failure_is_recorded_rather_than_panicking() {
        // `Catalog::from_file` is the failure point `parse_shipped` delegates to; a duplicate id
        // must be an error, not "last one wins", so `get` cannot depend on file order.
        let duplicated = r#"
            version = 1
            [[providers]]
            id = "dupe"
            label = "First"
            dialect = "open-ai"
            base_url = "https://api.example.com/v1"
            auth = { type = "bearer" }
            [[providers]]
            id = "dupe"
            label = "Second"
            dialect = "open-ai"
            base_url = "https://api.example.com/v1"
            auth = { type = "bearer" }
        "#;
        let file: CatalogFile = toml::from_str(duplicated).expect("the fixture itself must parse");
        let error = Catalog::from_file(file).expect_err("a duplicate id must fail");
        assert!(error.contains("duplicate provider id"), "{error}");

        // And a truly malformed file is an Err, never an empty-but-successful catalogue.
        assert!(toml::from_str::<CatalogFile>("version = 1\nproviders = 3").is_err());
    }

    #[test]
    fn quota_windows_are_positive_and_provider_declared() {
        for provider in catalog().all() {
            for window in &provider.quota {
                assert!(
                    window.seconds > 0,
                    "{} has a non-positive quota window",
                    provider.id
                );
            }
        }
        let anthropic = catalog().get("anthropic").expect("anthropic is shipped");
        assert_eq!(anthropic.quota.len(), 1);
        assert_eq!(anthropic.quota[0].kind, WindowKind::ProviderDeclared);
        assert!(anthropic.quota[0].reset_only);
    }
}
