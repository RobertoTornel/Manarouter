//! `mr combo` — named model routes, and the ordered steps inside them.
//!
//! Stub: the clap surface is final, the bodies are filled in one at a time.

use clap::Subcommand;

use crate::output::CliError;
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
    /// Selection strategy: priority, round-robin, reset-aware-priority.
    #[arg(long, value_name = "STRATEGY")]
    pub strategy: Option<String>,
    /// What to do when the stream breaks mid-response: restart, resume, terminate.
    #[arg(long = "midstream-policy", value_name = "POLICY")]
    pub midstream_policy: Option<String>,
    /// Keep the combo out of listing and discovery until it is enabled.
    #[arg(long)]
    pub hidden: bool,
    /// Race the first N steps instead of falling back.
    #[arg(long)]
    pub hedge: bool,
    /// Which step indices take part in the hedge (`0,1,2`).
    #[arg(long, value_name = "INDICES")]
    pub participants: Option<String>,
    /// Hedge decision window in milliseconds.
    #[arg(long = "window-ms", value_name = "MS")]
    pub window_ms: Option<u64>,
}

impl AddArgs {
    /// The name from whichever spelling the operator used.
    pub fn resolved_name(&self) -> Option<&str> {
        self.positional.as_deref().or(self.name.as_deref())
    }
}

#[derive(Subcommand, Debug)]
pub enum StepCmd {
    /// Append (or insert at --at) a step.
    Add {
        /// Combo name or id.
        name: String,
        /// The step to add, `provider/model`.
        #[arg(long = "step", value_name = "PROVIDER/MODEL")]
        step: String,
        /// Insert position (default: append).
        #[arg(long, value_name = "INDEX")]
        at: Option<usize>,
    },
    /// Delete the step at this index (`combo show` numbers them).
    Remove {
        /// Combo name or id.
        name: String,
        /// Step index, zero-based.
        index: usize,
    },
    /// Move the step at `from` to position `to`.
    Move {
        /// Combo name or id.
        name: String,
        /// Current step index, zero-based.
        from: usize,
        /// Destination index, zero-based (must be within the combo).
        to: usize,
    },
}

/// Run a combo subcommand.
pub async fn run(cmd: ComboCmd, ctx: &Ctx) -> Result<(), CliError> {
    let _ = (&cmd, ctx);
    Err(CliError::failure("`mr combo` is not implemented yet"))
}
