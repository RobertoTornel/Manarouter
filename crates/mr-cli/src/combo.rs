//! `mr combo` — named model routes, and the ordered steps inside them.
//!
//! The operator's step order is authoritative ([`mr_core::model::Combo`'s own docs say so]):
//! this module never sorts steps, and `combo step move` is the only way to reorder them.
//! Bounds problems on step indices are usage errors (the operator is pointing at a place that
//! does not exist in the command itself); removing the last step is refused as a validation
//! problem, because a combo with no routable steps cannot compile into a snapshot.

use clap::Subcommand;

use mr_core::config::Config;
use mr_core::dialect::{ComboStrategy, MidstreamPolicy};
use mr_core::model::{Combo, ComboStep};

use crate::output::{
    all_labels, parse_choice, print_json, require_combo, require_provider, say, warn, CliError,
    Table,
};
use crate::Ctx;

#[derive(Subcommand, Debug)]
pub enum ComboCmd {
    /// Add a combo (`combo create` is an alias; the name can be positional or --name).
    #[command(alias = "create")]
    Add(AddArgs),
    /// List combos: names, ids, strategies, step counts.
    List,
    /// Show one combo with its numbered steps.
    Show {
        /// Combo name or id.
        name: String,
    },
    /// Remove a combo.
    Remove {
        /// Combo name or id.
        name: String,
        /// Required when client keys or aliases reference this combo by name.
        #[arg(long)]
        force: bool,
    },
    /// Re-enable a disabled combo.
    Enable {
        /// Combo name or id.
        name: String,
    },
    /// Disable a combo without deleting it.
    Disable {
        /// Combo name or id.
        name: String,
    },
    /// Edit a combo's steps: add, remove or move them.
    #[command(subcommand)]
    Step(StepCmd),
}

#[derive(clap::Args, Debug)]
pub struct AddArgs {
    /// Combo name (positional). `--name` says the same thing.
    #[arg(value_name = "NAME", required_unless_present = "name")]
    pub positional: Option<String>,
    /// Combo name, when you prefer a flag to the positional.
    #[arg(long)]
    pub name: Option<String>,
    /// Add a step, `provider/model` (repeat to build a chain).
    #[arg(long = "step", value_name = "PROVIDER/MODEL")]
    pub step: Vec<String>,
    /// Selection strategy: priority, round-robin,
    /// reset-aware-priority (default: priority).
    #[arg(long = "strategy", value_name = "STRATEGY")]
    pub strategy: Option<String>,
    /// What to do once bytes are on the wire: restart, resume,
    /// terminate (default: restart).
    #[arg(long = "midstream-policy", value_name = "POLICY")]
    pub midstream_policy: Option<String>,
    /// Hide the combo from model listings.
    #[arg(long)]
    pub hidden: bool,
    /// Race the first steps and take the fastest good answer.
    #[arg(long)]
    pub hedge: bool,
    /// Comma-separated step indices that race (with --hedge).
    #[arg(long, value_name = "0,1,2")]
    pub participants: Option<String>,
    /// Hedging eligibility window in milliseconds (with --hedge).
    #[arg(long, value_name = "MS")]
    pub window_ms: Option<u64>,
}

impl AddArgs {
    /// The combo name from whichever spelling was used. clap guarantees at least one is present
    /// (`positional` is `required_unless_present = "name"`).
    pub fn resolved_name(&self) -> Option<&str> {
        self.positional.as_deref().or(self.name.as_deref())
    }
}

#[derive(Subcommand, Debug)]
pub enum StepCmd {
    /// Append (or insert with --at) one `provider/model` step.
    Add {
        /// Combo name or id.
        name: String,
        /// The step, `provider/model`.
        #[arg(long = "step", value_name = "PROVIDER/MODEL")]
        step: String,
        /// Insert position instead of appending (0-based).
        #[arg(long, value_name = "INDEX")]
        at: Option<usize>,
    },
    /// Delete one step by index.
    Remove {
        /// Combo name or id.
        name: String,
        /// Step index, as printed by `combo show`.
        index: usize,
    },
    /// Move one step from index `from` to index `to` (`combo step move <name> <from> <to>`).
    Move {
        /// Combo name or id.
        name: String,
        /// Step index to move.
        from: usize,
        /// Index to move it to.
        to: usize,
    },
}

/// `MidstreamPolicy` has no `ALL` const; held honest by `midstream_choices_match_serde`.
const MIDSTREAM_CHOICES: [&str; 2] = ["restart", "terminate"];

/// Run a combo subcommand.
pub async fn run(cmd: ComboCmd, ctx: &Ctx) -> Result<(), CliError> {
    match cmd {
        ComboCmd::Add(a) => add(ctx, a),
        ComboCmd::List => list(ctx),
        ComboCmd::Show { name } => show(ctx, &name),
        ComboCmd::Remove { name, force } => remove(ctx, &name, force),
        ComboCmd::Enable { name } => set_enabled(ctx, &name, true),
        ComboCmd::Disable { name } => set_enabled(ctx, &name, false),
        ComboCmd::Step(step) => step_cmd(ctx, step),
    }
}

// ---------------------------------------------------------------------------
// steps
// ---------------------------------------------------------------------------

/// Split `provider/model` on the first slash only: model ids legitimately contain slashes
/// (`accounts/.../models/...` on Google-style endpoints).
fn parse_step(spec: &str) -> Result<(String, String), CliError> {
    let (provider, model) = spec.split_once('/').ok_or_else(|| {
        CliError::usage(format!(
            "--step expects `provider/model`, got `{spec}` (no `/`)"
        ))
    })?;
    let (provider, model) = (provider.trim(), model.trim());
    if provider.is_empty() || model.is_empty() {
        return Err(CliError::usage(format!(
            "--step expects `provider/model`, got `{spec}`"
        )));
    }
    Ok((provider.to_owned(), model.to_owned()))
}

/// Parse a step against the current config: the provider must exist, or the combo would compile
/// to a step that can never run (mr-core reports that as `unknown provider` much later).
fn build_step(cfg: &Config, spec: &str) -> Result<ComboStep, CliError> {
    let (provider, model) = parse_step(spec)?;
    let p = require_provider(cfg, &provider)?;
    if !p.enabled {
        warn(format!(
            "provider `{provider}` is disabled; the step will not be routable until it is enabled"
        ));
    }
    Ok(ComboStep::new(&provider, &model))
}

/// `--participants 0,1,2` — explicit step indices, in priority order.
fn parse_participants(raw: &str, steps: usize) -> Result<Vec<u16>, CliError> {
    let mut out: Vec<u16> = Vec::new();
    for piece in raw.split(',') {
        let piece = piece.trim();
        if piece.is_empty() {
            continue;
        }
        let n: u16 = piece.parse().map_err(|_| {
            CliError::usage(format!(
                "--participants expects comma-separated step indices, got `{raw}`"
            ))
        })?;
        if n as usize >= steps {
            return Err(CliError::validation(format!(
                "participant index {n} is out of range: the combo has {steps} step(s), \
                 so the valid indices are 0..{}",
                steps.saturating_sub(1)
            )));
        }
        if out.contains(&n) {
            return Err(CliError::usage(format!(
                "participant index {n} appears twice"
            )));
        }
        out.push(n);
    }
    if out.is_empty() {
        return Err(CliError::usage(
            "--participants needs at least one step index, for example --participants 0,1",
        ));
    }
    Ok(out)
}

/// Position of a combo by name or id, with `require_combo`'s not-found error.
fn combo_position(cfg: &Config, name: &str) -> Result<usize, CliError> {
    require_combo(cfg, name)?;
    cfg.combos
        .iter()
        .position(|c| c.name == name || c.id.as_str() == name)
        .ok_or_else(|| CliError::not_found(format!("no combo named `{name}`")))
}

// ---------------------------------------------------------------------------
// add / list / show / remove / enable / disable
// ---------------------------------------------------------------------------

fn add(ctx: &Ctx, args: AddArgs) -> Result<(), CliError> {
    let mut cfg = ctx.load()?;

    // Two spellings, one opinion: if both are given they must agree, then `resolved_name`
    // (positional first, --name as fallback) is the name.
    let name: String = match (args.positional.as_deref(), args.name.as_deref()) {
        (Some(p), Some(n)) if p != n => {
            return Err(CliError::usage(format!(
                "combo name given twice and differently: `{p}` and `{n}`"
            )));
        }
        _ => args
            .resolved_name()
            .ok_or_else(|| {
                CliError::usage("a combo needs a name: `mr combo add <name> --step provider/model`")
            })?
            .to_owned(),
    };
    if name.is_empty() {
        return Err(CliError::usage("a combo name cannot be empty"));
    }
    if cfg.combo(&name).is_some() {
        return Err(CliError::validation(format!(
            "combo `{name}` already exists; nothing was written"
        ))
        .with_detail(format!("inspect it with `mr combo show {name}`")));
    }
    if args.step.is_empty() {
        return Err(CliError::usage(format!(
            "combo `{name}` needs at least one --step provider/model"
        ))
        .with_detail("for example: --step openai/gpt-5.5 --step anthropic/claude-sonnet-4-5"));
    }

    // Build every step first: one unknown provider means nothing is written at all.
    let mut specs = Vec::with_capacity(args.step.len());
    for spec in &args.step {
        specs.push(build_step(&cfg, spec)?);
    }

    let first = specs.remove(0);
    let mut combo = Combo::new(&name, first);
    combo.steps.extend(specs); // flag order, preserved exactly

    if let Some(strategy) = &args.strategy {
        combo.strategy = parse_choice(
            "--strategy",
            strategy,
            &all_labels(&ComboStrategy::ALL),
            ComboStrategy::parse,
        )?;
    }
    if let Some(policy) = &args.midstream_policy {
        combo.midstream_policy = parse_choice(
            "--midstream-policy",
            policy,
            &MIDSTREAM_CHOICES.map(str::to_owned),
            MidstreamPolicy::parse,
        )?;
    }
    if args.hidden {
        combo.hidden = true;
    }
    if args.hedge {
        combo.hedge.enabled = true;
    }
    if let Some(raw) = &args.participants {
        combo.hedge.participants = parse_participants(raw, combo.steps.len())?;
        combo.hedge.enabled = true; // naming participants is asking for the race
    }
    if let Some(ms) = args.window_ms {
        combo.hedge.window_ms = u16::try_from(ms).map_err(|_| {
            CliError::validation(format!(
                "--window-ms must fit in {ms}ms as a u16 (max 65535)"
            ))
        })?;
        combo.hedge.enabled = true;
    }

    let human = vec![
        format!("combo `{name}` added with {} step(s)", combo.steps.len()),
        format!("steps: {}", args.step.join("  ")),
        format!("next: mr key create --allow-models \"{name}\""),
    ];
    let doc = serde_json::to_value(&combo)
        .map_err(|e| CliError::failure(format!("cannot serialise the combo: {e}")))?;
    cfg.combos.push(combo);
    let out = ctx.save(&cfg)?;
    ctx.finish(&out, human, doc)
}

fn list(ctx: &Ctx) -> Result<(), CliError> {
    let cfg = ctx.load()?;
    if ctx.json {
        return print_json(&cfg.combos);
    }
    let mut t = Table::new(["NAME", "ID", "STRATEGY", "STEPS", "ENABLED", "STATUS"]);
    for c in &cfg.combos {
        let enabled_steps = c.steps.iter().filter(|s| s.enabled).count();
        let status = if !c.enabled {
            "disabled"
        } else if enabled_steps == 0 {
            "no enabled steps"
        } else {
            "ready"
        };
        t.row(vec![
            c.name.clone(),
            c.id.as_str().to_owned(),
            serde_label_of(&c.strategy),
            format!("{} ({enabled_steps} on)", c.steps.len()),
            if c.enabled { "yes" } else { "no" }.to_owned(),
            status.to_owned(),
        ]);
    }
    t.print();
    if cfg.combos.is_empty() {
        say("no combos yet; `mr combo add <name> --step provider/model` creates one");
    }
    Ok(())
}

/// Serde label without importing `serde_label`'s whole vocabulary into this module's head.
fn serde_label_of<T: serde::Serialize>(v: &T) -> String {
    crate::output::serde_label(v)
}

fn show(ctx: &Ctx, name: &str) -> Result<(), CliError> {
    let cfg = ctx.load()?;
    let c = require_combo(&cfg, name)?;
    if ctx.json {
        return print_json(c);
    }
    let mut t = Table::new(["FIELD", "VALUE"]);
    t.row(vec!["name".to_owned(), c.name.clone()]);
    t.row(vec!["id".to_owned(), c.id.as_str().to_owned()]);
    t.row(vec!["strategy".to_owned(), serde_label_of(&c.strategy)]);
    t.row(vec![
        "midstream".to_owned(),
        serde_label_of(&c.midstream_policy),
    ]);
    t.row(vec![
        "enabled".to_owned(),
        if c.enabled { "yes" } else { "no" }.to_owned(),
    ]);
    t.row(vec![
        "hidden".to_owned(),
        if c.hidden { "yes" } else { "no" }.to_owned(),
    ]);
    t.row(vec![
        "hedge".to_owned(),
        if c.hedge.enabled {
            format!(
                "on (window {}ms{})",
                c.hedge.window_ms,
                if c.hedge.participants.is_empty() {
                    String::new()
                } else {
                    format!(", participants {:?}", c.hedge.participants)
                }
            )
        } else {
            "off".to_owned()
        },
    ]);
    t.print();

    let mut s = Table::new(["#", "PROVIDER", "MODEL", "WEIGHT", "ON", "NOTE"]);
    for (i, step) in c.steps.iter().enumerate() {
        s.row(vec![
            i.to_string(),
            step.provider.as_str().to_owned(),
            step.model.clone(),
            step.weight.to_string(),
            if step.enabled { "yes" } else { "no" }.to_owned(),
            step.note.clone().unwrap_or_default(),
        ]);
    }
    s.print();
    Ok(())
}

/// Does an alias address this combo (`Brains` or `Brains/fast`)?
fn alias_points_to(alias_to: &str, combo: &str) -> bool {
    alias_to == combo || alias_to.starts_with(&format!("{combo}/"))
}

fn remove(ctx: &Ctx, name: &str, force: bool) -> Result<(), CliError> {
    let mut cfg = ctx.load()?;
    require_combo(&cfg, name)?;

    let mut refs: Vec<String> = Vec::new();
    for a in &cfg.aliases {
        if alias_points_to(&a.to, name) {
            refs.push(format!("alias `{}` -> `{}`", a.from, a.to));
        }
    }
    for k in &cfg.client_keys {
        if k.allow_models.iter().any(|m| m == name) {
            refs.push(format!("client key `{}` allows model `{name}`", k.label));
        }
    }
    if !refs.is_empty() && !force {
        return Err(CliError::validation(format!(
            "combo `{name}` is still referenced; nothing was written"
        ))
        .with_details(refs)
        .with_detail("re-run with --force to remove it and those references"));
    }

    let aliases_dropped = cfg.aliases.len();
    cfg.aliases.retain(|a| !alias_points_to(&a.to, name));
    let aliases_dropped = aliases_dropped - cfg.aliases.len();
    let mut keys_touched = 0usize;
    for k in &mut cfg.client_keys {
        let before = k.allow_models.len();
        k.allow_models.retain(|m| m != name);
        if k.allow_models.len() != before {
            keys_touched += 1;
        }
    }
    cfg.combos
        .retain(|c| c.name != name && c.id.as_str() != name);

    let mut human = vec![format!("combo `{name}` removed")];
    if aliases_dropped > 0 {
        human.push(format!("{aliases_dropped} alias(es) dropped with it"));
    }
    if keys_touched > 0 {
        human.push(format!("{keys_touched} client key(s) updated"));
    }
    let out = ctx.save(&cfg)?;
    ctx.finish(
        &out,
        human,
        serde_json::json!({
            "name": name,
            "removed": true,
            "aliases_dropped": aliases_dropped,
            "keys_updated": keys_touched,
        }),
    )
}

fn set_enabled(ctx: &Ctx, name: &str, enabled: bool) -> Result<(), CliError> {
    let mut cfg = ctx.load()?;
    let idx = combo_position(&cfg, name)?;
    let c = &mut cfg.combos[idx];
    c.enabled = enabled;
    let name = c.name.clone();
    let verb = if enabled { "enabled" } else { "disabled" };
    let out = ctx.save(&cfg)?;
    ctx.finish(
        &out,
        vec![format!("combo `{name}` {verb}")],
        serde_json::json!({ "name": name, "enabled": enabled }),
    )
}

// ---------------------------------------------------------------------------
// combo step add / remove / move
// ---------------------------------------------------------------------------

fn step_cmd(ctx: &Ctx, cmd: StepCmd) -> Result<(), CliError> {
    match cmd {
        StepCmd::Add { name, step, at } => step_add(ctx, &name, &step, at),
        StepCmd::Remove { name, index } => step_remove(ctx, &name, index),
        StepCmd::Move { name, from, to } => step_move(ctx, &name, from, to),
    }
}

fn step_add(ctx: &Ctx, name: &str, spec: &str, at: Option<usize>) -> Result<(), CliError> {
    let mut cfg = ctx.load()?;
    // Validate the new step (provider must exist) before touching the combo.
    let step = build_step(&cfg, spec)?;
    let idx = combo_position(&cfg, name)?;
    let combo = &mut cfg.combos[idx];
    let len = combo.steps.len();
    let insert_at = match at {
        Some(i) if i > len => {
            return Err(CliError::usage(format!(
                "insert position {i} is out of range: combo `{}` has {len} step(s), so positions \
                 are 0..={len}",
                combo.name
            )));
        }
        Some(i) => i,
        None => len,
    };
    combo.steps.insert(insert_at, step.clone());
    let combo_name = combo.name.clone();
    let human = vec![format!(
        "step inserted at {insert_at} of `{combo_name}`: {}/{}",
        step.provider.as_str(),
        step.model
    )];
    let out = ctx.save(&cfg)?;
    ctx.finish(
        &out,
        human,
        serde_json::json!({
            "name": combo_name,
            "index": insert_at,
            "step": step,
        }),
    )
}

fn step_remove(ctx: &Ctx, name: &str, index: usize) -> Result<(), CliError> {
    let mut cfg = ctx.load()?;
    let idx = combo_position(&cfg, name)?;
    let combo = &mut cfg.combos[idx];
    let len = combo.steps.len();
    if index >= len {
        return Err(CliError::usage(format!(
            "step index {index} is out of range: combo `{}` has {len} step(s), so the valid \
             indices are 0..{}",
            combo.name,
            len.saturating_sub(1)
        )));
    }
    if len == 1 {
        return Err(CliError::validation(format!(
            "combo `{}` would be left without any steps",
            combo.name
        ))
        .with_detail("a combo needs at least one step to compile into a route")
        .with_detail(format!(
            "remove the whole combo with `mr combo remove {name}`"
        )));
    }
    let removed = combo.steps.remove(index);
    let combo_name = combo.name.clone();
    let out = ctx.save(&cfg)?;
    ctx.finish(
        &out,
        vec![format!(
            "step {index} removed from `{combo_name}`: {}/{}",
            removed.provider.as_str(),
            removed.model
        )],
        serde_json::json!({ "name": combo_name, "removed_index": index, "step": removed }),
    )
}

fn step_move(ctx: &Ctx, name: &str, from: usize, to: usize) -> Result<(), CliError> {
    let mut cfg = ctx.load()?;
    let idx = combo_position(&cfg, name)?;
    let combo = &mut cfg.combos[idx];
    let len = combo.steps.len();
    if from >= len || to >= len {
        let valid = format!("0..{}", len.saturating_sub(1));
        let which = if from >= len { from } else { to };
        let flag = if from >= len { "from" } else { "to" };
        return Err(CliError::usage(format!(
            "{flag} index {which} is out of range: combo `{}` has {len} step(s), so the valid \
             indices are {valid}",
            combo.name
        )));
    }
    let step = combo.steps.remove(from);
    let moved_desc = format!("{}/{}", step.provider.as_str(), step.model);
    combo.steps.insert(to, step);
    let combo_name = combo.name.clone();
    let snapshot: Vec<String> = combo
        .steps
        .iter()
        .map(|s| format!("{}/{}", s.provider.as_str(), s.model))
        .collect();
    let out = ctx.save(&cfg)?;
    ctx.finish(
        &out,
        vec![
            format!("moved step {from} -> {to} in `{combo_name}`: {moved_desc}"),
            format!("order: {}", snapshot.join("  ")),
        ],
        serde_json::json!({
            "name": combo_name,
            "from": from,
            "to": to,
            "order": snapshot,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::testutil::{block_on, with_config_dir};
    use crate::output::Exit;

    /// Seed a config with one openai-ish provider per id, so step building has something real to
    /// resolve against.
    fn seed(ids: &[&str]) {
        let mut cfg = Config::default();
        for id in ids {
            cfg.providers.push(mr_core::model::Provider::new(
                *id,
                mr_core::dialect::Dialect::OpenAi,
                "http://127.0.0.1:9/v1",
            ));
        }
        cfg.save_default().unwrap();
    }

    fn add_args(name: &str, steps: &[&str]) -> AddArgs {
        AddArgs {
            positional: Some(name.to_owned()),
            name: None,
            step: steps.iter().map(|s| s.to_string()).collect(),
            strategy: None,
            midstream_policy: None,
            hidden: false,
            hedge: false,
            participants: None,
            window_ms: None,
        }
    }

    fn run_step(ctx: &Ctx, cmd: StepCmd) -> Result<(), CliError> {
        block_on(run(ComboCmd::Step(cmd), ctx))
    }

    #[test]
    fn step_order_is_the_flag_order_end_to_end() {
        with_config_dir("combo-step-order", |_dir| {
            seed(&["openai", "anthropic", "deepseek"]);
            let ctx = Ctx { json: false };
            let mut args = add_args(
                "Brains",
                &[
                    "deepseek/deepseek-chat",
                    "openai/gpt-5.5",
                    "anthropic/claude-sonnet-4-5",
                ],
            );
            args.strategy = Some("priority".into());
            block_on(run(ComboCmd::Add(args), &ctx)).unwrap();

            let cfg = Config::load_default().unwrap();
            let c = cfg.combo("Brains").unwrap();
            let order: Vec<String> = c
                .steps
                .iter()
                .map(|s| format!("{}/{}", s.provider.as_str(), s.model))
                .collect();
            assert_eq!(
                order,
                vec![
                    "deepseek/deepseek-chat",
                    "openai/gpt-5.5",
                    "anthropic/claude-sonnet-4-5",
                ]
            );
            assert_eq!(c.strategy, ComboStrategy::Priority);
        });
    }

    #[test]
    fn step_move_reorders_and_persists() {
        with_config_dir("combo-step-move", |_dir| {
            seed(&["openai", "anthropic"]);
            let ctx = Ctx { json: false };
            block_on(run(
                ComboCmd::Add(add_args("Chain", &["openai/a", "openai/b", "anthropic/c"])),
                &ctx,
            ))
            .unwrap();

            run_step(
                &ctx,
                StepCmd::Move {
                    name: "Chain".into(),
                    from: 0,
                    to: 2,
                },
            )
            .unwrap();

            let cfg = Config::load_default().unwrap();
            let c = cfg.combo("Chain").unwrap();
            let order: Vec<&str> = c.steps.iter().map(|s| s.model.as_str()).collect();
            assert_eq!(order, vec!["b", "c", "a"]);
        });
    }

    #[test]
    fn step_move_out_of_range_is_a_usage_error_and_changes_nothing() {
        with_config_dir("combo-move-oob", |_dir| {
            seed(&["openai"]);
            let ctx = Ctx { json: false };
            block_on(run(
                ComboCmd::Add(add_args("Chain", &["openai/a", "openai/b"])),
                &ctx,
            ))
            .unwrap();

            for (from, to) in [(0usize, 9usize), (9, 0), (5, 5)] {
                let err = run_step(
                    &ctx,
                    StepCmd::Move {
                        name: "Chain".into(),
                        from,
                        to,
                    },
                )
                .unwrap_err();
                assert_eq!(err.exit, Exit::Usage, "{err}");
            }
            // Untouched: both steps still in their original places.
            let cfg = Config::load_default().unwrap();
            let c = cfg.combo("Chain").unwrap();
            let order: Vec<&str> = c.steps.iter().map(|s| s.model.as_str()).collect();
            assert_eq!(order, vec!["a", "b"]);
        });
    }

    #[test]
    fn step_add_at_beyond_the_end_is_a_usage_error() {
        with_config_dir("combo-add-at-oob", |_dir| {
            seed(&["openai"]);
            let ctx = Ctx { json: false };
            block_on(run(ComboCmd::Add(add_args("Chain", &["openai/a"])), &ctx)).unwrap();
            let err = run_step(
                &ctx,
                StepCmd::Add {
                    name: "Chain".into(),
                    step: "openai/b".into(),
                    at: Some(4),
                },
            )
            .unwrap_err();
            assert_eq!(err.exit, Exit::Usage, "{err}");
            // ...but appending with --at == len is legal.
            run_step(
                &ctx,
                StepCmd::Add {
                    name: "Chain".into(),
                    step: "openai/b".into(),
                    at: Some(1),
                },
            )
            .unwrap();
            let cfg = Config::load_default().unwrap();
            assert_eq!(cfg.combo("Chain").unwrap().steps.len(), 2);
        });
    }

    #[test]
    fn removing_the_last_step_is_refused_and_removing_by_index_works() {
        with_config_dir("combo-step-remove", |_dir| {
            seed(&["openai"]);
            let ctx = Ctx { json: false };
            block_on(run(ComboCmd::Add(add_args("Solo", &["openai/a"])), &ctx)).unwrap();
            let err = run_step(
                &ctx,
                StepCmd::Remove {
                    name: "Solo".into(),
                    index: 0,
                },
            )
            .unwrap_err();
            assert_eq!(err.exit, Exit::Validation, "{err}");

            // Two steps: removing the unknown index is a usage error, a real one succeeds.
            block_on(run(
                ComboCmd::Add(add_args("Pair", &["openai/a", "openai/b"])),
                &ctx,
            ))
            .unwrap();
            let err = run_step(
                &ctx,
                StepCmd::Remove {
                    name: "Pair".into(),
                    index: 7,
                },
            )
            .unwrap_err();
            assert_eq!(err.exit, Exit::Usage, "{err}");
            run_step(
                &ctx,
                StepCmd::Remove {
                    name: "Pair".into(),
                    index: 1,
                },
            )
            .unwrap();
            let cfg = Config::load_default().unwrap();
            assert_eq!(cfg.combo("Pair").unwrap().steps.len(), 1);
        });
    }

    #[test]
    fn combo_add_needs_a_step_and_a_known_provider() {
        with_config_dir("combo-add-guards", |_dir| {
            seed(&["openai"]);
            let ctx = Ctx { json: false };
            let err = block_on(run(ComboCmd::Add(add_args("Empty", &[])), &ctx)).unwrap_err();
            assert_eq!(err.exit, Exit::Usage, "{err}");

            let err = block_on(run(
                ComboCmd::Add(add_args("Ghost", &["nosuchprovider/model"])),
                &ctx,
            ))
            .unwrap_err();
            assert_eq!(err.exit, Exit::NotFound, "{err}");
            // Nothing was written: the config has no combos at all.
            assert!(Config::load_default().unwrap().combos.is_empty());
        });
    }

    #[test]
    fn duplicate_combo_names_refuse_to_overwrite() {
        with_config_dir("combo-dup", |_dir| {
            seed(&["openai"]);
            let ctx = Ctx { json: false };
            block_on(run(ComboCmd::Add(add_args("Dupe", &["openai/a"])), &ctx)).unwrap();
            let err =
                block_on(run(ComboCmd::Add(add_args("Dupe", &["openai/b"])), &ctx)).unwrap_err();
            assert_eq!(err.exit, Exit::Validation, "{err}");
            let cfg = Config::load_default().unwrap();
            assert_eq!(cfg.combo("Dupe").unwrap().steps.len(), 1);
        });
    }

    #[test]
    fn name_twice_and_differently_is_a_usage_error() {
        with_config_dir("combo-name-twice", |_dir| {
            seed(&["openai"]);
            let ctx = Ctx { json: false };
            let mut args = add_args("One", &["openai/a"]);
            args.name = Some("Two".into());
            let err = block_on(run(ComboCmd::Add(args), &ctx)).unwrap_err();
            assert_eq!(err.exit, Exit::Usage, "{err}");
            assert!(Config::load_default().unwrap().combos.is_empty());
        });
    }

    #[test]
    fn hedge_flags_reach_the_combo() {
        with_config_dir("combo-hedge", |_dir| {
            seed(&["openai"]);
            let ctx = Ctx { json: false };
            let mut args = add_args("Racy", &["openai/a", "openai/b", "openai/c"]);
            args.participants = Some("0,2".into());
            args.window_ms = Some(250);
            block_on(run(ComboCmd::Add(args), &ctx)).unwrap();
            let cfg = Config::load_default().unwrap();
            let c = cfg.combo("Racy").unwrap();
            assert!(c.hedge.enabled);
            assert_eq!(c.hedge.participants, vec![0, 2]);
            assert_eq!(c.hedge.window_ms, 250);

            // Out-of-range participant is a validation problem, and nothing was written.
            let mut args = add_args("BadRacy", &["openai/a"]);
            args.participants = Some("4".into());
            let err = block_on(run(ComboCmd::Add(args), &ctx)).unwrap_err();
            assert_eq!(err.exit, Exit::Validation, "{err}");

            let mut args = add_args("BigWindow", &["openai/a"]);
            args.window_ms = Some(100_000);
            let err = block_on(run(ComboCmd::Add(args), &ctx)).unwrap_err();
            assert_eq!(err.exit, Exit::Validation, "{err}");
        });
    }

    #[test]
    fn remove_refuses_references_then_force_drops_them() {
        with_config_dir("combo-remove-refs", |_dir| {
            seed(&["openai"]);
            let ctx = Ctx { json: false };
            block_on(run(ComboCmd::Add(add_args("Brain", &["openai/a"])), &ctx)).unwrap();

            // An alias addressing the combo (name, and name/model form).
            let mut cfg = Config::load_default().unwrap();
            cfg.aliases.push(mr_core::model::Alias {
                from: "gpt".into(),
                to: "Brain".into(),
                generated: false,
            });
            cfg.aliases.push(mr_core::model::Alias {
                from: "fast".into(),
                to: "Brain/fast".into(),
                generated: false,
            });
            cfg.client_keys.push(mr_core::model::ClientKey {
                id: "k1".into(),
                label: "editor".into(),
                key_hash: "h".into(),
                prefix: None,
                enabled: true,
                allow_models: vec!["Brain".into()],
                allow_routes: vec![],
                limit_requests: 0,
                limits: Default::default(),
                note: None,
            });
            cfg.save_default().unwrap();

            let err = block_on(run(
                ComboCmd::Remove {
                    name: "Brain".into(),
                    force: false,
                },
                &ctx,
            ))
            .unwrap_err();
            assert_eq!(err.exit, Exit::Validation, "{err}");
            // 3 references + the --force hint.
            assert_eq!(err.details.len(), 4, "{:?}", err.details);
            assert!(err.details.iter().any(|d| d.contains("alias `gpt`")));
            assert!(err.details.iter().any(|d| d.contains("alias `fast`")));
            assert!(err.details.iter().any(|d| d.contains("client key")));
            assert!(err.details[3].contains("--force"));
            assert!(Config::load_default().unwrap().combo("Brain").is_some());

            block_on(run(
                ComboCmd::Remove {
                    name: "Brain".into(),
                    force: true,
                },
                &ctx,
            ))
            .unwrap();
            let cfg = Config::load_default().unwrap();
            assert!(cfg.combo("Brain").is_none());
            assert!(cfg.aliases.is_empty());
            assert!(cfg.client_keys[0].allow_models.is_empty());
        });
    }

    #[test]
    fn midstream_choices_match_serde_labels() {
        for variant in [MidstreamPolicy::Restart, MidstreamPolicy::Terminate] {
            let label = serde_json::to_value(variant)
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned();
            assert!(
                MIDSTREAM_CHOICES.contains(&label.as_str()),
                "{label} missing from MIDSTREAM_CHOICES"
            );
            assert!(MidstreamPolicy::parse(&label).is_some());
        }
    }

    #[test]
    fn step_specs_split_on_the_first_slash_only() {
        assert_eq!(
            parse_step("google/models/gemini-2.5-pro").unwrap(),
            ("google".to_owned(), "models/gemini-2.5-pro".to_owned())
        );
        let err = parse_step("nonsense").unwrap_err();
        assert_eq!(err.exit, Exit::Usage);
        assert!(err.message.contains("provider/model"), "{}", err.message);
    }

    #[test]
    fn participants_parse_rejects_gaps_and_duplicates() {
        assert_eq!(parse_participants("0, 2", 3).unwrap(), vec![0, 2]);
        assert_eq!(parse_participants("1", 2).unwrap().len(), 1);
        assert_eq!(
            parse_participants("3", 3).unwrap_err().exit,
            Exit::Validation
        );
        assert_eq!(parse_participants("1,1", 3).unwrap_err().exit, Exit::Usage);
        assert_eq!(parse_participants("", 3).unwrap_err().exit, Exit::Usage);
        assert_eq!(parse_participants("x", 3).unwrap_err().exit, Exit::Usage);
    }
}
