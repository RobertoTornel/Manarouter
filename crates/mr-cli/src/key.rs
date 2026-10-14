//! `mr key` — client keys the relay accepts from callers.
//!
//! The plaintext of a key exists for exactly one moment: the output of `key create`. It is hashed
//! (SHA-256, lowercase hex — the exact format the relay's `KeyEntry::from_config` verifies) into
//! config.json and never stored or printed again — `key list` shows id, label, prefix and
//! permissions only, and its `--json` output strips `key_hash` the way the panel API does.
//!
//! The mint format (`mr-` + 40 lowercase hex chars from 25 random bytes, prefix = the first seven
//! characters) mirrors `manarouter-conductor`'s `generate_client_key`, so a key minted by the CLI
//! and one minted by the panel are indistinguishable to the relay.

use clap::Subcommand;

use mr_core::config::Config;
use mr_core::dialect::RouteKind;
use mr_core::ids::ClientKeyId;
use mr_core::model::ClientKey;

use crate::output::{all_labels, parse_choice, print_json, say, CliError, Table};
use crate::Ctx;

#[derive(Subcommand, Debug)]
pub enum KeyCmd {
    /// Create a client key. The plaintext is printed once (bare on stdout with --print).
    Create(CreateArgs),
    /// List keys: id, label, prefix, permissions. Never a key or its hash.
    List,
    /// Re-enable a disabled key.
    Enable {
        /// Key id.
        id: String,
    },
    /// Disable a key (rejected at authentication, before routing).
    Disable {
        /// Key id.
        id: String,
    },
    /// Delete a key from the configuration.
    Remove {
        /// Key id.
        id: String,
    },
}

#[derive(clap::Args, Debug)]
pub struct CreateArgs {
    /// Human label, for example `my-editor`.
    #[arg(long)]
    pub label: Option<String>,
    /// Print only the bare key on stdout (for `$(...)` capture); the confirmation text moves to
    /// stderr. Without it the key is printed once inside a human-readable block.
    #[arg(long)]
    pub print: bool,
    /// Restrict to these model ids/patterns (comma-separated or repeated).
    #[arg(long = "allow-models", value_name = "PATTERNS")]
    pub allow_models: Vec<String>,
    /// Restrict to these routes (`chat.completions`, ...).
    #[arg(long = "allow-routes", value_name = "ROUTES")]
    pub allow_routes: Vec<String>,
    /// Per-key request budget (0 = unlimited).
    #[arg(long, value_name = "N")]
    pub limit_requests: Option<u64>,
    /// Descriptive note.
    #[arg(long)]
    pub note: Option<String>,
}

/// Mint a client key: `mr-` + 40 lowercase hex characters (25 random bytes).
///
/// Cross-crate format: the panel mints exactly this shape, and the relay only ever sees the
/// SHA-256 of whatever string this produced.
pub(crate) fn generate_client_key() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 25];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("mr-{hex}")
}

/// Lowercase SHA-256 hex — the format the relay accepts (exactly 64 hex characters) and
/// `hash_presented` recomputes on every request.
pub(crate) fn sha256_hex(text: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The first seven characters, stored so two keys can be told apart without holding either.
fn key_prefix(plaintext: &str) -> String {
    plaintext.chars().take(7).collect()
}

/// Run a key subcommand.
pub async fn run(cmd: KeyCmd, ctx: &Ctx) -> Result<(), CliError> {
    match cmd {
        KeyCmd::Create(a) => create(ctx, a),
        KeyCmd::List => list(ctx),
        KeyCmd::Enable { id } => set_enabled(ctx, &id, true),
        KeyCmd::Disable { id } => set_enabled(ctx, &id, false),
        KeyCmd::Remove { id } => remove(ctx, &id),
    }
}

// ---------------------------------------------------------------------------
// create
// ---------------------------------------------------------------------------

/// Split comma-separated flag values into a de-duplicated list.
fn split_patterns(values: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for spec in values {
        for piece in spec.split(',') {
            let piece = piece.trim();
            if !piece.is_empty() && !out.iter().any(|x| x == piece) {
                out.push(piece.to_owned());
            }
        }
    }
    out
}

/// `--allow-routes` spellings: the serde kebab form first (that is what `parse_choice` offers as
/// the documented choice list), then the dotted `RouteKind::as_str` form the relay's `permits`
/// actually compares against, then the obvious colloquialisms.
fn parse_route(raw: &str) -> Option<RouteKind> {
    let lower = raw.trim().to_ascii_lowercase();
    RouteKind::ALL
        .iter()
        .copied()
        .find(|r| r.as_str() == lower)
        .or_else(|| match lower.replace(['_', ' '], "-").as_str() {
            "chat" | "chat-completions" | "completions" => Some(RouteKind::ChatCompletions),
            "response" | "responses" | "responses-api" => Some(RouteKind::Responses),
            "embedding" | "embeddings" => Some(RouteKind::Embeddings),
            "audio-transcription" | "audio-transcriptions" | "transcription" | "transcriptions" => {
                Some(RouteKind::AudioTranscription)
            }
            "audio-speech" | "speech" | "tts" => Some(RouteKind::AudioSpeech),
            "image" | "images" | "image-generation" | "images-generation" => {
                Some(RouteKind::ImagesGenerations)
            }
            "model" | "models" => Some(RouteKind::Models),
            "message" | "messages" => Some(RouteKind::Messages),
            "count-tokens" | "messages-count-tokens" => Some(RouteKind::CountTokens),
            _ => None,
        })
}

/// `--allow-models` entries must name something real (or be a glob): a restriction that
/// accidentally matches nothing locks the key out silently, which is worse than refusing it.
fn check_allow_models(cfg: &Config, patterns: &[String]) -> Result<(), CliError> {
    if patterns.is_empty() {
        return Ok(());
    }
    let reachable_ids = cfg.reachable_model_ids();
    let reachable_qualified = cfg.reachable_models();
    let unknown: Vec<String> = patterns
        .iter()
        .filter(|p| {
            if p.contains('*') || p.contains('?') {
                return false; // a glob is a deliberate pattern, not a name to verify
            }
            !(cfg.combo(p).is_some()
                || cfg.provider(p).is_some()
                || reachable_ids.iter().any(|m| m == *p)
                || reachable_qualified.iter().any(|m| m == *p))
        })
        .cloned()
        .collect();
    if unknown.is_empty() {
        Ok(())
    } else {
        Err(CliError::validation(format!(
            "--allow-models names nothing this router can reach: {}",
            unknown.join(", ")
        ))
        .with_detail(
            "entries must be a combo, a provider, a model the config knows, or a glob like \
             `gpt-*`",
        ))
    }
}

/// Build the key record and its plaintext without touching the config. Everything that could be
/// refused (route spellings, unreachable model patterns) is refused here, so `create` either
/// validates everything up front or writes nothing.
fn build_key(cfg: &Config, args: &CreateArgs) -> Result<(ClientKey, String), CliError> {
    let allow_models = split_patterns(&args.allow_models);
    check_allow_models(cfg, &allow_models)?;

    let mut allow_routes: Vec<String> = Vec::new();
    for spec in &args.allow_routes {
        for piece in spec.split(',') {
            let piece = piece.trim();
            if piece.is_empty() {
                continue;
            }
            let route: RouteKind = parse_choice(
                "--allow-routes",
                piece,
                &all_labels(&RouteKind::ALL),
                parse_route,
            )?;
            if !allow_routes.iter().any(|r| r == route.as_str()) {
                allow_routes.push(route.as_str().to_owned());
            }
        }
    }

    let plaintext = generate_client_key();
    let key = ClientKey {
        id: ClientKeyId::generate(),
        label: args.label.clone().unwrap_or_else(|| "default".to_owned()),
        key_hash: sha256_hex(&plaintext),
        prefix: Some(key_prefix(&plaintext)),
        enabled: true,
        allow_models,
        allow_routes,
        limit_requests: args.limit_requests.unwrap_or(0),
        note: args.note.clone(),
    };
    Ok((key, plaintext))
}

/// `key list`'s view of one key: everything the human table shows, `key_hash` removed — the same
/// redaction `manarouter-conductor`'s GET view performs. No key, no hash, no way.
fn sanitized(key: &ClientKey) -> serde_json::Value {
    let mut v = serde_json::to_value(key).unwrap_or(serde_json::Value::Null);
    if let Some(obj) = v.as_object_mut() {
        obj.remove("key_hash");
    }
    v
}

fn create(ctx: &Ctx, args: CreateArgs) -> Result<(), CliError> {
    let mut cfg = ctx.load()?;
    let (key, plaintext) = build_key(&cfg, &args)?;
    let label = key.label.clone();
    let key_json = sanitized(&key);
    cfg.client_keys.push(key);
    let out = ctx.save(&cfg)?;

    if ctx.json {
        // The one moment plaintext exists, so it belongs in this document: id, prefix, permissions
        // and the plaintext itself, all fields of the key just created.
        let mut doc = key_json;
        if let Some(obj) = doc.as_object_mut() {
            obj.insert("plaintext".into(), serde_json::Value::String(plaintext));
        }
        return ctx.finish(&out, Vec::new(), doc);
    }
    if args.print {
        // Bare stdout — exactly what `$(mr key create --print)` captures. Everything else the
        // operator should know goes to stderr so the capture stays clean.
        println!("{plaintext}");
        eprintln!(
            "key `{label}` stored; config {}, snapshot {}",
            out.config.display(),
            out.snapshot.display()
        );
        eprintln!("the key above is shown once and never again");
        return Ok(());
    }
    ctx.finish(
        &out,
        vec![
            format!("{plaintext} (shown once, never again)"),
            format!(
                "key `{label}` stored; config {}, snapshot {}",
                out.config.display(),
                out.snapshot.display()
            ),
        ],
        doc_root(&key_json, &plaintext),
    )
}

/// The `--json` document for a successful create: the redacted key plus its one-time plaintext.
fn doc_root(key_json: &serde_json::Value, plaintext: &str) -> serde_json::Value {
    let mut doc = key_json.clone();
    if let Some(obj) = doc.as_object_mut() {
        obj.insert(
            "plaintext".into(),
            serde_json::Value::String(plaintext.to_owned()),
        );
    }
    doc
}

// ---------------------------------------------------------------------------
// list / enable / disable / remove
// ---------------------------------------------------------------------------

fn list(ctx: &Ctx) -> Result<(), CliError> {
    let cfg = ctx.load()?;
    if ctx.json {
        let rows: Vec<serde_json::Value> = cfg.client_keys.iter().map(sanitized).collect();
        return print_json(&rows);
    }
    let mut t = Table::new([
        "ID", "LABEL", "PREFIX", "MODELS", "ROUTES", "LIMIT", "ENABLED",
    ]);
    for k in &cfg.client_keys {
        t.row(vec![
            k.id.as_str().to_owned(),
            k.label.clone(),
            k.prefix.clone().unwrap_or_default(),
            if k.allow_models.is_empty() {
                "(all)".to_owned()
            } else {
                k.allow_models.join(", ")
            },
            if k.allow_routes.is_empty() {
                "(all)".to_owned()
            } else {
                k.allow_routes.join(", ")
            },
            if k.limit_requests == 0 {
                "unlimited".to_owned()
            } else {
                k.limit_requests.to_string()
            },
            if k.enabled { "yes" } else { "no" }.to_owned(),
        ]);
    }
    t.print();
    if cfg.client_keys.is_empty() {
        say("no client keys; `mr key create --label my-editor` mints one (printed once)");
    }
    Ok(())
}

/// A key by id first, then by label (labels are not unique; the first match wins and the id is
/// what scripts should use).
fn find_key<'a>(cfg: &'a Config, id: &str) -> Result<&'a ClientKey, CliError> {
    cfg.client_keys
        .iter()
        .find(|k| k.id.as_str() == id)
        .or_else(|| cfg.client_keys.iter().find(|k| k.label == id))
        .ok_or_else(|| {
            let known: Vec<&str> = cfg.client_keys.iter().map(|k| k.label.as_str()).collect();
            let mut e = CliError::not_found(format!("no client key `{id}`"));
            if known.is_empty() {
                e = e.with_detail("no keys exist yet; create one with `mr key create`");
            } else {
                e = e.with_detail(format!("labels: {}", known.join(", ")));
            }
            e
        })
}

fn find_key_mut(cfg: &mut Config, id: &str) -> Result<(usize, String), CliError> {
    let position = cfg
        .client_keys
        .iter()
        .position(|k| k.id.as_str() == id)
        .or_else(|| cfg.client_keys.iter().position(|k| k.label == id))
        .ok_or_else(|| {
            let known: Vec<&str> = cfg.client_keys.iter().map(|k| k.label.as_str()).collect();
            let mut e = CliError::not_found(format!("no client key `{id}`"));
            if known.is_empty() {
                e = e.with_detail("no keys exist yet; create one with `mr key create`");
            } else {
                e = e.with_detail(format!("labels: {}", known.join(", ")));
            }
            e
        })?;
    Ok((position, cfg.client_keys[position].id.as_str().to_owned()))
}

fn set_enabled(ctx: &Ctx, id: &str, enabled: bool) -> Result<(), CliError> {
    let mut cfg = ctx.load()?;
    let (position, key_id) = find_key_mut(&mut cfg, id)?;
    cfg.client_keys[position].enabled = enabled;
    let label = cfg.client_keys[position].label.clone();
    let out = ctx.save(&cfg)?;
    let state = if enabled { "enabled" } else { "disabled" };
    ctx.finish(
        &out,
        vec![format!("key `{label}` ({key_id}) {state}")],
        serde_json::json!({ "id": key_id, "label": label, "enabled": enabled }),
    )
}

fn remove(ctx: &Ctx, id: &str) -> Result<(), CliError> {
    let mut cfg = ctx.load()?;
    // Read what the caller asked for before removing it, so the report names the real key even
    // when they typed a label.
    let label = find_key(&cfg, id)?.label.clone();
    let (position, key_id) = find_key_mut(&mut cfg, id)?;
    cfg.client_keys.remove(position);
    let out = ctx.save(&cfg)?;
    ctx.finish(
        &out,
        vec![format!(
            "key `{label}` ({key_id}) removed; callers holding it are refused from now on"
        )],
        serde_json::json!({ "id": key_id, "label": label, "removed": true }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::testutil::with_config_dir;
    use crate::output::Exit as E;

    /// The relay verifies lowercase SHA-256 hex; this is the format contract.
    #[test]
    fn sha256_hex_is_the_documented_format() {
        // Known-answer test for SHA-256("abc").
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(sha256_hex("mr-00").len(), 64);
        assert!(sha256_hex("mr-00")
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)));
    }

    #[test]
    fn minted_keys_match_the_conductor_shape() {
        let k1 = generate_client_key();
        let k2 = generate_client_key();
        assert!(k1.starts_with("mr-"), "{k1}");
        // 25 random bytes, hex-encoded: 200 bits of entropy. `generate_client_key` chooses
        // the width; nothing downstream parses a fixed length, so this pins the format the
        // code promises rather than a number the test invented.
        assert_eq!(k1.len(), 3 + 50, "{k1}");
        assert!(k1[3..].chars().all(|c| c.is_ascii_hexdigit()), "{k1}");
        assert_ne!(k1, k2, "two mints must differ");
        assert_eq!(key_prefix(&k1), k1.chars().take(7).collect::<String>());
    }

    #[test]
    fn sanitized_strips_the_hash_but_keeps_the_prefix() {
        let key = ClientKey {
            id: ClientKeyId::generate(),
            label: "editor".into(),
            key_hash: "deadbeef".repeat(8),
            prefix: Some("mr-abc".into()),
            enabled: true,
            allow_models: vec!["gpt-5.5".into()],
            allow_routes: vec![],
            limit_requests: 0,
            note: None,
        };
        let v = sanitized(&key);
        assert!(v.get("key_hash").is_none(), "{v}");
        assert_eq!(v["prefix"], "mr-abc");
        assert_eq!(v["label"], "editor");
        let text = v.to_string();
        assert!(!text.contains("deadbeef"));
    }

    #[test]
    fn route_spellings_store_the_dotted_form_the_relay_compares() {
        let cfg = Config::default();
        let args = CreateArgs {
            label: None,
            print: false,
            allow_models: vec![],
            allow_routes: vec!["chat.completions".into(), "responses".into(), "chat".into()],
            limit_requests: None,
            note: None,
        };
        let (key, plaintext) = build_key(&cfg, &args).unwrap();
        // `chat` and `chat.completions` are the same route, said twice; `permits` compares the
        // dotted form, so that is what gets stored.
        assert_eq!(key.allow_routes, vec!["chat.completions", "responses"]);
        assert_eq!(
            key.prefix.as_deref(),
            Some(&plaintext.chars().take(7).collect::<String>()[..])
        );
        assert_eq!(key.key_hash, sha256_hex(&plaintext));
        assert!(key.enabled);

        // An unknown route is a usage error listing the choices.
        let args = CreateArgs {
            allow_routes: vec!["telepathy".into()],
            ..args
        };
        let err = build_key(&cfg, &args).unwrap_err();
        assert_eq!(err.exit, E::Usage, "{err}");
        assert!(err.message.contains("chat-completions"), "{}", err.message);
    }

    #[test]
    fn unreachable_allow_models_is_refused() {
        let cfg = Config::default();
        let args = CreateArgs {
            label: Some("editor".into()),
            print: false,
            allow_models: vec!["ghost-model".into()],
            allow_routes: vec![],
            limit_requests: None,
            note: None,
        };
        let err = build_key(&cfg, &args).unwrap_err();
        assert_eq!(err.exit, E::Validation, "{err}");
        assert!(err.message.contains("ghost-model"), "{}", err.message);

        // Globs are deliberate patterns, not names to verify; commas split; repeats de-duplicate.
        let args = CreateArgs {
            label: None,
            print: false,
            allow_models: vec!["gpt-*".into(), "o3-*, o4-*".into(), "gpt-*".into()],
            allow_routes: vec![],
            limit_requests: None,
            note: None,
        };
        let (key, _) = build_key(&cfg, &args).unwrap();
        assert_eq!(key.allow_models, vec!["gpt-*", "o3-*", "o4-*"]);
    }

    #[test]
    fn stored_config_holds_the_hash_and_never_the_plaintext() {
        with_config_dir("key-storage", |_dir| {
            let cfg = Config::default();
            let (key, plaintext) = build_key(
                &cfg,
                &CreateArgs {
                    label: Some("editor".into()),
                    print: false,
                    allow_models: vec![],
                    allow_routes: vec![],
                    limit_requests: None,
                    note: None,
                },
            )
            .unwrap();
            let mut cfg = cfg;
            cfg.client_keys.push(key);
            cfg.save_default().unwrap();

            let text = std::fs::read_to_string(mr_core::config::config_path()).unwrap();
            // Assert both halves: the hash is there, the plaintext is not.
            let stored = &Config::load_default().unwrap().client_keys[0];
            assert!(text.contains(&stored.key_hash), "hash must be stored");
            assert!(!text.contains(&plaintext), "plaintext must never be stored");
            assert!(text.contains(stored.id.as_str()), "id must be stored");
        });
    }

    #[test]
    fn create_end_to_end_puts_the_hash_in_config_and_never_the_plaintext() {
        with_config_dir("key-create", |_dir| {
            let ctx = Ctx { json: false };
            let args = CreateArgs {
                label: Some("editor".into()),
                print: false,
                allow_models: vec![],
                allow_routes: vec!["chat".into()],
                limit_requests: Some(500),
                note: Some("coding".into()),
            };
            create(&ctx, args).unwrap();

            let cfg = Config::load_default().unwrap();
            assert_eq!(cfg.client_keys.len(), 1);
            let k = &cfg.client_keys[0];
            assert_eq!(k.label, "editor");
            assert_eq!(k.limit_requests, 500);
            assert_eq!(k.allow_routes, vec!["chat.completions"]);
            // The stored hash is a full 64-char hex string, never plaintext.
            assert_eq!(k.key_hash.len(), 64);
            assert!(!k.key_hash.starts_with("mr-"));
            assert!(
                k.key_hash.chars().all(|c| c.is_ascii_hexdigit()),
                "the stored hash must be hex, so it cannot be the key itself: {}",
                k.key_hash
            );

            // list runs (human + json) and never errors.
            list(&ctx).unwrap();
            let ctx = Ctx { json: true };
            list(&ctx).unwrap();
        });
    }

    #[test]
    fn enable_disable_remove_exit_codes() {
        with_config_dir("key-lifecycle", |_dir| {
            let ctx = Ctx { json: false };
            let (key, _) = {
                let mut cfg = Config::load_default().unwrap();
                let built = build_key(
                    &cfg,
                    &CreateArgs {
                        label: Some("editor".into()),
                        print: false,
                        allow_models: vec![],
                        allow_routes: vec![],
                        limit_requests: None,
                        note: None,
                    },
                )
                .unwrap();
                cfg.client_keys.push(built.0.clone());
                cfg.save_default().unwrap();
                built
            };

            set_enabled(&ctx, key.id.as_str(), false).unwrap();
            assert!(!Config::load_default().unwrap().client_keys[0].enabled);
            set_enabled(&ctx, "editor", true).unwrap();
            assert!(Config::load_default().unwrap().client_keys[0].enabled);

            let err = set_enabled(&ctx, "ghost", false).unwrap_err();
            assert_eq!(err.exit, E::NotFound, "{err}");

            remove(&ctx, "editor").unwrap();
            assert!(Config::load_default().unwrap().client_keys.is_empty());
            let err = remove(&ctx, "editor").unwrap_err();
            assert_eq!(err.exit, E::NotFound, "{err}");
        });
    }
}
