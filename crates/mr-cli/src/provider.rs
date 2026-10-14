//! `mr provider` — add, inspect, test, enable and remove upstream providers.
//!
//! Adding a provider resolves three sources in a documented order: the catalogue (a template with
//! a real base URL, auth kind and headers), a dialect spelling, and the operator's own flags.
//! What reaches disk is always validated first ([`Ctx::save`]).
//!
//! `provider test` is the one command here that speaks to the network: a single minimal
//! authenticated request, built with the relay's exact TLS stack (webpki roots, ALPN h2/http1.1),
//! reporting the upstream's real status and a **redacted** body — never a credential.

use clap::Subcommand;

use http::Method;
use mr_core::catalog::{catalog, load_error};
use mr_core::dialect::{AuthKind, Capability, Dialect, KeyStrategy};
use mr_core::ids::ProviderId;
use mr_core::model::Provider;
use mr_core::secret::provider_secret_id;

use crate::output::{
    all_labels, paint, parse_choice, redact_text, require_provider, say, serde_label, CliError,
    Table,
};
use crate::Ctx;

// The variants differ a lot in size because clap arguments carry their own documentation
// and defaults inline. This enum is constructed once, at parse time, from `std::env::args`
// and dropped immediately, so the unused bytes in the small variants are never held; the
// alternative is boxing every payload, which for a `Subcommand` derive means extracting
// each inline struct variant into a named type purely to satisfy the lint.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand, Debug)]
pub enum ProviderCmd {
    /// Add a provider: a catalogue id, or a dialect + base URL for a custom endpoint.
    Add(AddArgs),
    /// List configured providers.
    List,
    /// Show one provider in full.
    Show {
        /// Provider id.
        id: String,
    },
    /// Remove a provider. Refuses while credentials or combos still reference it, unless --force.
    Remove {
        /// Provider id.
        id: String,
        /// Also drop the provider's vault entries and combo steps.
        #[arg(long)]
        force: bool,
    },
    /// Re-enable a disabled provider.
    Enable {
        /// Provider id.
        id: String,
    },
    /// Disable a provider without deleting it.
    Disable {
        /// Provider id.
        id: String,
    },
    /// Make one real, minimal authenticated request and report the upstream's actual response.
    Test {
        /// Provider id.
        id: String,
        /// Model to exercise (default: the provider's first model, or a catalogue probe).
        #[arg(long)]
        model: Option<String>,
    },
}

#[derive(clap::Args, Debug)]
pub struct AddArgs {
    /// Catalogue id (`openai`), a dialect spelling (`openai-compatible`), or the new provider id
    /// when --dialect and --base-url describe a custom endpoint.
    pub target: String,
    /// Name for the new provider (defaults to the catalogue id, or to `target`).
    #[arg(long)]
    pub id: Option<String>,
    /// Wire dialect for a custom endpoint (`open-ai`, `anthropic`, `google`, ...).
    #[arg(long, value_name = "DIALECT")]
    pub dialect: Option<String>,
    /// Base URL of a custom endpoint.
    #[arg(long, value_name = "URL")]
    pub base_url: Option<String>,
    /// Human label shown in tables and the panel.
    #[arg(long)]
    pub label: Option<String>,
    /// Seed from this catalogue template instead of the positional.
    #[arg(long = "from-catalog", value_name = "ID")]
    pub from_catalog: Option<String>,
    /// How the credential is attached: bearer, api-key, header:NAME[:PREFIX], query[:PARAM],
    /// oauth:PLATFORM.
    #[arg(long, value_name = "KIND")]
    pub auth: Option<String>,
    /// Add a capability (`--capability chat`, `--capability audio-transcription`, ...).
    #[arg(long = "capability", value_name = "CAPABILITY")]
    pub capability: Vec<String>,
    /// Add a static header (`--header "anthropic-version: 2023-06-01"`).
    #[arg(long = "header", value_name = "NAME: VALUE")]
    pub header: Vec<String>,
    /// Account selection strategy for this provider.
    #[arg(long = "key-strategy", value_name = "STRATEGY")]
    pub key_strategy: Option<String>,
    /// TCP connect timeout in milliseconds.
    #[arg(long = "connect-timeout-ms", value_name = "MS")]
    pub connect_timeout_ms: Option<u64>,
    /// Time-to-first-byte timeout in milliseconds.
    #[arg(long = "ttfb-timeout-ms", value_name = "MS")]
    pub ttfb_timeout_ms: Option<u64>,
    /// Idle-connection timeout in milliseconds.
    #[arg(long = "idle-timeout-ms", value_name = "MS")]
    pub idle_timeout_ms: Option<u64>,
    /// Whole-request timeout in milliseconds (0 = unlimited).
    #[arg(long = "total-timeout-ms", value_name = "MS")]
    pub total_timeout_ms: Option<u64>,
}

/// Capability spellings; `Capability` has no `ALL` const, so this list is written out and held by
/// a test against the enum's serde labels.
pub(crate) const CAPABILITY_CHOICES: [&str; 11] = [
    "chat",
    "responses",
    "embeddings",
    "audio-transcription",
    "audio-speech",
    "images-generations",
    "models",
    "messages",
    "tool-calling",
    "reasoning",
    "vision",
];

/// Run a provider subcommand.
pub async fn run(cmd: ProviderCmd, ctx: &Ctx) -> Result<(), CliError> {
    match cmd {
        ProviderCmd::Add(a) => add(ctx, a),
        ProviderCmd::List => list(ctx),
        ProviderCmd::Show { id } => show(ctx, &id),
        ProviderCmd::Remove { id, force } => remove(ctx, &id, force),
        ProviderCmd::Enable { id } => set_enabled(ctx, &id, true),
        ProviderCmd::Disable { id } => set_enabled(ctx, &id, false),
        ProviderCmd::Test { id, model } => test(ctx, &id, model.as_deref()).await,
    }
}

// ---------------------------------------------------------------------------
// provider add — resolution rules
// ---------------------------------------------------------------------------

/// Provider ids are config keys: URL-safe, boring, and never path-shaped.
fn check_id(id: &str) -> Result<(), CliError> {
    if id.is_empty() {
        return Err(CliError::usage("a provider id cannot be empty"));
    }
    if id.len() > 64 {
        return Err(CliError::usage(format!(
            "provider id `{id}` is longer than 64 characters"
        )));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(CliError::usage(format!(
            "provider id `{id}` may only contain letters, digits, `-`, `_` and `.`"
        )));
    }
    Ok(())
}

/// Parse `--auth`'s grammar. The serde spelling (`bearer`, `api-key-header`, `query-param`, ...)
/// is accepted first; the parameterised forms are what the flag grammar adds.
pub(crate) fn parse_auth(raw: &str) -> Result<AuthKind, CliError> {
    let trimmed = raw.trim();
    let lowered = trimmed.to_ascii_lowercase();
    if let Ok(kind) = serde_json::from_str::<AuthKind>(&format!("\"{lowered}\"")) {
        return Ok(kind);
    }
    let (head, rest) = match trimmed.split_once(':') {
        Some((h, r)) => (h.trim().to_ascii_lowercase(), Some(r.trim().to_owned())),
        None => (lowered.clone(), None),
    };
    match head.as_str() {
        "bearer" => Ok(AuthKind::Bearer),
        "api-key" | "api-key-header" | "apikey" => Ok(AuthKind::ApiKeyHeader {
            header: rest.unwrap_or_else(|| "x-api-key".to_owned()),
        }),
        "header" => {
            let spec = rest.ok_or_else(|| {
                CliError::usage(
                    "--auth header expects `header:NAME[:PREFIX]`, e.g. `header:x-api-key`",
                )
            })?;
            let (name, prefix) = match spec.split_once(':') {
                Some((n, p)) => (n.trim().to_owned(), p.trim().to_owned()),
                None => (spec.trim().to_owned(), String::new()),
            };
            if name.is_empty() {
                return Err(CliError::usage(format!(
                    "invalid --auth `{raw}`: header name is empty"
                )));
            }
            Ok(AuthKind::Header { name, prefix })
        }
        "query" | "query-param" => Ok(AuthKind::QueryParam {
            param: rest
                .filter(|r| !r.is_empty())
                .unwrap_or_else(|| "key".to_owned()),
        }),
        "oauth" => {
            let platform = rest
                .filter(|r| !r.is_empty())
                .ok_or_else(|| CliError::usage("--auth oauth expects `oauth:PLATFORM`"))?;
            Ok(AuthKind::OAuth { platform })
        }
        _ => Err(CliError::usage(format!(
            "invalid --auth `{raw}`: expected bearer, api-key, api-key-header, \
             header:NAME[:PREFIX], query[:PARAM], or oauth:PLATFORM"
        ))),
    }
}

/// Parse `--header "Name: value"`.
fn parse_header(spec: &str) -> Result<(String, String), CliError> {
    let (name, value) = spec
        .split_once(':')
        .ok_or_else(|| CliError::usage(format!("--header expects `Name: value`, got `{spec}`")))?;
    let (name, value) = (name.trim(), value.trim());
    if name.is_empty() || value.is_empty() {
        return Err(CliError::usage(format!(
            "--header needs both a name and a value, got `{spec}`"
        )));
    }
    Ok((name.to_owned(), value.to_owned()))
}

/// Resolve `provider add` flags into the provider that would be written.
///
/// Precedence, tested in the module's tests:
/// 1. id: `--id` > catalogue template id > positional (which must then *not* be a dialect spelling
///    unless `--id` says what to call it);
/// 2. dialect: `--dialect` > template > `Dialect::parse(positional)` > open-ai for a hand-added
///    endpoint;
/// 3. base URL: `--base-url` > template > refusal (a provider without one cannot serve).
pub(crate) fn resolve_add(a: &AddArgs) -> Result<Provider, CliError> {
    let template = match &a.from_catalog {
        Some(t) => Some(catalog().get(t).ok_or_else(|| {
            let mut e = CliError::usage(format!("no catalogue entry named `{t}`"));
            e = match load_error() {
                Some(err) => e.with_detail(format!("catalogue failed to load: {err}")),
                None => e.with_detail(format!(
                    "the catalogue has {} entries; common ids: openai, anthropic, google, \
                     openrouter, deepseek, groq",
                    catalog().len()
                )),
            };
            e
        })?),
        None => catalog().get(&a.target),
    };

    let dialect_flag = match &a.dialect {
        Some(d) => Some(parse_choice(
            "--dialect",
            d,
            &all_labels(&Dialect::ALL),
            Dialect::parse,
        )?),
        None => None,
    };

    let positional_is_dialect =
        template.is_none() && dialect_flag.is_none() && Dialect::parse(&a.target).is_some();
    let id = if let Some(id) = &a.id {
        id.clone()
    } else if let Some(t) = template {
        t.id.clone()
    } else if positional_is_dialect {
        return Err(CliError::usage(format!(
            "`{}` names a dialect, not a provider id; pass --id <name> to say what this \
             endpoint should be called",
            a.target
        )));
    } else {
        a.target.clone()
    };
    check_id(&id)?;

    let dialect = dialect_flag
        .or_else(|| template.map(|t| t.dialect))
        .unwrap_or(Dialect::OpenAi);

    let base_url = a
        .base_url
        .clone()
        .or_else(|| template.map(|t| t.base_url.clone()))
        .ok_or_else(|| {
            CliError::usage(format!(
                "`{}` matches no catalogue entry, so there is no base URL to copy: pass \
                 --base-url https://... (and optionally --dialect open-ai)",
                a.target
            ))
        })?;
    if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
        return Err(CliError::usage(format!(
            "--base-url must start with http:// or https://, got `{base_url}`"
        )));
    }

    let mut p = match template {
        Some(t) => t.to_provider(),
        None => Provider::new(&id, dialect, &base_url),
    };
    p.id = ProviderId::from(id);
    p.dialect = dialect;
    p.base_url = base_url;
    if let Some(label) = &a.label {
        p.label = Some(label.clone());
    }
    if let Some(auth) = &a.auth {
        p.auth = parse_auth(auth)?;
    }
    for cap in &a.capability {
        let cap: Capability = parse_choice(
            "--capability",
            cap,
            &CAPABILITY_CHOICES.map(str::to_owned),
            Capability::parse,
        )?;
        if !p.capabilities.contains(&cap) {
            p.capabilities.push(cap);
        }
    }
    for h in &a.header {
        let (name, value) = parse_header(h)?;
        p.headers.insert(name, value);
    }
    if let Some(strategy) = &a.key_strategy {
        p.key_strategy = parse_choice(
            "--key-strategy",
            strategy,
            &all_labels(&KeyStrategy::ALL),
            KeyStrategy::parse,
        )?;
    }
    if let Some(ms) = a.connect_timeout_ms {
        p.connect_timeout_ms = ms;
    }
    if let Some(ms) = a.ttfb_timeout_ms {
        p.ttfb_timeout_ms = ms;
    }
    if let Some(ms) = a.idle_timeout_ms {
        p.idle_timeout_ms = ms;
    }
    if let Some(ms) = a.total_timeout_ms {
        p.total_timeout_ms = ms;
    }
    Ok(p)
}

fn add(ctx: &Ctx, a: AddArgs) -> Result<(), CliError> {
    let mut cfg = ctx.load()?;
    let p = resolve_add(&a)?;
    if cfg.providers.iter().any(|e| e.id.as_str() == p.id.as_str()) {
        return Err(CliError::validation(format!(
            "provider `{}` already exists; nothing was written",
            p.id
        ))
        .with_detail(format!(
            "inspect it with `mr provider show {}`, or pass --id to name the new one",
            p.id
        )));
    }
    let human = vec![
        format!(
            "provider `{}` added: dialect {}, base {}",
            p.id,
            serde_label(&p.dialect),
            p.base_url
        ),
        format!("next: mr account add {} --secret-stdin", p.id),
    ];
    let doc = serde_json::to_value(&p)
        .map_err(|e| CliError::failure(format!("cannot serialise the provider: {e}")))?;
    cfg.providers.push(p);
    let out = ctx.save(&cfg)?;
    ctx.finish(&out, human, doc)
}

// ---------------------------------------------------------------------------
// list / show / enable / disable
// ---------------------------------------------------------------------------

/// One-word status for tables: what an operator needs to know at a glance.
fn status(p: &Provider) -> &'static str {
    if !p.enabled {
        return "disabled";
    }
    let has_secret = p
        .accounts
        .iter()
        .any(|a| a.enabled && a.secret_ref.is_some());
    if has_secret {
        "ready"
    } else {
        "no credential"
    }
}

fn status_cell(enabled_color: bool, p: &Provider) -> String {
    let s = status(p);
    if !enabled_color {
        return s.to_owned();
    }
    match s {
        "ready" => paint(true, "32", s),
        "disabled" => paint(true, "90", s),
        _ => paint(true, "33", s),
    }
}

fn list(ctx: &Ctx) -> Result<(), CliError> {
    let cfg = ctx.load()?;
    if ctx.json {
        let rows: Vec<serde_json::Value> = cfg
            .providers
            .iter()
            .map(|p| serde_json::to_value(p).unwrap_or_else(|_| serde_json::Value::Null))
            .collect();
        return crate::output::print_json(&rows);
    }
    let color = crate::output::use_color();
    let mut t = Table::new(["ID", "DIALECT", "BASE URL", "ACCOUNTS", "MODELS", "STATUS"]);
    for p in &cfg.providers {
        t.row(vec![
            p.id.as_str().to_owned(),
            serde_label(&p.dialect),
            p.base_url.clone(),
            p.accounts.iter().filter(|a| a.enabled).count().to_string(),
            p.models.len().to_string(),
            status_cell(color, p),
        ]);
    }
    t.print();
    if cfg.providers.is_empty() {
        say("no providers yet; start with `mr provider add openai`");
    }
    Ok(())
}

fn show(ctx: &Ctx, id: &str) -> Result<(), CliError> {
    let cfg = ctx.load()?;
    let p = require_provider(&cfg, id)?;
    if ctx.json {
        return crate::output::print_json(p);
    }
    let color = crate::output::use_color();
    let mut t = Table::new(["FIELD", "VALUE"]);
    let auth = serde_json::to_string(&p.auth).unwrap_or_else(|_| "?".to_owned());
    let accounts: Vec<String> = p
        .accounts
        .iter()
        .map(|a| {
            format!(
                "{} ({}, priority {})",
                a.label.clone().unwrap_or_else(|| a.id.as_str().to_owned()),
                if a.enabled { "enabled" } else { "disabled" },
                a.priority
            )
        })
        .collect();
    t.row(vec!["id".to_owned(), p.id.as_str().to_owned()]);
    t.row(vec![
        "label".to_owned(),
        p.label.clone().unwrap_or_default(),
    ]);
    t.row(vec!["dialect".to_owned(), serde_label(&p.dialect)]);
    t.row(vec!["base_url".to_owned(), p.base_url.clone()]);
    t.row(vec!["auth".to_owned(), auth]);
    t.row(vec![
        "capabilities".to_owned(),
        p.capabilities
            .iter()
            .map(|c| c.as_str().to_owned())
            .collect::<Vec<_>>()
            .join(", "),
    ]);
    t.row(vec![
        "key_strategy".to_owned(),
        serde_label(&p.key_strategy),
    ]);
    t.row(vec!["status".to_owned(), status_cell(color, p)]);
    t.row(vec![
        "accounts".to_owned(),
        if accounts.is_empty() {
            "(none)".to_owned()
        } else {
            accounts.join(", ")
        },
    ]);
    t.row(vec![
        "models".to_owned(),
        match &p.models_synced_at {
            Some(ts) => format!("{} (synced {})", p.models.len(), ts),
            None => format!("{} (never synced)", p.models.len()),
        },
    ]);
    if let Some(note) = &p.note {
        t.row(vec!["note".to_owned(), note.clone()]);
    }
    t.print();
    Ok(())
}

fn set_enabled(ctx: &Ctx, id: &str, enabled: bool) -> Result<(), CliError> {
    let mut cfg = ctx.load()?;
    let p = cfg
        .providers
        .iter_mut()
        .find(|p| p.id.as_str() == id)
        .ok_or_else(|| CliError::not_found(format!("no provider with id `{id}`")))?;
    p.enabled = enabled;
    let verb = if enabled { "enabled" } else { "disabled" };
    let doc = serde_json::json!({ "id": id, "enabled": enabled });
    let out = ctx.save(&cfg)?;
    ctx.finish(&out, vec![format!("provider `{id}` {verb}")], doc)
}

// ---------------------------------------------------------------------------
// remove
// ---------------------------------------------------------------------------

fn remove(ctx: &Ctx, id: &str, force: bool) -> Result<(), CliError> {
    let mut cfg = ctx.load()?;
    {
        let p = require_provider(&cfg, id)?;
        let mut refs: Vec<String> = Vec::new();
        for c in &cfg.combos {
            for (i, s) in c.steps.iter().enumerate() {
                if s.provider.as_str() == id {
                    refs.push(format!("combo `{}` step {i}", c.name));
                }
            }
        }
        for a in &cfg.aliases {
            if a.to.starts_with(&format!("{id}/")) {
                refs.push(format!("alias `{}` -> `{}`", a.from, a.to));
            }
        }
        if !refs.is_empty() && !force {
            return Err(CliError::validation(format!(
                "provider `{id}` is still referenced; nothing was written"
            ))
            .with_details(refs)
            .with_detail("re-run with --force to remove it anyway (its vault entries go too)"));
        }
        // A provider with credentials is exactly the orphan-the-credential case --force exists for,
        // but say what is about to happen when it is not forced either.
        let _ = p;
    }

    // Drop steps and aliases that would dangle, then the provider itself.
    let mut dropped_steps = 0usize;
    for c in &mut cfg.combos {
        let before = c.steps.len();
        c.steps.retain(|s| s.provider.as_str() != id);
        dropped_steps += before - c.steps.len();
    }
    let alias_prefix = format!("{id}/");
    let before = cfg.aliases.len();
    cfg.aliases.retain(|a| !a.to.starts_with(&alias_prefix));
    let dropped_aliases = before - cfg.aliases.len();

    cfg.providers.retain(|p| p.id.as_str() != id);

    let mut human = vec![format!("provider `{id}` removed")];
    if dropped_steps > 0 {
        human.push(format!("{dropped_steps} combo step(s) dropped with it"));
    }
    if dropped_aliases > 0 {
        human.push(format!("{dropped_aliases} alias(es) dropped with it"));
    }

    let out = ctx.save(&cfg)?;

    // Vault cleanup happens only after the config write succeeded: deleting a credential for a
    // provider that is still configured would leave it broken, which is the worse failure.
    let mut removed_secrets = 0usize;
    let mut vault = ctx.vault()?;
    for secret_id in vault.ids() {
        if (secret_id == provider_secret_id(id)
            || secret_id.starts_with(&format!("provider/{id}/")))
            && vault.delete(&secret_id)
        {
            removed_secrets += 1;
        }
    }
    if removed_secrets > 0 {
        vault.flush().map_err(|e| {
            CliError::failure(format!(
                "provider removed from the configuration, but the vault could not be rewritten: {e}"
            ))
        })?;
        human.push(format!("{removed_secrets} vault entr(y/ies) deleted"));
    }

    let doc = serde_json::json!({
        "id": id,
        "removed": true,
        "steps_dropped": dropped_steps,
        "aliases_dropped": dropped_aliases,
        "vault_entries_deleted": removed_secrets,
    });
    ctx.finish(&out, human, doc)
}

// ---------------------------------------------------------------------------
// provider test
// ---------------------------------------------------------------------------

pub(crate) mod net {
    //! Everything the outbound probe needs: how to attach the credential and where to send it.
    //!
    //! One TLS client for every outbound probe, built exactly like manarouter-relay's upstream
    //! client — same root set, same ALPN order — so `provider test` reports what the router
    //! would see. (No reqwest exists in this workspace; this is the stack the relay already
    //! builds.)

    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    use hyper_util::client::legacy::connect::HttpConnector;
    use hyper_util::client::legacy::Client;
    use hyper_util::rt::TokioExecutor;

    use crate::output::CliError;

    pub type HttpClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, Full<Bytes>>;

    /// Response bodies are capped: a probe that needs more than 16 KiB is the wrong probe, and an
    /// unbounded body read is how a hostile upstream runs you out of memory.
    const MAX_BODY: usize = 16 * 1024;

    /// A reply a probe can print honestly: status, bounded body, wall time.
    pub struct Reply {
        pub status: http::StatusCode,
        pub body: String,
        pub elapsed: Duration,
    }

    /// Build the client. Roots load per process lifetime here (one probe run is one process).
    pub fn client() -> HttpClient {
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let mut tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        // Server picks the first protocol it also supports: h2-capable upstreams get h2,
        // everyone else falls back to HTTP/1.1.
        tls.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let mut http = HttpConnector::new();
        http.set_nodelay(true);
        // Lets one connector handle plaintext `http://` loopback providers (Ollama, vLLM, ...).
        http.enforce_http(false);
        let https = hyper_rustls::HttpsConnector::from((http, Arc::new(tls)));
        Client::builder(TokioExecutor::new())
            .http2_only(false)
            .build(https)
    }

    /// Percent-encode for a query parameter (secrets ride in `?key=` on Google-style endpoints).
    pub fn url_encode(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for b in s.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(b as char)
                }
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    }

    /// One bounded request. Errors are `CliError`s with the exit code the failure deserves.
    pub async fn request(
        client: &HttpClient,
        method: http::Method,
        url: &str,
        headers: &[(String, String)],
        body: Option<String>,
        timeout: Duration,
    ) -> Result<Reply, CliError> {
        use http::{header, HeaderName, HeaderValue, Request};

        let uri: http::Uri = url
            .parse()
            .map_err(|e| CliError::validation(format!("not a usable URL: {url} ({e})")))?;
        let mut req = Request::builder().method(method).uri(uri);
        if body.is_some() {
            req = req.header(header::CONTENT_TYPE, "application/json");
        }
        for (name, value) in headers {
            let n = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| CliError::validation(format!("invalid header name `{name}`")))?;
            let v = HeaderValue::from_str(value).map_err(|_| {
                CliError::validation(format!(
                    "value for header `{name}` contains characters that cannot go on the wire"
                ))
            })?;
            req = req.header(n, v);
        }
        let payload = body.unwrap_or_default();
        let req = req
            .body(Full::new(Bytes::from(payload)))
            .map_err(|e| CliError::failure(format!("cannot build the request: {e}")))?;

        let started = Instant::now();
        let resp = tokio::time::timeout(timeout, client.request(req))
            .await
            .map_err(|_| {
                CliError::failure(format!(
                    "timed out after {}ms waiting for {url}",
                    timeout.as_millis()
                ))
            })?
            .map_err(|e| CliError::failure(format!("cannot reach {url}: {e}")))?;
        let status = resp.status();
        let mut body = resp.into_body();

        let mut collected: Vec<u8> = Vec::new();
        while collected.len() < MAX_BODY {
            let frame = tokio::time::timeout(timeout, body.frame())
                .await
                .map_err(|_| CliError::failure(format!("timed out reading the body from {url}")))?
                .transpose()
                .map_err(|e| CliError::failure(format!("reading the response from {url}: {e}")))?;
            let Some(frame) = frame else { break };
            if let Ok(data) = frame.into_data() {
                let take = (MAX_BODY - collected.len()).min(data.len());
                collected.extend_from_slice(&data[..take]);
            }
        }
        Ok(Reply {
            status,
            body: String::from_utf8_lossy(&collected).into_owned(),
            elapsed: started.elapsed(),
        })
    }
}

/// Credential for a provider: the first enabled account's vault secret, else a provider-wide one.
pub(crate) fn credential(ctx: &Ctx, p: &Provider) -> Result<String, CliError> {
    let vault = ctx.vault()?;
    if p.accounts.is_empty() {
        return Err(CliError::not_found(format!(
            "provider `{}` has no credential: add one with `mr account add {} --secret-stdin`",
            p.id, p.id
        )));
    }
    let mut missing = Vec::new();
    for a in p.accounts.iter().filter(|a| a.enabled) {
        match &a.secret_ref {
            Some(secret_id) => match vault.get(secret_id) {
                Ok(Some(s)) if !s.is_empty() => return Ok(s),
                Ok(_) => missing.push(format!(
                    "account {}: `{secret_id}` is referenced but not stored",
                    a.label.clone().unwrap_or_else(|| a.id.as_str().to_owned())
                )),
                Err(e) => return Err(CliError::from(e)),
            },
            None => missing.push(format!(
                "account {}: no credential attached",
                a.label.clone().unwrap_or_else(|| a.id.as_str().to_owned())
            )),
        }
    }
    let provider_wide = provider_secret_id(p.id.as_str());
    if vault.contains(&provider_wide) {
        if let Ok(Some(s)) = vault.get(&provider_wide) {
            if !s.is_empty() {
                return Ok(s);
            }
        }
    }
    if matches!(p.auth, AuthKind::OAuth { .. }) {
        return Err(CliError::failure(format!(
            "`{}` is an OAuth provider and has no stored credential for the CLI to use; the CLI \
             cannot mint an OAuth session — sign in through the panel instead",
            p.id
        ))
        .with_detail("see docs/10-oauth-overview.md for the login flow"));
    }
    Err(
        CliError::not_found(format!("provider `{}` has no usable credential", p.id))
            .with_details(missing)
            .with_detail(format!(
                "add one with `mr account add {} --secret-stdin`",
                p.id
            )),
    )
}

/// Attach the credential according to the provider's `auth`. Query-param credentials ride in the
/// URL instead and are handled by the caller.
pub(crate) fn auth_headers(p: &Provider, secret: &str) -> Result<Vec<(String, String)>, CliError> {
    Ok(match &p.auth {
        AuthKind::Bearer | AuthKind::OAuth { .. } => {
            vec![("authorization".to_owned(), format!("Bearer {secret}"))]
        }
        AuthKind::ApiKeyHeader { header } => {
            vec![(header.clone(), secret.to_owned())]
        }
        AuthKind::Header { name, prefix } => {
            vec![(name.clone(), format!("{prefix}{secret}"))]
        }
        AuthKind::QueryParam { .. } => Vec::new(),
    })
}

/// How a `--model` probe talks to each dialect.
enum ProbeKind {
    Chat,
    Messages,
    Responses,
    Google,
}

fn probe_kind(d: Dialect) -> ProbeKind {
    match d {
        Dialect::Anthropic => ProbeKind::Messages,
        Dialect::Google => ProbeKind::Google,
        Dialect::Codex => ProbeKind::Responses,
        _ => ProbeKind::Chat,
    }
}

fn join(base: &str, path: &str) -> String {
    format!("{}{}", base.trim_end_matches('/'), path)
}

fn probe_body(kind: &ProbeKind, model: &str) -> String {
    match kind {
        ProbeKind::Chat | ProbeKind::Responses => serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "ping"}],
        })
        .to_string(),
        ProbeKind::Messages => serde_json::json!({
            "model": model,
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "ping"}],
        })
        .to_string(),
        ProbeKind::Google => serde_json::json!({
            "contents": [{"role": "user", "parts": [{"text": "ping"}]}],
        })
        .to_string(),
    }
}

fn probe_path(kind: &ProbeKind, model: &str) -> String {
    match kind {
        ProbeKind::Chat => "/chat/completions".to_owned(),
        ProbeKind::Messages => "/messages".to_owned(),
        ProbeKind::Responses => "/responses".to_owned(),
        ProbeKind::Google => format!("/models/{}:generateContent", net::url_encode(model)),
    }
}

/// One real request; the report says what the upstream actually answered.
async fn test(ctx: &Ctx, id: &str, model: Option<&str>) -> Result<(), CliError> {
    let cfg = ctx.load()?;
    let p = require_provider(&cfg, id)?.clone();
    let secret = credential(ctx, &p)?;
    let headers = auth_headers(&p, &secret)?;

    // Default probe: GET the model list. It authenticates without choosing a model, so it cannot
    // fail for the wrong reason. `--model` escalates to a real one-token conversation.
    let (method, url, body) = match (model, probe_kind(p.dialect)) {
        (None, _) => (Method::GET, join(&p.base_url, "/models"), None),
        (Some(m), kind) => {
            let mut url = join(&p.base_url, &probe_path(&kind, m));
            if let AuthKind::QueryParam { param } = &p.auth {
                let sep = if url.contains('?') { '&' } else { '?' };
                url.push(sep);
                url.push_str(param);
                url.push('=');
                url.push_str(&net::url_encode(&secret));
            }
            (Method::POST, url, Some(probe_body(&kind, m)))
        }
    };

    let client = net::client();
    let reply = net::request(
        &client,
        method.clone(),
        &url,
        &headers,
        body,
        std::time::Duration::from_millis(15_000),
    )
    .await?;

    let redacted = redact_text(&reply.body);
    let truncated: String = redacted.chars().take(600).collect();
    let ok = reply.status.is_success();
    let listed_models = if !ok || model.is_some() {
        None
    } else {
        // Count what a model-listing endpoint returned, tolerating both `{"data":[...]}` and
        // `{"models":[...]}` shapes.
        serde_json::from_str::<serde_json::Value>(&reply.body)
            .ok()
            .and_then(|v| {
                v.get("data")
                    .or_else(|| v.get("models"))
                    .and_then(|arr| arr.as_array())
                    .map(|a| a.len())
            })
    };

    let verdict = if ok {
        match listed_models {
            Some(n) => format!("OK — authenticated, {n} models listed"),
            None => "OK — authenticated".to_owned(),
        }
    } else {
        match reply.status.as_u16() {
            401 | 403 => "FAIL — the credential was rejected".to_owned(),
            404 => "FAIL — endpoint not found; check base_url".to_owned(),
            429 => "FAIL — rate limited (the credential itself was accepted)".to_owned(),
            500..=599 => "FAIL — upstream error".to_owned(),
            _ => "FAIL".to_owned(),
        }
    };

    if ctx.json {
        crate::output::print_json(&serde_json::json!({
            "id": id,
            "method": method.as_str(),
            "url": url,
            "status": reply.status.as_u16(),
            "ok": ok,
            "latency_ms": reply.elapsed.as_millis(),
            "models": listed_models,
            "outcome": verdict,
            "body": truncated,
        }))?;
    } else {
        let mut t = Table::new(["FIELD", "VALUE"]);
        t.row(vec![
            "endpoint".to_owned(),
            format!("{} {url}", method.as_str()),
        ]);
        t.row(vec![
            "status".to_owned(),
            format!(
                "{} {}",
                reply.status.as_u16(),
                reply.status.canonical_reason().unwrap_or("")
            ),
        ]);
        t.row(vec![
            "latency".to_owned(),
            format!("{}ms", reply.elapsed.as_millis()),
        ]);
        let color = crate::output::use_color();
        let painted = if ok {
            paint(color, "32", &verdict)
        } else {
            paint(color, "31", &verdict)
        };
        t.row(vec!["outcome".to_owned(), painted]);
        if !truncated.trim().is_empty() {
            t.row(vec!["body".to_owned(), truncated.clone()]);
        }
        t.print();
    }

    if !ok {
        return Err(CliError::failure(format!(
            "provider `{id}` probe failed: HTTP {}",
            reply.status.as_u16()
        ))
        .with_detail(verdict));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(target: &str) -> AddArgs {
        AddArgs {
            target: target.to_owned(),
            id: None,
            dialect: None,
            base_url: None,
            label: None,
            from_catalog: None,
            auth: None,
            capability: Vec::new(),
            header: Vec::new(),
            key_strategy: None,
            connect_timeout_ms: None,
            ttfb_timeout_ms: None,
            idle_timeout_ms: None,
            total_timeout_ms: None,
        }
    }

    #[test]
    fn catalogue_template_supplies_id_dialect_and_base_url() {
        let p = resolve_add(&args("anthropic")).unwrap();
        assert_eq!(p.id.as_str(), "anthropic");
        assert_eq!(p.dialect, Dialect::Anthropic);
        assert_eq!(p.base_url, "https://api.anthropic.com/v1");
        assert_eq!(
            p.auth,
            AuthKind::ApiKeyHeader {
                header: "x-api-key".into()
            }
        );
        assert!(p.headers.contains_key("anthropic-version"));
    }

    #[test]
    fn dialect_spelling_with_id_builds_a_custom_endpoint() {
        let mut a = args("openai-compatible");
        a.id = Some("gateway".into());
        a.base_url = Some("https://gateway.internal/v1".into());
        let p = resolve_add(&a).unwrap();
        assert_eq!(p.id.as_str(), "gateway");
        assert_eq!(p.dialect, Dialect::OpenAi);
        assert_eq!(p.base_url, "https://gateway.internal/v1");
    }

    #[test]
    fn dialect_spelling_without_id_is_a_usage_error() {
        let mut a = args("openai-compatible");
        a.base_url = Some("https://gateway.internal/v1".into());
        let err = resolve_add(&a).unwrap_err();
        assert_eq!(err.exit, crate::output::Exit::Usage);
        assert!(err.message.contains("--id"), "{}", err.message);
    }

    #[test]
    fn hand_added_endpoint_defaults_to_the_openai_dialect() {
        let mut a = args("myproxy");
        a.base_url = Some("http://127.0.0.1:8080/v1".into());
        let p = resolve_add(&a).unwrap();
        assert_eq!(p.id.as_str(), "myproxy");
        assert_eq!(p.dialect, Dialect::OpenAi);
    }

    #[test]
    fn unknown_target_without_base_url_is_a_usage_error() {
        let err = resolve_add(&args("frobnicate")).unwrap_err();
        assert_eq!(err.exit, crate::output::Exit::Usage);
        assert!(err.message.contains("--base-url"), "{}", err.message);
    }

    #[test]
    fn unknown_catalogue_template_lists_the_problem() {
        let mut a = args("whatever");
        a.from_catalog = Some("nope".into());
        a.base_url = Some("https://x/v1".into());
        let err = resolve_add(&a).unwrap_err();
        assert_eq!(err.exit, crate::output::Exit::Usage);
        assert!(err.message.contains("nope"), "{}", err.message);
    }

    #[test]
    fn dialect_flag_wins_over_the_template() {
        let mut a = args("openai");
        a.dialect = Some("open-ai".into());
        a.base_url = Some("http://localhost:9999/v1".into());
        let p = resolve_add(&a).unwrap();
        assert_eq!(p.id.as_str(), "openai");
        assert_eq!(p.dialect, Dialect::OpenAi);
        assert_eq!(p.base_url, "http://localhost:9999/v1");
    }

    #[test]
    fn capability_and_header_flags_reach_the_provider() {
        let mut a = args("myproxy");
        a.base_url = Some("http://127.0.0.1:1/v1".into());
        a.capability = vec!["audio-transcription".into(), "chat".into()];
        a.header = vec!["X-Team: platform".into()];
        a.key_strategy = Some("rr".into());
        let p = resolve_add(&a).unwrap();
        assert!(p.capabilities.contains(&Capability::AudioTranscription));
        assert_eq!(
            p.headers.get("X-Team").map(String::as_str),
            Some("platform")
        );
        assert_eq!(p.key_strategy, KeyStrategy::RoundRobin);
    }

    #[test]
    fn check_id_rejects_path_shaped_ids() {
        assert!(check_id("openai").is_ok());
        assert!(check_id("openai-compatible-chat-42").is_ok());
        assert!(check_id("has/slash").is_err());
        assert!(check_id("has space").is_err());
        assert!(check_id("").is_err());
    }

    #[test]
    fn parse_auth_covers_the_documented_grammar() {
        assert_eq!(parse_auth("bearer").unwrap(), AuthKind::Bearer);
        assert_eq!(
            parse_auth("api-key").unwrap(),
            AuthKind::ApiKeyHeader {
                header: "x-api-key".into()
            }
        );
        assert_eq!(
            parse_auth("api-key:authorization").unwrap(),
            AuthKind::ApiKeyHeader {
                header: "authorization".into()
            }
        );
        assert_eq!(
            parse_auth("header:x-trace:Token ").unwrap(),
            AuthKind::Header {
                name: "x-trace".into(),
                prefix: "Token".into()
            }
        );
        assert_eq!(
            parse_auth("query").unwrap(),
            AuthKind::QueryParam {
                param: "key".into()
            }
        );
        assert_eq!(
            parse_auth("query:api_token").unwrap(),
            AuthKind::QueryParam {
                param: "api_token".into()
            }
        );
        assert_eq!(
            parse_auth("oauth:github").unwrap(),
            AuthKind::OAuth {
                platform: "github".into()
            }
        );
        // The serde spelling of the plain kinds is accepted too.
        assert_eq!(
            parse_auth("query-param").unwrap(),
            AuthKind::QueryParam {
                param: "key".into()
            }
        );
        let err = parse_auth("carrier-pigeon").unwrap_err();
        assert_eq!(err.exit, crate::output::Exit::Usage);
        assert!(err.message.contains("expected"), "{}", err.message);
        let err = parse_auth("oauth").unwrap_err();
        assert!(err.message.contains("oauth:PLATFORM"), "{}", err.message);
    }

    #[test]
    fn capability_choices_match_the_enum_serde_labels() {
        for variant in [
            Capability::Chat,
            Capability::Responses,
            Capability::Embeddings,
            Capability::AudioTranscription,
            Capability::AudioSpeech,
            Capability::ImagesGenerations,
            Capability::Models,
            Capability::Messages,
            Capability::ToolCalling,
            Capability::Reasoning,
            Capability::Vision,
        ] {
            let label = serde_json::to_value(variant)
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned();
            assert!(
                CAPABILITY_CHOICES.contains(&label.as_str()),
                "{label} missing from CAPABILITY_CHOICES"
            );
            assert!(
                Capability::parse(&label).is_some(),
                "{label} must parse back"
            );
        }
        assert_eq!(CAPABILITY_CHOICES.len(), 11);
    }

    #[test]
    fn status_reflects_enabled_and_credential() {
        let mut p = Provider::new("x", Dialect::OpenAi, "http://127.0.0.1:1/v1");
        assert_eq!(status(&p), "no credential");
        let mut acct = mr_core::model::Account::new("main");
        acct.secret_ref = Some("provider/x/account/a".into());
        p.accounts.push(acct);
        assert_eq!(status(&p), "ready");
        p.enabled = false;
        assert_eq!(status(&p), "disabled");
    }

    #[test]
    fn url_encode_leaves_key_material_url_safe_and_escapes_the_rest() {
        assert_eq!(net::url_encode("abc-123_~."), "abc-123_~.");
        assert_eq!(net::url_encode("a b&c"), "a%20b%26c");
    }

    #[test]
    fn probe_bodies_are_dialect_appropriate() {
        assert!(probe_body(&ProbeKind::Messages, "claude").contains("\"max_tokens\":1"));
        assert!(probe_body(&ProbeKind::Chat, "gpt").contains("\"messages\""));
        assert!(probe_body(&ProbeKind::Google, "gem").contains("\"contents\""));
        assert_eq!(
            probe_path(&ProbeKind::Google, "gemini 2"),
            "/models/gemini%202:generateContent"
        );
        assert_eq!(probe_path(&ProbeKind::Responses, "x"), "/responses");
    }
}
