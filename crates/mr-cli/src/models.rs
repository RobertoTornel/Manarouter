//! `mr models` — a provider's model catalogue: read it from config, refresh it from upstream.
//!
//! `models sync` is one bounded `GET /models` per provider, merged with the rule mr-core itself
//! establishes: `manual` entries are authoritative and a sync never overwrites them.

use clap::Subcommand;

use mr_core::dialect::AuthKind;
use mr_core::ids::Millis;
use mr_core::model::{ModelEntry, ModelSource, Provider};

use crate::output::{all_labels, parse_choice, print_json, require_provider, say, CliError, Table};
use crate::provider::net;
use crate::provider::{auth_headers, credential};
use crate::Ctx;

#[derive(Subcommand, Debug)]
pub enum ModelsCmd {
    /// List models (`models list` alone lists every provider's, as qualified ids).
    List {
        /// Provider id (positional).
        provider: Option<String>,
        /// Provider id (`--provider`, the same value as the positional).
        #[arg(long = "provider")]
        provider_flag: Option<String>,
        /// Only entries with this source: synced, manual, inferred.
        #[arg(long, value_name = "SOURCE")]
        source: Option<String>,
    },
    /// Fetch the provider's model list over the network and store it.
    Sync {
        /// Provider id (positional).
        provider: Option<String>,
        /// Sync every configured provider.
        #[arg(long)]
        all: bool,
    },
}

impl ModelsCmd {
    /// The provider a `List` invocation should operate on, from either spelling.
    pub fn list_provider<'a>(
        provider: &'a Option<String>,
        provider_flag: &'a Option<String>,
    ) -> Option<&'a str> {
        provider.as_deref().or(provider_flag.as_deref())
    }
}

/// Run a models subcommand.
pub async fn run(cmd: ModelsCmd, ctx: &Ctx) -> Result<(), CliError> {
    match cmd {
        ModelsCmd::List {
            provider,
            provider_flag,
            source,
        } => list(
            ctx,
            ModelsCmd::list_provider(&provider, &provider_flag),
            &source,
        ),
        ModelsCmd::Sync { provider, all } => sync(ctx, provider.as_deref(), all).await,
    }
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

/// `--source` accepts the serde spelling and the obvious colloquial aliases.
fn parse_source(raw: &str) -> Result<ModelSource, CliError> {
    let choices = all_labels(&[
        ModelSource::Synced,
        ModelSource::Manual,
        ModelSource::Inferred,
    ]);
    parse_choice("--source", raw, &choices, |s| {
        Some(match s.to_ascii_lowercase().as_str() {
            "synced" | "sync" | "discovered" => ModelSource::Synced,
            "manual" | "hand" => ModelSource::Manual,
            "inferred" => ModelSource::Inferred,
            _ => return None,
        })
    })
}

fn row_for(id: &str, m: &ModelEntry) -> Vec<String> {
    vec![
        id.to_owned(),
        m.id.clone(),
        m.label.clone().unwrap_or_default(),
        m.context_length.map(|n| n.to_string()).unwrap_or_default(),
        crate::output::serde_label(&m.source),
    ]
}

const HEADERS: [&str; 5] = ["PROVIDER", "MODEL", "LABEL", "CONTEXT", "SOURCE"];

fn list(ctx: &Ctx, provider: Option<&str>, source: &Option<String>) -> Result<(), CliError> {
    let cfg = ctx.load()?;
    let filter = match source {
        Some(raw) => Some(parse_source(raw)?),
        None => None,
    };
    let matches = |m: &ModelEntry| filter.is_none_or(|f| m.source == f);

    if let Some(pid) = provider {
        let p = require_provider(&cfg, pid)?;
        if ctx.json {
            let entries: Vec<&ModelEntry> = p.models.iter().filter(|m| matches(m)).collect();
            return print_json(&entries);
        }
        let mut t = Table::new(HEADERS);
        for m in p.models.iter().filter(|m| matches(m)) {
            t.row(row_for(p.id.as_str(), m));
        }
        t.print();
        if t.is_empty() {
            if p.models.is_empty() {
                say(format!(
                    "no models stored for `{pid}`; run `mr models sync {pid}` to fetch them"
                ));
            } else {
                say("no models match that --source filter");
            }
        }
        return Ok(());
    }

    // No provider: every stored model, qualified with its provider so the id is unambiguous.
    if ctx.json {
        let rows: Vec<serde_json::Value> = cfg
            .providers
            .iter()
            .flat_map(|p| {
                p.models
                    .iter()
                    .filter(|m| matches(m))
                    .map(|m| serde_json::json!({ "provider": p.id, "model": m }))
            })
            .collect();
        return print_json(&rows);
    }
    let mut t = Table::new(HEADERS);
    for p in &cfg.providers {
        for m in p.models.iter().filter(|m| matches(m)) {
            t.row(row_for(p.id.as_str(), m));
        }
    }
    t.print();
    if t.is_empty() {
        say("no models stored anywhere; run `mr models sync --all` to fetch them");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// sync
// ---------------------------------------------------------------------------

/// One model as the upstream described it.
#[derive(Debug)]
struct Fetched {
    id: String,
    label: Option<String>,
    context_length: Option<u32>,
    max_output_tokens: Option<u32>,
    capabilities: Vec<mr_core::dialect::Capability>,
}

/// Parse a `/models` reply: OpenAI's `{"data":[{"id":...}]}`, Google's
/// `{"models":[{"name":"models/...",...}]}`, or a bare array. The Google `models/` prefix is
/// stripped — the relay's `model` field carries the bare id and the endpoint re-adds it.
fn parse_models(body: &str) -> Result<Vec<Fetched>, CliError> {
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| CliError::failure(format!("the /models response is not JSON: {e}")))?;
    let arr = if let Some(a) = v.as_array() {
        a
    } else {
        v.get("data")
            .or_else(|| v.get("models"))
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                CliError::failure("the /models response has neither a `data` nor a `models` array")
            })?
    };

    let number = |item: &serde_json::Value, keys: &[&str]| -> Option<u32> {
        keys.iter()
            .find_map(|k| item.get(*k).and_then(serde_json::Value::as_u64))
            .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
    };

    let mut out: Vec<Fetched> = Vec::new();
    for item in arr {
        let f = if let Some(s) = item.as_str() {
            Fetched {
                id: s.to_owned(),
                label: None,
                context_length: None,
                max_output_tokens: None,
                capabilities: Vec::new(),
            }
        } else {
            let id = item
                .get("id")
                .and_then(serde_json::Value::as_str)
                .or_else(|| item.get("name").and_then(serde_json::Value::as_str))
                .ok_or_else(|| {
                    CliError::failure("a model entry has neither an `id` nor a `name`")
                })?;
            let label = item
                .get("label")
                .or_else(|| item.get("displayName"))
                .or_else(|| item.get("display_name"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            let capabilities = item
                .get("capabilities")
                .and_then(serde_json::Value::as_array)
                .map(|caps| {
                    caps.iter()
                        .filter_map(serde_json::Value::as_str)
                        .filter_map(mr_core::dialect::Capability::parse)
                        .collect()
                })
                .unwrap_or_default();
            Fetched {
                id: id.to_owned(),
                label,
                context_length: number(
                    item,
                    &[
                        "context_length",
                        "contextLength",
                        "context_window",
                        "contextWindow",
                    ],
                ),
                max_output_tokens: number(item, &["max_output_tokens", "maxOutputTokens"]),
                capabilities,
            }
        };
        let id = f.id.strip_prefix("models/").unwrap_or(&f.id).to_owned();
        if id.is_empty() {
            continue;
        }
        out.push(Fetched { id, ..f });
    }
    Ok(out)
}

/// Merge a fetch into a provider's catalogue. Returns `(added, refreshed)`.
///
/// The rule mirrors [`mr_core::model::ModelSource::is_overwritable_by_sync`]: a `manual` entry
/// keeps every field the operator typed; only `synced_at` moves.
fn merge(p: &mut Provider, fetched: Vec<Fetched>, now: Millis) -> (usize, usize) {
    let mut added = 0usize;
    let mut refreshed = 0usize;
    for f in fetched {
        match p.models.iter_mut().find(|m| m.id == f.id) {
            Some(existing) => {
                if existing.source.is_overwritable_by_sync() {
                    if f.label.is_some() {
                        existing.label = f.label;
                    }
                    if f.context_length.is_some() {
                        existing.context_length = f.context_length;
                    }
                    if f.max_output_tokens.is_some() {
                        existing.max_output_tokens = f.max_output_tokens;
                    }
                    if !f.capabilities.is_empty() {
                        existing.capabilities = f.capabilities;
                    }
                }
                existing.synced_at = Some(now);
                refreshed += 1;
            }
            None => {
                p.models.push(ModelEntry {
                    id: f.id,
                    source: ModelSource::Synced,
                    label: f.label,
                    context_length: f.context_length,
                    max_output_tokens: f.max_output_tokens,
                    capabilities: f.capabilities,
                    input_price_per_mtok: None,
                    output_price_per_mtok: None,
                    synced_at: Some(now),
                    hidden: false,
                });
                added += 1;
            }
        }
    }
    (added, refreshed)
}

#[derive(Debug)]
struct Summary {
    provider: String,
    total: usize,
    added: usize,
    refreshed: usize,
}

async fn sync_one(
    ctx: &Ctx,
    client: &net::HttpClient,
    cfg: &mut mr_core::config::Config,
    index: usize,
) -> Result<Summary, CliError> {
    let now = Millis::now();
    // Credential is optional here: plenty of local providers (`ollama`, `vllm`) expose /models
    // without auth. If one exists we attach it; if not we still ask.
    let secret = match credential(ctx, &cfg.providers[index]) {
        Ok(s) => Some(s),
        Err(e) if e.exit == crate::output::Exit::NotFound => None,
        Err(e) => return Err(e),
    };
    let p = &mut cfg.providers[index];
    let mut headers = Vec::new();
    let mut query = String::new();
    if let Some(secret) = &secret {
        match &p.auth {
            AuthKind::QueryParam { param } => {
                query = format!("?{param}={}", net::url_encode(secret));
            }
            _ => headers = auth_headers(p, secret)?,
        }
    }
    let base = p.base_url.trim_end_matches('/');
    let url = format!("{base}/models{query}");
    let pid = p.id.as_str().to_owned();

    let reply = net::request(
        client,
        http::Method::GET,
        &url,
        &headers,
        None,
        std::time::Duration::from_secs(20),
    )
    .await?;
    if !reply.status.is_success() {
        let mut e = CliError::failure(format!(
            "`{pid}` GET /models returned HTTP {}",
            reply.status.as_u16()
        ));
        let body: String = crate::output::redact_text(&reply.body)
            .chars()
            .take(300)
            .collect();
        if !body.trim().is_empty() {
            e = e.with_detail(body);
        }
        if matches!(reply.status.as_u16(), 401 | 403) {
            e = e.with_detail(format!(
                "the credential was rejected; check it with `mr provider test {pid}` \
                 or add a fresh one: `mr account add {pid} --secret-stdin`"
            ));
        }
        return Err(e);
    }

    let fetched = parse_models(&reply.body)?;
    let (added, refreshed) = merge(p, fetched, now);
    p.models_synced_at = Some(now);
    Ok(Summary {
        provider: pid,
        total: p.models.len(),
        added,
        refreshed,
    })
}

async fn sync(ctx: &Ctx, provider: Option<&str>, all: bool) -> Result<(), CliError> {
    let mut cfg = ctx.load()?;
    if all && provider.is_some() {
        return Err(CliError::usage("give a provider id or --all, not both"));
    }
    let targets: Vec<usize> = if all {
        let idx: Vec<usize> = cfg
            .providers
            .iter()
            .enumerate()
            .filter(|(_, p)| p.enabled)
            .map(|(i, _)| i)
            .collect();
        if idx.is_empty() {
            if cfg.providers.is_empty() {
                return Err(CliError::not_found(
                    "no providers configured; add one with `mr provider add openai`",
                ));
            }
            return Err(CliError::not_found(
                "no enabled providers to sync; enable one with `mr provider enable <id>`",
            ));
        }
        idx
    } else {
        let pid = provider.ok_or_else(|| {
            CliError::usage("name a provider: `mr models sync openai`, or pass --all")
        })?;
        let i = cfg
            .providers
            .iter()
            .position(|p| p.id.as_str() == pid)
            .ok_or_else(|| CliError::not_found(format!("no provider with id `{pid}`")))?;
        vec![i]
    };

    let client = net::client();
    let mut summaries = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    for i in targets {
        let pid = cfg.providers[i].id.as_str().to_owned();
        match sync_one(ctx, &client, &mut cfg, i).await {
            Ok(s) => summaries.push(s),
            Err(e) => {
                if !all {
                    // Single target: its failure is the command's failure, and nothing changed.
                    return Err(e);
                }
                failures.push(format!("{pid}: {}", e.message));
            }
        }
    }

    let mut human: Vec<String> = summaries
        .iter()
        .map(|s| {
            format!(
                "`{}`: {} models ({} added, {} refreshed)",
                s.provider, s.total, s.added, s.refreshed
            )
        })
        .collect();
    if all {
        for f in &failures {
            human.push(format!("FAILED {f}"));
        }
    }
    let skipped_disabled: Vec<String> = cfg
        .providers
        .iter()
        .filter(|p| !p.enabled)
        .map(|p| p.id.as_str().to_owned())
        .collect();
    if all && !skipped_disabled.is_empty() {
        human.push(format!(
            "skipped (disabled): {}",
            skipped_disabled.join(", ")
        ));
    }

    let doc = serde_json::json!({
        "synced": summaries
            .iter()
            .map(|s| serde_json::json!({
                "provider": s.provider,
                "models": s.total,
                "added": s.added,
                "refreshed": s.refreshed,
            }))
            .collect::<Vec<_>>(),
        "failed": failures,
    });
    let out = ctx.save(&cfg)?;
    ctx.finish(&out, human, doc)?;
    if !failures.is_empty() {
        return Err(CliError::failure(format!(
            "{} provider(s) failed to sync; the others were stored",
            failures.len()
        ))
        .with_details(failures));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_filter_accepts_serde_and_colloquial_spellings() {
        assert_eq!(parse_source("synced").unwrap(), ModelSource::Synced);
        assert_eq!(parse_source("sync").unwrap(), ModelSource::Synced);
        assert_eq!(parse_source("manual").unwrap(), ModelSource::Manual);
        assert_eq!(parse_source("hand").unwrap(), ModelSource::Manual);
        assert_eq!(parse_source("Inferred").unwrap(), ModelSource::Inferred);
        let err = parse_source("telepathy").unwrap_err();
        assert_eq!(err.exit, crate::output::Exit::Usage);
        assert!(err.message.contains("synced"), "{}", err.message);
    }

    #[test]
    fn parse_models_handles_openai_and_google_shapes() {
        let openai = parse_models(
            r#"{"object":"list","data":[
                {"id":"gpt-5.5","object":"model","capabilities":["chat"]},
                {"id":"o3-mini","context_length":200000,"max_output_tokens":100000}
            ]}"#,
        )
        .unwrap();
        assert_eq!(openai.len(), 2);
        assert_eq!(openai[0].id, "gpt-5.5");
        assert!(openai[0]
            .capabilities
            .contains(&mr_core::dialect::Capability::Chat));
        assert_eq!(openai[1].context_length, Some(200_000));
        assert_eq!(openai[1].max_output_tokens, Some(100_000));

        let google = parse_models(
            r#"{"models":[
                {"name":"models/gemini-2.5-pro","displayName":"Gemini 2.5 Pro","contextWindow":1048576}
            ]}"#,
        )
        .unwrap();
        // The `models/` prefix is stripped: the relay's model field wants the bare id.
        assert_eq!(google[0].id, "gemini-2.5-pro");
        assert_eq!(google[0].label.as_deref(), Some("Gemini 2.5 Pro"));
        assert_eq!(google[0].context_length, Some(1_048_576));

        let bare = parse_models(r#"["a","models/b"]"#).unwrap();
        assert_eq!(
            bare.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[test]
    fn parse_models_rejects_nonsense() {
        let err = parse_models("not json at all").unwrap_err();
        assert_eq!(err.exit, crate::output::Exit::Failure);
        let err = parse_models(r#"{"hello":"world"}"#).unwrap_err();
        assert!(err.message.contains("`data`"), "{}", err.message);
        let err = parse_models(r#"[{"object":"model"}]"#).unwrap_err();
        assert!(err.message.contains("neither an `id`"), "{}", err.message);
    }

    #[test]
    fn merge_respects_manual_entries_and_counts_changes() {
        let mut p = Provider::new("openai", mr_core::dialect::Dialect::OpenAi, "https://x/v1");
        p.models.push(ModelEntry {
            id: "gpt-5.5".into(),
            source: ModelSource::Manual,
            label: Some("my name for it".into()),
            context_length: Some(42),
            max_output_tokens: None,
            capabilities: Vec::new(),
            input_price_per_mtok: None,
            output_price_per_mtok: None,
            synced_at: None,
            hidden: false,
        });
        p.models.push(ModelEntry {
            id: "o3".into(),
            source: ModelSource::Synced,
            label: Some("old label".into()),
            context_length: None,
            max_output_tokens: None,
            capabilities: Vec::new(),
            input_price_per_mtok: None,
            output_price_per_mtok: None,
            synced_at: None,
            hidden: false,
        });
        let now = Millis::now();
        let (added, refreshed) = merge(
            &mut p,
            vec![
                Fetched {
                    id: "gpt-5.5".into(),
                    label: Some("provider label".into()),
                    context_length: Some(400_000),
                    max_output_tokens: None,
                    capabilities: Vec::new(),
                },
                Fetched {
                    id: "o3".into(),
                    label: Some("new label".into()),
                    context_length: Some(200_000),
                    max_output_tokens: None,
                    capabilities: Vec::new(),
                },
                Fetched {
                    id: "gpt-5.5-mini".into(),
                    label: None,
                    context_length: None,
                    max_output_tokens: None,
                    capabilities: Vec::new(),
                },
            ],
            now,
        );
        assert_eq!((added, refreshed), (1, 2));

        let manual = p.models.iter().find(|m| m.id == "gpt-5.5").unwrap();
        // Manual: untouched fields stay exactly as the operator typed them.
        assert_eq!(manual.label.as_deref(), Some("my name for it"));
        assert_eq!(manual.context_length, Some(42));
        assert_eq!(manual.synced_at, Some(now)); // but reconciliation is recorded

        let synced = p.models.iter().find(|m| m.id == "o3").unwrap();
        assert_eq!(synced.label.as_deref(), Some("new label"));
        assert_eq!(synced.context_length, Some(200_000));
        assert!(p.models.iter().any(|m| m.id == "gpt-5.5-mini"));
    }

    #[test]
    fn listing_without_stored_models_points_at_sync() {
        crate::output::testutil::with_config_dir("models-list-empty", |_dir| {
            let mut cfg = mr_core::config::Config::default();
            cfg.providers.push(Provider::new(
                "openai",
                mr_core::dialect::Dialect::OpenAi,
                "https://x/v1",
            ));
            cfg.save_default().unwrap();
            let ctx = Ctx { json: false };
            list(&ctx, Some("openai"), &None).unwrap();
            // --json half: an empty array, never an error.
            let ctx = Ctx { json: true };
            list(&ctx, Some("openai"), &None).unwrap();
        });
    }

    #[test]
    fn sync_usage_guards() {
        crate::output::testutil::with_config_dir("models-sync-guards", |_dir| {
            let ctx = Ctx { json: false };
            let err = crate::output::testutil::block_on(sync(&ctx, None, false)).unwrap_err();
            assert_eq!(err.exit, crate::output::Exit::Usage, "{err}");
            let err =
                crate::output::testutil::block_on(sync(&ctx, Some("openai"), true)).unwrap_err();
            assert_eq!(err.exit, crate::output::Exit::Usage, "{err}");
            let err =
                crate::output::testutil::block_on(sync(&ctx, Some("nope"), false)).unwrap_err();
            assert_eq!(err.exit, crate::output::Exit::NotFound, "{err}");
        });
    }
}
