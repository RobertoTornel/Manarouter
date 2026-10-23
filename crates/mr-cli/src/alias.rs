//! `mr alias` — alternative spellings that resolve to a real model or combo.
//!
//! The target check here is mr-core's own validation rule wearing a better error message: an
//! alias must point at a combo, a provider, or a bare model the config can actually reach.
//! Anything else never reaches disk (`Ctx::save` re-validates anyway, as a second net).

use clap::Subcommand;

use mr_core::model::Alias;

use crate::output::{print_json, say, CliError, Table};
use crate::Ctx;

#[derive(Subcommand, Debug)]
pub enum AliasCmd {
    /// List aliases: the spelling and where it points.
    List,
    /// Add a spelling: a combo name (or id), a provider id, or a bare model id.
    Add {
        /// What a client sends (`gpt-5`).
        from: String,
        /// What it resolves to (a combo name, or a bare model id the router can reach).
        to: String,
    },
    /// Remove an alias.
    Remove {
        /// The spelling to remove.
        from: String,
    },
}

/// Run an alias subcommand.
pub async fn run(cmd: AliasCmd, ctx: &Ctx) -> Result<(), CliError> {
    match cmd {
        AliasCmd::List => list(ctx),
        AliasCmd::Add { from, to } => add(ctx, &from, &to),
        AliasCmd::Remove { from } => remove(ctx, &from),
    }
}

/// Which kind of thing the alias points at, for the list table (never a guess: `stale` when the
/// target has gone stale, which is exactly what an operator needs to see).
///
/// `reachable` is `cfg.reachable_model_ids()` computed once by the caller: this runs per row,
/// and rebuilding the model list for every alias is wasteful on a large catalog.
fn kind_of(cfg: &mr_core::config::Config, reachable: &[String], to: &str) -> &'static str {
    if cfg.combo(to).is_some() {
        "combo"
    } else if cfg.provider(to).is_some() {
        "provider"
    } else if to
        .split_once('/')
        .is_some_and(|(p, _)| cfg.provider(p).is_some())
        || reachable.iter().any(|m| m == to)
    {
        "model"
    } else {
        "stale"
    }
}

fn list(ctx: &Ctx) -> Result<(), CliError> {
    let cfg = ctx.load()?;
    if ctx.json {
        return print_json(&cfg.aliases);
    }
    let reachable = cfg.reachable_model_ids();
    let mut t = Table::new(["FROM", "KIND", "TO"]);
    for a in &cfg.aliases {
        t.row(vec![
            a.from.clone(),
            kind_of(&cfg, &reachable, &a.to).to_owned(),
            a.to.clone(),
        ]);
    }
    t.print();
    if cfg.aliases.is_empty() {
        say("no aliases; `mr alias add gpt-5 gpt-5.5` teaches the router a short name");
    }
    Ok(())
}

fn add(ctx: &Ctx, from: &str, to: &str) -> Result<(), CliError> {
    let mut cfg = ctx.load()?;
    let (from, to) = (from.trim(), to.trim());
    if from.is_empty() {
        return Err(CliError::usage(
            "the alias spelling (`from`) cannot be empty",
        ));
    }
    if to.is_empty() {
        return Err(CliError::usage("the alias target (`to`) cannot be empty"));
    }

    // An alias whose spelling is already a callable name could never be reached: the router
    // resolves the real thing first. Refuse the ambiguity now rather than at request time.
    if cfg.combo(from).is_some() {
        return Err(CliError::validation(format!(
            "`{from}` is already a combo name; an alias with that spelling could never be reached"
        )));
    }
    if cfg.provider(from).is_some() {
        return Err(CliError::validation(format!(
            "`{from}` is already a provider id; an alias with that spelling could never be reached"
        )));
    }
    if let Some(existing) = cfg.aliases.iter().find(|a| a.from == from) {
        return Err(CliError::validation(format!(
            "alias `{from}` already points at `{}`; nothing was written",
            existing.to
        ))
        .with_detail(format!("replace it: `mr alias remove {from}` first")));
    }

    resolve_target(&cfg, to)?;

    cfg.aliases.push(Alias {
        from: from.to_owned(),
        to: to.to_owned(),
        generated: false,
    });
    let out = ctx.save(&cfg)?;
    ctx.finish(
        &out,
        vec![
            format!("alias `{from}` -> `{to}`"),
            format!("clients can now ask for `{from}`"),
        ],
        serde_json::json!({ "from": from, "to": to }),
    )
}

/// Is `to` something the router can resolve?
///
/// This mirrors mr-core's validation rule exactly — combo (id or name), provider id, or a bare
/// model id the config can actually reach — because anything looser here would pass this check
/// and then be refused by `Ctx::save`'s validation with a less helpful message. Notably,
/// qualified `provider/model` targets are **not** accepted by mr-core's validator today; when the
/// bare model exists, this says so with a `did you mean` hint instead of leaving the operator to
/// decode the validator's wording.
fn resolve_target(cfg: &mr_core::config::Config, to: &str) -> Result<(), CliError> {
    if cfg.combo(to).is_some() || cfg.provider(to).is_some() {
        return Ok(());
    }
    let reachable = cfg.reachable_model_ids();
    if reachable.iter().any(|m| m == to) {
        return Ok(());
    }

    let mut err = CliError::not_found(format!(
        "alias target `{to}` names neither a combo, a provider, nor a model"
    ));
    if let Some((provider, bare)) = to.split_once('/') {
        if cfg.provider(provider).is_some() && reachable.iter().any(|m| m == bare) {
            err = err.with_detail(format!(
                "config validation takes bare model ids, not qualified ones: did you mean `{bare}`?"
            ));
        }
    }
    let combos: Vec<&str> = cfg.combos.iter().map(|c| c.name.as_str()).collect();
    let providers: Vec<&str> = cfg.providers.iter().map(|p| p.id.as_str()).collect();
    Err(err
        .with_detail(format!(
            "known combos: {}",
            if combos.is_empty() {
                "(none)".to_owned()
            } else {
                combos.join(", ")
            }
        ))
        .with_detail(format!(
            "known providers: {}",
            if providers.is_empty() {
                "(none)".to_owned()
            } else {
                providers.join(", ")
            }
        ))
        .with_detail(format!("reachable models: {}", {
            let ids = reachable;
            if ids.is_empty() {
                "(none yet — sync a model list with `mr models sync <provider>`, or add a \
                     combo step)"
                    .to_owned()
            } else {
                let mut ids = ids;
                ids.truncate(8);
                ids.join(", ")
            }
        })))
}

fn remove(ctx: &Ctx, from: &str) -> Result<(), CliError> {
    let mut cfg = ctx.load()?;
    let from = from.trim();
    let position = cfg
        .aliases
        .iter()
        .position(|a| a.from == from)
        .ok_or_else(|| {
            let known: Vec<&str> = cfg.aliases.iter().map(|a| a.from.as_str()).collect();
            let mut e = CliError::not_found(format!("no alias `{from}`"));
            if known.is_empty() {
                e = e.with_detail("no aliases exist yet");
            } else {
                e = e.with_detail(format!("known aliases: {}", known.join(", ")));
            }
            e
        })?;
    let removed = cfg.aliases.remove(position);
    let out = ctx.save(&cfg)?;
    ctx.finish(
        &out,
        vec![format!(
            "alias `{}` -> `{}` removed",
            removed.from, removed.to
        )],
        serde_json::json!({ "from": removed.from, "to": removed.to, "removed": true }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::testutil::{block_on, with_config_dir};
    use crate::output::Exit;
    use mr_core::dialect::Dialect;
    use mr_core::model::{Combo, ComboStep, Provider};

    #[test]
    fn alias_add_checks_the_target_and_the_spelling() {
        with_config_dir("alias-targets", |_dir| {
            let mut cfg = mr_core::config::Config::default();
            cfg.providers
                .push(Provider::new("openai", Dialect::OpenAi, "https://x/v1"));
            let mut combo = Combo::new("Brains", ComboStep::new("openai", "gpt-5.5"));
            combo.steps.push(ComboStep::new("openai", "o3"));
            cfg.combos.push(combo);
            assert_eq!(
                kind_of(&cfg, &cfg.reachable_model_ids(), "gpt-5.5"),
                "model"
            );
            cfg.save_default().unwrap();

            let ctx = Ctx { json: false };

            // A combo name is a valid target...
            block_on(run(
                AliasCmd::Add {
                    from: "smart".into(),
                    to: "Brains".into(),
                },
                &ctx,
            ))
            .unwrap();
            // ...and so is a bare model id the config can reach (via the combo's step).
            block_on(run(
                AliasCmd::Add {
                    from: "g5".into(),
                    to: "gpt-5.5".into(),
                },
                &ctx,
            ))
            .unwrap();

            // A target that names nothing real: exit 4 with the known lists.
            let err = block_on(run(
                AliasCmd::Add {
                    from: "nope".into(),
                    to: "ghost/nowhere".into(),
                },
                &ctx,
            ))
            .unwrap_err();
            assert_eq!(err.exit, Exit::NotFound, "{err}");
            assert!(err.details.iter().any(|d| d.contains("openai")));

            // A qualified target is refused with a did-you-mean, mirroring mr-core's validator.
            let err = block_on(run(
                AliasCmd::Add {
                    from: "g5q".into(),
                    to: "openai/gpt-5.5".into(),
                },
                &ctx,
            ))
            .unwrap_err();
            assert_eq!(err.exit, Exit::NotFound, "{err}");
            assert!(
                err.details
                    .iter()
                    .any(|d| d.contains("did you mean `gpt-5.5`")),
                "{:?}",
                err.details
            );

            // Duplicate spelling: refused, and the original target survives.
            let err = block_on(run(
                AliasCmd::Add {
                    from: "g5".into(),
                    to: "Brains".into(),
                },
                &ctx,
            ))
            .unwrap_err();
            assert_eq!(err.exit, Exit::Validation, "{err}");

            // Spelling equal to a combo name would shadow the combo.
            let err = block_on(run(
                AliasCmd::Add {
                    from: "Brains".into(),
                    to: "gpt-5.5".into(),
                },
                &ctx,
            ))
            .unwrap_err();
            assert_eq!(err.exit, Exit::Validation, "{err}");

            let cfg = mr_core::config::Config::load_default().unwrap();
            assert_eq!(cfg.aliases.len(), 2);
        });
    }

    #[test]
    fn alias_remove_is_not_found_then_works() {
        with_config_dir("alias-remove", |_dir| {
            let mut cfg = mr_core::config::Config::default();
            cfg.providers
                .push(Provider::new("openai", Dialect::OpenAi, "https://x/v1"));
            cfg.save_default().unwrap();
            let ctx = Ctx { json: false };
            block_on(run(
                AliasCmd::Add {
                    from: "gpt-5".into(),
                    to: "openai".into(),
                },
                &ctx,
            ))
            .unwrap();

            let err = block_on(run(
                AliasCmd::Remove {
                    from: "ghost".into(),
                },
                &ctx,
            ))
            .unwrap_err();
            assert_eq!(err.exit, Exit::NotFound, "{err}");

            block_on(run(
                AliasCmd::Remove {
                    from: "gpt-5".into(),
                },
                &ctx,
            ))
            .unwrap();
            assert!(mr_core::config::Config::load_default()
                .unwrap()
                .aliases
                .is_empty());
        });
    }

    #[test]
    fn list_reports_stale_targets_honestly() {
        with_config_dir("alias-list-stale", |_dir| {
            let mut cfg = mr_core::config::Config::default();
            cfg.aliases.push(Alias {
                from: "a".into(),
                to: "gone".into(),
                generated: false,
            });
            cfg.save_default().unwrap();
            let ctx = Ctx { json: false };
            list(&ctx).unwrap();
            let cfg = mr_core::config::Config::load_default().unwrap();
            let reachable = cfg.reachable_model_ids();
            assert_eq!(kind_of(&cfg, &reachable, "gone"), "stale");
            assert_eq!(kind_of(&cfg, &reachable, "a"), "stale");
        });
    }
}
