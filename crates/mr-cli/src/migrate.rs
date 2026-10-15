//! `mr migrate` — import a previous installation.
//!
//! The rules the migration must keep, from docs/17 and plan §13: it reads a *copy* of the
//! reference database and never the original, and it does nothing until `--apply` is given.
//!
//! OAuth transfer lives in [`mr_core::oauth::migrate`], because it needs the platform
//! implementations and the vault; this module is the CLI surface over it and the reporting.

use std::path::PathBuf;

use mr_core::oauth::migrate::{
    run_migration, MigrationOptions, MigrationReport, PlannedTransfer, TransferAction,
};

use crate::output::{say, warn, CliError};
use crate::Ctx;

#[derive(clap::Args, Debug)]
pub struct MigrateCmd {
    /// Which system to import from.
    #[arg(value_enum)]
    pub source: Source,
    /// Perform the import. Without it the command only reports what it would do.
    #[arg(long)]
    pub apply: bool,
    /// Path to the reference database (default: ~/.omniroute/storage.sqlite).
    #[arg(long, value_name = "PATH")]
    pub db: Option<PathBuf>,
    /// The reference data directory holding its `.env` (default: alongside the database).
    #[arg(long, value_name = "DIR")]
    pub data_dir: Option<PathBuf>,
    /// Only consider these reference providers (repeatable), for example `--only codex`.
    #[arg(long, value_name = "PROVIDER")]
    pub only: Vec<String>,
}

/// Supported sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Source {
    /// Import an OmniRoute installation.
    Omniroute,
}

/// Run a migrate subcommand.
pub async fn run(cmd: MigrateCmd, ctx: &Ctx) -> Result<(), CliError> {
    match cmd.source {
        Source::Omniroute => run_omniroute(cmd, ctx).await,
    }
}

async fn run_omniroute(cmd: MigrateCmd, ctx: &Ctx) -> Result<(), CliError> {
    let options = MigrationOptions {
        apply: cmd.apply,
        database: cmd.db.clone(),
        data_dir: cmd.data_dir.clone(),
        only: cmd.only.clone(),
    };
    let report = run_migration(&options)
        .await
        .map_err(|e| CliError::failure(e.to_string()))?;

    if ctx.json {
        return crate::output::print_json(&report_json(&report));
    }
    print_human(&report, cmd.apply);
    Ok(())
}

/// The machine-readable report.
///
/// A dry run and an applied run share one shape so a caller can diff them; `applied` is simply
/// empty before `--apply`.
fn report_json(report: &MigrationReport) -> serde_json::Value {
    serde_json::json!({
        "source": report.plan.source.display().to_string(),
        "source_sha256": report.plan.source_hash,
        "source_changed_while_reading": report.plan.source_changed,
        "models_updated": report.models_updated,
        "transfers": report.plan.transfers.iter().map(transfer_json).collect::<Vec<_>>(),
        "refused": report.plan.refused.iter().map(|(p, r)| serde_json::json!({"provider": p, "reason": r})).collect::<Vec<_>>(),
        "skipped": report.plan.skipped.iter().map(|(p, r)| serde_json::json!({"provider": p, "reason": r})).collect::<Vec<_>>(),
        "applied": report.applied.iter().map(|a| serde_json::json!({
            "provider": a.provider_id,
            "account": a.account_id,
            "label": a.label,
            "provider_created": a.provider_created,
        })).collect::<Vec<_>>(),
        "failed": report.failed.iter().map(|(id, reason)| serde_json::json!({"connection": id, "reason": reason})).collect::<Vec<_>>(),
        "config": report.config_path.display().to_string(),
        "snapshot": report.snapshot_path.display().to_string(),
    })
}

fn transfer_json(t: &PlannedTransfer) -> serde_json::Value {
    serde_json::json!({
        "reference_id": t.reference_id,
        "reference_provider": t.reference_provider,
        "platform": t.platform,
        "provider": t.provider_id,
        "label": t.label,
        "email": t.email,
        "has_refresh_token": t.has_refresh,
        // A fingerprint, never the credential.
        "credential_fingerprint": t.fingerprint,
        "active_in_reference": t.was_active,
        "cloudcode_project": t.cloudcode_project,
        "reason": t.reason,
        "action": t.action.label(),
    })
}

/// Human-readable output, shaped like the report `docs/17-migrating-from-omniroute.md` shows.
fn print_human(report: &MigrationReport, applied: bool) {
    say(format!(
        "Reference database  {}",
        report.plan.source.display()
    ));
    say("                    (copy opened read-only; original untouched)");
    say(format!("Original sha256     {}", report.plan.source_hash));
    if report.plan.source_changed {
        // Reported, not fatal: the reference is usually running, and a live SQLite write changes
        // the file. What matters is that the *copy* the import read is the one that was hashed.
        warn(
            "the reference database changed while it was being read (it is probably running); \
             the import used the copy taken at the start",
        );
    }
    say(String::new());

    let importable = report.plan.importable().len();
    say(format!("OAuth accounts to import   {importable}"));
    for transfer in report.plan.importable() {
        let project = transfer
            .cloudcode_project
            .as_deref()
            .map(|p| format!("  project {p}"))
            .unwrap_or_default();
        // A grant with no project is importable *if* onboarding succeeds, which a dry run cannot
        // know. Saying so is more honest than either hiding it or promising it.
        let caveat = if transfer.action == TransferAction::NeedsProject {
            "  (needs Cloud Code onboarding)"
        } else {
            ""
        };
        say(format!(
            "  {} / {:<28} {}  refreshable{}{}",
            transfer.provider_id,
            transfer.email.as_deref().unwrap_or(&transfer.label),
            transfer
                .fingerprint
                .as_deref()
                .map(|f| format!("credential …{f}"))
                .unwrap_or_else(|| "credential present".to_owned()),
            project,
            caveat,
        ));
    }

    let untouched: Vec<&PlannedTransfer> = report
        .plan
        .transfers
        .iter()
        .filter(|t| t.action == TransferAction::AlreadyPresent)
        .collect();
    if !untouched.is_empty() {
        say(String::new());
        say(format!(
            "OAuth accounts already present   {}",
            untouched.len()
        ));
        for transfer in untouched {
            say(format!(
                "  {} / {}   left alone",
                transfer.provider_id,
                transfer.email.as_deref().unwrap_or(&transfer.label)
            ));
        }
    }

    let relogin: Vec<&PlannedTransfer> = report
        .plan
        .transfers
        .iter()
        .filter(|t| t.action == TransferAction::NeedsRelogin)
        .collect();
    if !relogin.is_empty() {
        say(String::new());
        say(format!(
            "OAuth accounts needing a fresh login   {}",
            relogin.len()
        ));
        for transfer in relogin {
            say(format!(
                "  {} / {}   {}",
                transfer.provider_id,
                transfer.email.as_deref().unwrap_or(&transfer.label),
                transfer
                    .reason
                    .as_deref()
                    .unwrap_or("the stored grant cannot be refreshed by this build")
            ));
        }
    }

    if !report.plan.refused.is_empty() {
        say(String::new());
        say(format!("Providers refused   {}", report.plan.refused.len()));
        for (provider, reason) in &report.plan.refused {
            say(format!("  {provider:<18} {reason}"));
        }
    }
    if !report.plan.skipped.is_empty() {
        say(String::new());
        say(format!("Providers skipped   {}", report.plan.skipped.len()));
        for (provider, reason) in &report.plan.skipped {
            say(format!("  {provider:<18} {reason}"));
        }
    }

    if !applied {
        say(String::new());
        say("Nothing was written. Re-run with --apply to perform the import.");
        return;
    }

    say(String::new());
    if report.applied.is_empty() && report.failed.is_empty() && report.models_updated.is_empty() {
        say("Nothing to import.");
        return;
    }
    if !report.models_updated.is_empty() {
        say(format!(
            "Model lists extended   {} (credential already present, models added)",
            report.models_updated.len()
        ));
        for label in &report.models_updated {
            say(format!("  {label}"));
        }
    }
    say(format!("Accounts imported   {}", report.applied.len()));
    for entry in &report.applied {
        say(format!(
            "  {} / {}   account {}{}",
            entry.provider_id,
            entry.label,
            entry.account_id,
            if entry.provider_created {
                "   (provider created from the catalogue)"
            } else {
                ""
            }
        ));
    }
    if !report.failed.is_empty() {
        warn(format!(
            "{} account(s) could not be imported:",
            report.failed.len()
        ));
        for (connection, reason) in &report.failed {
            say(format!("  {connection}   {reason}"));
        }
    }
    // Only true when something was actually written; a run where every import failed leaves the
    // existing configuration alone, and claiming otherwise would be a lie an operator would
    // discover by reading the file.
    if !report.applied.is_empty() {
        say(String::new());
        say(format!(
            "configuration written to {}",
            report.config_path.display()
        ));
        say(format!(
            "routing snapshot written to {}",
            report.snapshot_path.display()
        ));
        say(
            "The imported grants were refreshed, so these accounts are live; the relay picks them \
             up on its next snapshot reload.",
        );
    }
}
