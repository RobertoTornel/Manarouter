//! The batch lane: many prompts on one rented card, with the cost written down.
//!
//! # Why a second lane exists
//!
//! The request path is built for one interactive call: it is latency-critical, it streams, and it
//! holds nothing after the client disconnects. A batch is the opposite shape — hundreds or thousands
//! of independent prompts, an operator who does not care whether item 400 finishes now or in ten
//! minutes, and a rented GPU that bills by the second whether it is working or not. Serving a batch
//! through the request path would work and would be wrong: a client that submits 1,000 prompts in a
//! loop gets 1,000 independent cold starts on a serverless endpoint, which is the most expensive way
//! to run it.
//!
//! So this module is the second lane of `docs/research/ARCHITECTURE-gpu-rentals.md` §9: submit a
//! file, get a batch id, poll it, read the results in order. It is deliberately a *queue*, not a
//! pass-through: `POST /v1/batches` validates and returns immediately, and the items are dispatched
//! by a small fixed worker pool.
//!
//! # Where the money goes, and why boot cost is tracked separately
//!
//! On a serverless GPU every cold start has a fixed fee: the time the rented container spends
//! loading weights before it can serve its first token. The operator measured 180 s at $3.03/h on an
//! RTX PRO 6000 ($0.1516) and 110 s at $1.95/h on an L40S ($0.0596). That fee is not proportional to
//! anything the batch did — it is the price of the card being cold — so reporting one blended dollar
//! figure would hide the single number that decides whether batching was worth it. The usage block
//! therefore reports `gpu_seconds`, `cold_starts` and `boot_cost_usd` next to `cost_usd`, and
//! `cost_usd` is computed as `boot_cost_usd + (gpu_seconds - cold_start_seconds) * price`, which is
//! algebraically the honest `gpu_seconds * price` but keeps the two halves legible.
//!
//! Every cost figure comes from the GPU-second ledger ([`crate::kv_budget::GpuBilling`] writes one
//! `gpu_usage` row per settled attempt), never from token counts. Tokens are not money here: they are
//! what the client is charged for elsewhere, and pricing a rented card off them would be a guess
//! dressed as a measurement. When the operator's configuration declares no `price_per_second` for
//! the provider, every cost field is reported as `null` rather than estimated.
//!
//! A cold start is **normal**, not an error. A batch item that waited for a container to boot and
//! then succeeded is a completed item with a nonzero `cold_starts` count; it must never appear in
//! the error file. The signal for it is [`crate::select::CooldownReason::ColdStart`], which the relay
//! records on the model's cooldown scope; the relay-side executor (see `ServerBatchExecutor` in
//! `server.rs`, which is where the health state is reachable from) counts it by diffing the health
//! snapshot around each item.
//!
//! # A ceiling on what a batch may spend
//!
//! Reporting a cost after the fact does not stop a batch from running up one, so a submission may
//! carry `max_cost_usd`. The check is made against money **already settled**, never against a
//! prediction: before each item is dispatched the lane reads the ledger it has been keeping, and if
//! the batch has reached its cap it dispatches nothing further. The items that never ran are retired
//! with no result file, exactly as a cancellation retires them — they are not failures, because
//! nothing about them failed; the operator's ceiling was reached first.
//!
//! In-flight items are not interrupted. A cap that killed a request mid-answer would waste the GPU
//! seconds it had already been billed for and produce neither a result nor an error, so the ceiling
//! stops the queue and lets the current item land. An endpoint that declares no `price_per_second`
//! contributes `0.0` to the ledger, which makes the running total a *floor*: a capped batch can
//! overspend its cap by the unpriced share, but it can never under-report what it spent.
//!
//! A batch that stops this way ends as `cancelled`, because that is what the OpenAI batch status
//! vocabulary offers for "stopped before its item count", and carries `budget_exceeded: true` and
//! `stopped_reason: "max_cost_usd_exceeded"` so the reason is never inferred from the status alone.
//! A batch that happens to end exactly at its cap reports the same, since the cap did bind.
//!
//! # Admission is KV tokens, not requests
//!
//! A rented endpoint has a fixed KV-cache budget (the operator's own measurements: 1,777,758 tokens
//! on the RTX PRO 6000, 281,729 on the L40S). What it cannot do is serve more *tokens* than fit; how
//! many separate requests those tokens are spread across is a detail of how the client sliced its
//! work. The lane therefore admits per item, in tokens, through the same
//! [`crate::kv_budget::AdmissionPool`] the request path uses, under the same key (the provider id,
//! because the budget belongs to the endpoint). Each item reserves before it is dispatched and
//! releases when its answer has been fully read, which is exact here because batch items do not
//! stream.
//!
//! Two consequences are worth stating. First, the reserve happens before the upstream call, so an
//! item that cannot fit is an item that was never sent — that is where the money is saved. Second, an
//! endpoint that is merely *full* (other work is in flight) is not a failure: the item waits, with a
//! bounded backoff, and only gives up after [`ADMISSION_WAIT`].
//!
//! # Resumability
//!
//! The relay is a process; processes die. A batch that took twenty minutes and is re-run from the
//! beginning after a crash pays for the GPU time it already bought, so the lane writes the truth to
//! disk as it goes:
//!
//! ```text
//! <state_dir>/batches/
//!   files/<file_id>.json        file metadata          (the API's view of a file)
//!   files/<file_id>.jsonl       file bytes             (input, output or error)
//!   jobs/<batch_id>/job.json    batch metadata + counters + output/error file ids
//!   jobs/<batch_id>/items/<n>.json   one settled item  (the crash-consistent truth)
//! ```
//!
//! The per-item file is the unit of progress: it is written atomically (temporary file, then rename)
//! *after* an item has settled, so an item file that exists is an item that must not be dispatched
//! again, and an item with no file was either never started or was in flight when the process died
//! and must be re-run. The counters in `job.json` are a convenience for the API while the relay is
//! running; on restart they are recomputed from the item files, because a counter is a cache and the
//! item files are the ledger. Output and error files are assembled in index order from the item
//! files, which is what makes the results come back in order with their original `custom_id`s even
//! though the workers finish them out of order.
//!
//! # What this module does not do
//!
//! - It does not enforce the completion window. `completion_window` is required and recorded, and
//!   `expires_at` is reported, but nothing expires a batch: an item waits for admission for at most
//!   [`ADMISSION_WAIT`] and an attempt has its own upstream timeout, so a batch always ends, but a
//!   batch that stalls does not become `expired`. The status exists because the API shape needs it.
//! - It does not re-apply per-client-key quota to individual items. Items are dispatched as
//!   pre-authorized requests (`pre_authorized` on the relay's dispatch path), because the batch was
//!   authorized once, as a whole, when it was submitted. An operator who meters client keys should
//!   treat a batch as one metered submission, not N metered calls.
//! - It does not price tokens. See above.
//!
//! # Concurrency rules this module keeps
//!
//! A fixed pool of [`DEFAULT_WORKERS`] tasks, not one thread per item: a batch of 10,000 items must
//! not create 10,000 concurrent upstream calls, and the whole point of the KV budget is that the item
//! count says nothing about how much can run at once. The pool size can be lowered (or raised, to 64)
//! with [`WORKERS_ENV`], which is the only priority knob this lane has.
//!
//! # Why there is no priority class
//!
//! A batch and an interactive request share one endpoint, so the question "does interactive traffic
//! win" is real. This lane answers it by *waiting*, not by pre-empting: an item holds nothing between
//! attempts (the KV lease is released before the retry sleep, and the sleep is not a reservation), and
//! the operator can shrink the worker pool so a batch never occupies the whole card. What it will not
//! do is cancel an in-flight upstream call to make room, because no route in this relay cancels its
//! own upstream call and a cancelled generation is billed the same as a finished one — the token
//! would be spent and wasted. So the choice is between "a batch delays an interactive request by one
//! item" and "an interactive request costs the operator a killed generation", and the module picks
//! the first. No lock is held across an `await` (the job map
//! and each job's metadata are `std::sync::Mutex`, taken, mutated and dropped within a synchronous
//! block), no batch I/O runs on the accept loop (file work goes through `spawn_blocking`), and no
//! credential, `Authorization` header or request body is ever logged.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Body as AxumBody;
use axum::extract::{FromRequest, Multipart, Path as RoutePath, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use bytes::Bytes;
use mr_core::dialect::Dialect;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::kv_budget::{AdmissionPool, GpuBilling, KvBudgetDenial, KvBudgetLease, KvBudgetLimits};
use crate::server::AppState;

/// The largest input file the lane will accept.
///
/// A batch file is a *collection* of request bodies and the lane holds the parsed items in memory for
/// the life of the job, so the cap is also a memory commitment. 32 MiB of JSONL is on the order of
/// 50,000 short chat prompts, comfortably more than an operator submits by hand.
pub const MAX_FILE_BYTES: usize = 32 * 1024 * 1024;

/// The most items one batch may contain.
///
/// Not a policy so much as an arithmetic guard: each item costs its body plus a little bookkeeping,
/// so an unbounded item count is an unbounded memory commitment. The input file cap already implies a
/// number near this one; this makes the implication explicit and testable.
pub const MAX_ITEMS: usize = 50_000;

/// The largest response body the lane will hold for one item.
///
/// A batch result must be written to disk to be resumable, so it cannot be streamed past the relay.
/// The cap exists so that one pathological item — a model that ignores its output limit — cannot
/// consume the process's memory. An item over the cap fails on its own and leaves the batch running.
const MAX_ITEM_BODY_BYTES: usize = 16 * 1024 * 1024;

/// How many upstream attempts one item gets.
///
/// Three, because the failures worth retrying are the transient ones — a container that is booting, a
/// proxy that reset a connection, a provider returning 503 — and a fourth attempt buys nothing a
/// third does not. A 4xx is never retried: the provider has judged the request, and asking again is
/// how a batch turns one bad prompt into three charges.
const MAX_ATTEMPTS: u32 = 3;

/// The number of workers a relay starts for the batch lane.
///
/// Small and fixed. The worker count is not what limits throughput — the KV budget is — and every
/// worker is a task that can hold an upstream call open, so the useful size is "enough to keep the
/// endpoint busy", which four does for a single rented card.
pub const DEFAULT_WORKERS: usize = 4;

/// Environment variable that overrides [`DEFAULT_WORKERS`].
///
/// The useful operator question this answers is not "how many items can the lane hold" — the queue
/// holds as many as the disk does — but "how much of one card may a batch take". A batch shares the
/// endpoint with interactive requests and this lane has no priority class (see the module notes), so
/// the honest knob is how many items it may have in flight at once: setting this to `1` means a batch
/// occupies one slot and an interactive request always finds room, and setting it high means the
/// batch is allowed to be what keeps the card warm.
pub const WORKERS_ENV: &str = "MANAROUTER_BATCH_WORKERS";

/// Worker count from the environment, clamped to a range that cannot be a mistake.
///
/// Zero workers would be a lane that accepts batches and never runs them, and a thousand would be a
/// thousand concurrent upstream calls, which is the failure the fixed pool exists to prevent. A value
/// that is not a number is ignored with a warning rather than silently replaced, so an operator who
/// mistyped can see that their setting had no effect.
pub fn workers_from_env() -> usize {
    match std::env::var(WORKERS_ENV) {
        Ok(raw) => parse_workers(&raw),
        Err(_) => DEFAULT_WORKERS,
    }
}

/// The parsing half of [`workers_from_env`], so the clamping can be tested without touching the
/// process environment (which every test in this binary shares).
fn parse_workers(raw: &str) -> usize {
    match raw.trim().parse::<usize>() {
        Ok(0) => {
            tracing::warn!(
                value = %raw,
                "the batch worker count cannot be zero; keeping the default"
            );
            DEFAULT_WORKERS
        }
        Ok(workers) if workers > 64 => {
            tracing::warn!(
                value = %raw,
                "the batch worker count is above 64, which is more concurrent upstream calls than a batch should make; clamping"
            );
            64
        }
        Ok(workers) => workers,
        Err(_) => {
            tracing::warn!(
                variable = WORKERS_ENV,
                value = %raw,
                "the batch worker count is not a number; keeping the default"
            );
            DEFAULT_WORKERS
        }
    }
}

/// How long one item may wait for KV-token admission before it is written off as failed.
///
/// An endpoint being full is a waiting condition, not an error, but an item that waits forever is
/// worse than an item that fails: the operator would see a batch that never ends. Five minutes is
/// longer than any interactive request's queue and shorter than the batch's own patience.
const ADMISSION_WAIT: Duration = Duration::from_secs(300);

/// The first backoff step when an item is waiting for room or retrying a transient failure.
const RETRY_BASE: Duration = Duration::from_millis(250);

/// The longest backoff step. Also the bound on how late a cancellation can be noticed while a worker
/// is sleeping.
const RETRY_CAP: Duration = Duration::from_secs(8);

/// The path every batch item is dispatched to. The only endpoint this lane supports.
///
/// A batch item is by definition a non-streaming completion, so the chat surface is the one that
/// carries it; `POST /v1/batches` refuses any other `endpoint` rather than silently rewriting it.
const CHAT_COMPLETIONS_PATH: &str = "/v1/chat/completions";

/// A queue entry that runs no item, only the assembly step.
///
/// Used when a batch is recovered with every item already settled: the output and error files still
/// have to be built from the item files, and that work belongs to a worker like any other. `usize::MAX`
/// is not a valid item index, so [`BatchLane::run_task`]'s lookup fails and it goes straight to the
/// retire step, which is exactly the behaviour this needs.
const ASSEMBLE_ONLY: usize = usize::MAX;

/// The cards this build has measured, from the operator's own measurements.
///
/// These are not vendor figures and are not looked up anywhere: they are the two numbers the operator
/// weighed when choosing a card, and they only apply when the *declared price* of the endpoint
/// matches one of them. A provider priced at anything else gets `card: null` and `boot_cost_usd:
/// null` — an unknown boot fee is reported as unknown, because a guessed dollar figure is
/// indistinguishable from a measured one once it is in a report.
struct BootProfile {
    /// The card the operator measured.
    card: &'static str,
    /// Seconds from container start to first served token.
    boot_seconds: f64,
    /// The hourly price the measurement was taken at.
    hourly_usd: f64,
}

/// The share of a batch's bill above which the cold start, not the generation, is what the operator
/// paid for.
///
/// This is a policy constant and nothing more: the boot fee is a fixed cost per batch, so the same
/// tokens cost less in one large job than in several small ones, and the ratio says by how much. Half
/// the bill is where the boot has stopped being an overhead and become the product; it is deliberately
/// round, lives in exactly one place, and is reported next to the two dollar figures it is derived
/// from so that anyone who disagrees can recompute it.
const BOOT_DOMINANT_SHARE: f64 = 0.5;

/// The measured boot profiles. See [`BootProfile`].
const BOOT_PROFILES: [BootProfile; 2] = [
    BootProfile {
        card: "RTX PRO 6000",
        boot_seconds: 180.0,
        hourly_usd: 3.03,
    },
    BootProfile {
        card: "L40S",
        boot_seconds: 110.0,
        hourly_usd: 1.95,
    },
];

/// The card whose declared hourly price matches this one, or `None` if the price is not one this
/// build has a measurement for.
///
/// The tolerance is two cents an hour: wide enough that a price written as `0.000841666666` in
/// configuration still matches `3.03`, narrow enough that two different cards cannot be confused.
fn boot_profile(price_per_second: f64) -> Option<&'static BootProfile> {
    let hourly = price_per_second * 3600.0;
    BOOT_PROFILES
        .iter()
        .find(|profile| (hourly - profile.hourly_usd).abs() <= 0.02)
}

// --- Wire shapes ---------------------------------------------------------------------------

/// The lifecycle status of a batch, as the OpenAI-shaped API spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Accepted, items being validated. In this implementation every line is validated before the
    /// submit response is written, so a batch the client can see is already past this state.
    Validating,
    /// Items are running.
    InProgress,
    /// Every item has settled. Some of them may have failed: `request_counts` says how many.
    Completed,
    /// The batch could not run at all. A batch whose *items* failed is still `Completed`.
    Failed,
    /// Cancelled by the operator. Items that had already settled are kept.
    Cancelled,
    /// Past its completion window. Defined because the API shape needs it, never set by this
    /// implementation, which enforces no window.
    Expired,
}

impl Status {
    /// The wire spelling.
    fn as_str(self) -> &'static str {
        match self {
            Self::Validating => "validating",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Expired => "expired",
        }
    }

    /// Whether the batch will not change again.
    fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Expired
        )
    }
}

/// A file as stored on disk.
///
/// `deleted` is our own field, not OpenAI's: a deleted file is hidden from the API but its bytes are
/// kept, because a batch that references it may still be resumed after a crash and re-reading the
/// input file is how that works. Deleting the bytes would make a running batch unresumable, so the
/// trade is a little disk for the guarantee.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileMeta {
    id: String,
    bytes: u64,
    created_at: u64,
    #[serde(default)]
    filename: Option<String>,
    purpose: String,
    #[serde(default)]
    deleted: bool,
}

impl FileMeta {
    /// The file object the API hands back. `deleted` is deliberately not part of it.
    fn view(&self) -> Value {
        json!({
            "id": self.id,
            "object": "file",
            "bytes": self.bytes,
            "created_at": self.created_at,
            "filename": self.filename,
            "purpose": self.purpose,
        })
    }
}

/// A batch's persisted metadata.
///
/// This is what `GET /v1/batches/{id}` reads while the relay is running, and what a restart reloads.
/// The counters in it are a cache of what the item files say; see the module doc.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct JobMeta {
    id: String,
    input_file_id: String,
    endpoint: String,
    completion_window: String,
    created_at: u64,
    #[serde(default)]
    in_progress_at: Option<u64>,
    #[serde(default)]
    expires_at: Option<u64>,
    #[serde(default)]
    finalizing_at: Option<u64>,
    #[serde(default)]
    completed_at: Option<u64>,
    #[serde(default)]
    failed_at: Option<u64>,
    #[serde(default)]
    cancelling_at: Option<u64>,
    #[serde(default)]
    cancelled_at: Option<u64>,
    status: Status,
    /// How many items the input file held. Fixed at submit time.
    total: usize,
    #[serde(default)]
    completed: usize,
    #[serde(default)]
    failed: usize,
    #[serde(default)]
    metadata: Option<Value>,
    #[serde(default)]
    output_file_id: Option<String>,
    #[serde(default)]
    error_file_id: Option<String>,
    /// OpenAI's `errors` list. Only populated when the batch could not run at all.
    #[serde(default)]
    errors: Option<Value>,
    /// The cost report. Absent until the batch reaches a terminal state.
    #[serde(default)]
    usage: Option<BatchUsage>,
    /// The operator's ceiling for this batch, if they set one. Recorded at submit time and enforced
    /// between items; see [`BatchLane::run_task`].
    #[serde(default)]
    max_cost_usd: Option<f64>,
    /// Whether the cap stopped the batch. False for a batch with no cap, and false for one that ran
    /// to the end below its cap.
    #[serde(default)]
    budget_exceeded: bool,
    /// Why the batch stopped early, when it did: `cancelled` for an operator's cancellation and
    /// `max_cost_usd_exceeded` for the cap. Absent when nothing stopped it.
    #[serde(default)]
    stopped_reason: Option<String>,
}

impl JobMeta {
    /// The batch object the API hands back.
    fn view(&self) -> Value {
        let mut object = json!({
            "id": self.id,
            "object": "batch",
            "endpoint": self.endpoint,
            "input_file_id": self.input_file_id,
            "completion_window": self.completion_window,
            "status": self.status.as_str(),
            "created_at": self.created_at,
            "request_counts": {
                "total": self.total,
                "completed": self.completed,
                "failed": self.failed,
            },
            "metadata": self.metadata,
            "output_file_id": self.output_file_id,
            "error_file_id": self.error_file_id,
            "errors": self.errors,
            // The cap is echoed even when it is null: a client that polls a batch it did not submit
            // should be able to see that nothing was capped rather than infer it from an absence.
            "max_cost_usd": self.max_cost_usd,
            "budget_exceeded": self.budget_exceeded,
            "stopped_reason": self.stopped_reason,
        });
        // The timestamps OpenAI omits until they happen are omitted here too: a null `completed_at`
        // on a running batch is noise the operator has to filter out of every poll.
        let stamps = [
            ("in_progress_at", self.in_progress_at),
            ("expires_at", self.expires_at),
            ("finalizing_at", self.finalizing_at),
            ("completed_at", self.completed_at),
            ("failed_at", self.failed_at),
            ("cancelling_at", self.cancelling_at),
            ("cancelled_at", self.cancelled_at),
        ];
        for (name, value) in stamps {
            if let Some(value) = value {
                object[name] = json!(value);
            }
        }
        if let Some(usage) = &self.usage {
            object["usage"] = usage.view();
        }
        object
    }
}

/// What a batch cost, in the units that decide whether it was worth running.
///
/// This block is an extension: OpenAI's batch object has no `usage`, and a client that ignores unknown
/// fields is unaffected. Every field is either measured or `null`; nothing here is estimated. See the
/// module doc for why `boot_cost_usd` is separate from `cost_usd`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BatchUsage {
    /// Summed `prompt_tokens` over the items that settled, as the provider reported them.
    pub prompt_tokens: u64,
    /// Summed `completion_tokens` over the items that settled.
    pub completion_tokens: u64,
    /// `prompt_tokens + completion_tokens`.
    pub total_tokens: u64,
    /// Wall clock spent inside this batch's item attempts, including cold-start waits, admission
    /// waits and failed attempts. This is the honest measure of how long the card was occupied.
    pub gpu_seconds: f64,
    /// `null` when any settled item's endpoint has no configured `price_per_second`.
    pub cost_usd: Option<f64>,
    /// How many cold starts were observed while running the batch.
    pub cold_starts: u64,
    /// `cold_starts * boot_seconds * price`. Zero when nothing booted, which is a fee known exactly at
    /// every rate; otherwise `null` when the declared price matches no measured card profile. See
    /// [`BOOT_PROFILES`].
    pub boot_cost_usd: Option<f64>,
    /// The provider every settled item ran on, or `null` if they disagreed or were unknown.
    pub provider: Option<String>,
    /// The model every settled item ran on, or `null` if they disagreed or were unknown.
    pub model: Option<String>,
    /// The card, when the declared price identifies one. `null` otherwise.
    pub card: Option<String>,
}

impl BatchUsage {
    /// The wire shape. Costs are rounded to six decimals and seconds to three, purely so that a report
    /// is stable when compared to itself; the rounding is far below the precision of any billing
    /// system the operator reconciles against.
    fn view(&self) -> Value {
        // Derived from the two figures above rather than stored, so the verdict can never disagree
        // with the dollars it is derived from. Both halves are measured; the share is arithmetic.
        let share = match (self.boot_cost_usd, self.cost_usd) {
            (Some(boot), Some(cost)) if cost > 0.0 => Some(boot / cost),
            _ => None,
        };
        // The number a batch operator compares cards on. Both halves are measured, so this is
        // arithmetic over the bill rather than a throughput prediction; `billable` excludes the boot
        // fee because a card's steady-state cost per token is what the boot fee distorts.
        let per_million = |dollars: Option<f64>| match (dollars, self.completion_tokens) {
            (Some(dollars), tokens) if tokens > 0 => Some(dollars / tokens as f64 * 1_000_000.0),
            _ => None,
        };
        let all_in_per_million = per_million(self.cost_usd);
        let serving_per_million = per_million(match (self.cost_usd, self.boot_cost_usd) {
            (Some(cost), Some(boot)) => Some((cost - boot).max(0.0)),
            _ => None,
        });
        let verdict = match share {
            Some(share) if share > BOOT_DOMINANT_SHARE => "boot_dominates",
            Some(_) => "work_dominates",
            // No price, or a free endpoint: there is no bill to divide, and inventing a verdict from
            // an absent denominator is exactly the guess this module refuses to make.
            None => "unknown",
        };
        json!({
            "prompt_tokens": self.prompt_tokens,
            "completion_tokens": self.completion_tokens,
            "total_tokens": self.total_tokens,
            "gpu_seconds": round_to(self.gpu_seconds, 1_000.0),
            "cost_usd": self.cost_usd.map(|cost| round_to(cost, 1_000_000.0)),
            "cold_starts": self.cold_starts,
            "boot_cost_usd": self.boot_cost_usd.map(|cost| round_to(cost, 1_000_000.0)),
            // The answer to "was that batch too small to be worth a cold start", in the only form this
            // module can support: measured dollars, not a predicted token count.
            "boot_share_pct": share.map(|share| round_to(share * 100.0, 100.0)),
            // What a million output tokens cost, all in. Includes the boot fee, which is the point:
            // a batch that produced 120K tokens and spent as long booting as generating must not
            // look cheap because its serving rate was good.
            "usd_per_million_output_tokens": all_in_per_million
                .map(|value| round_to(value, 1_000_000.0)),
            // The same figure with the boot fee removed, which is what a card-to-card comparison
            // needs. `null` when either half is unpriced, never a guess.
            "serving_usd_per_million_output_tokens": serving_per_million
                .map(|value| round_to(value, 1_000_000.0)),
            "cold_start_verdict": verdict,
            "provider": self.provider,
            "model": self.model,
            "card": self.card,
        })
    }
}

/// An item's failure, as it appears in the error file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemError {
    /// A stable, greppable code: the relay's own code when the refusal was ours, otherwise the
    /// provider's code, or `upstream_error_<status>` when it gave none.
    pub code: String,
    /// What the provider (or the relay) said. Never a credential and never the request body.
    pub message: String,
}

/// One item's outcome, as persisted in `items/<index>.json`.
///
/// This is the unit of resumability *and* the source of the output file, so it carries the whole
/// answer rather than a reference to one: rebuilding `output.jsonl` from these files is what makes
/// the assembled output crash-consistent.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ItemResult {
    /// The id this item's line carries in the output file.
    item_id: String,
    /// The `custom_id` from the input file, preserved verbatim. This is the client's join key.
    custom_id: String,
    /// Whether the item produced an answer a client can use.
    ok: bool,
    /// The HTTP status of the last attempt, or 0 when no attempt was made.
    status: u16,
    /// How many upstream attempts were made.
    attempts: u32,
    /// The GPU-second ledger id of the last attempt, so a cost in the report can be traced to a row in
    /// `gpu_usage`.
    request_id: Option<String>,
    /// Wall clock spent in this item's attempts, including waits and failures.
    gpu_seconds: f64,
    /// Cold starts observed while this item was in flight.
    cold_starts: u32,
    /// Token counts as the provider reported them. `None` when it reported none.
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    /// Where the item was routed, for the usage summary. `None` when the model resolved to a combo, in
    /// which case the provider is chosen per attempt and this item claims no cost attribution.
    provider_id: Option<String>,
    model: Option<String>,
    /// The price the ledger billed at, if any.
    price_per_second: Option<f64>,
    /// The last response body, parsed. Preserved even for a failed item: it is what the operator needs
    /// in order to see *why* it failed.
    response: Option<Value>,
    /// Present exactly when `ok` is false.
    error: Option<ItemError>,
}

impl ItemResult {
    /// A skeleton result for one item, before the outcome is known.
    fn start(item: &BatchItem, facts: &PlanFacts, request_id: &str, attempts: u32) -> Self {
        Self {
            item_id: format!("batch_req_{}", mr_core::ids::new_id()),
            custom_id: item.custom_id.clone(),
            ok: false,
            status: 0,
            attempts,
            request_id: Some(request_id.to_owned()),
            gpu_seconds: 0.0,
            cold_starts: 0,
            prompt_tokens: None,
            completion_tokens: None,
            provider_id: facts.provider_id.clone(),
            model: facts.model.clone(),
            price_per_second: facts.price_per_second,
            response: None,
            error: None,
        }
    }

    /// The line this item contributes to the output file (successes) or error file (failures).
    fn line(&self) -> Value {
        let response = self.response.as_ref().map(|body| {
            json!({
                "status_code": self.status,
                "request_id": self.request_id,
                "body": body,
            })
        });
        json!({
            "id": self.item_id,
            "custom_id": self.custom_id,
            "response": response,
            "error": self.error,
        })
    }
}

/// Where one item would be routed, as the lane needs to know it before dispatch.
///
/// The lane cannot work this out itself: resolving a model to a provider means reading the routing
/// snapshot, which belongs to the request path. So the executor — which is the relay integration —
/// answers the question, and the lane does the admitting, the retrying and the accounting.
#[derive(Debug, Clone)]
pub struct ItemPlan {
    /// The endpoint the item will be sent to. Doubles as the admission-pool key, because the KV budget
    /// belongs to the endpoint.
    pub provider_id: String,
    /// The model, for the ledger row and the usage summary.
    pub model: String,
    /// The prompt size in tokens, as the relay estimates it. This is what is admitted against.
    pub tokens: u64,
    /// The endpoint's configured limits, copied from the same provider the request path reads.
    pub limits: KvBudgetLimits,
    /// The endpoint's configured price per second, if it declares one.
    pub price_per_second: Option<f64>,
}

/// One item, as the executor receives it.
#[derive(Debug)]
pub struct ItemRequest {
    /// The URI to dispatch. Only the path is meaningful; it is the chat completions surface.
    pub uri: Uri,
    /// The item body: the `body` object from the input line, re-serialised.
    pub body: Bytes,
    /// The provider whose KV budget the lane has already reserved, when it has.
    ///
    /// The request path must not reserve a second time for the same endpoint: that would double count
    /// the item's tokens against the pool and bill the same wall clock twice.
    pub pre_admitted: Option<String>,
}

/// A captured upstream answer to one item.
#[derive(Debug)]
pub struct ItemReply {
    /// The status the relay produced, in the OpenAI dialect the batch asked in.
    pub status: StatusCode,
    /// The response headers, for `retry-after`.
    pub headers: HeaderMap,
    /// The whole response body, buffered. A batch item is never a stream.
    pub body: Bytes,
    /// How many cold starts the executor observed while this item was in flight.
    pub cold_starts: u32,
}

/// What the batch lane needs from the relay in order to run an item.
///
/// A trait rather than a direct dependency on `server.rs` for two reasons: the lane's scheduler,
/// resumability and accounting are then testable without an HTTP server, routing snapshot or
/// upstream, and the seam makes it obvious that the lane owns *when* an item runs while the relay owns
/// *where* it goes.
pub trait ItemExecutor: Send + Sync + 'static {
    /// Where this item would be routed, or `None` when the lane must not pre-admit it.
    ///
    /// `None` is not an error. It means "this item has no single endpoint to reserve against" — a
    /// model that resolves to a combo is dispatched like any other request, with its own per-attempt
    /// admission, and the lane simply claims no cost attribution for it.
    fn plan(&self, body: &[u8]) -> Option<ItemPlan>;

    /// The GPU-second ledger to bill against, or `None` to admit without recording a row.
    fn store(&self) -> Option<Arc<mr_core::store::Store>>;

    /// Dispatch one item and buffer its answer.
    fn execute<'a>(&'a self, request: ItemRequest) -> ItemFuture<'a>;
}

/// The future one item's dispatch resolves to.
pub type ItemFuture<'a> = Pin<Box<dyn Future<Output = ItemReply> + Send + 'a>>;

// --- The lane ------------------------------------------------------------------------------

/// One submitted batch, in memory.
struct Job {
    /// The batch id, and the name of its directory.
    id: String,
    /// `jobs/<id>`.
    dir: PathBuf,
    /// Every item, in input order, held for the life of the job.
    ///
    /// Held in memory rather than re-read per item because the item bodies are needed once each and
    /// the file is capped: re-reading the input file 10,000 times to dispatch 10,000 items would be
    /// the expensive way to be frugal with memory. The exception is a restart, which re-reads once.
    items: Vec<BatchItem>,
    /// The metadata the API reads and the restart reloads.
    meta: Mutex<JobMeta>,
    /// Set by cancellation. Checked before dispatching an item, so the queue stops promptly.
    cancelled: AtomicBool,
    /// Woken on cancellation, so a worker sleeping in a backoff stops waiting out its delay.
    cancelled_notify: Notify,
    /// Items that have neither settled nor been skipped.
    ///
    /// The batch is finished when this reaches zero — which is the only completion condition that is
    /// correct under cancellation, where most of the items are skipped rather than run.
    outstanding: AtomicUsize,
    /// Dollars spent by the items that have settled so far, as reported.
    ///
    /// A running total rather than a predicted one: the cap is checked against it before an item is
    /// dispatched, so nothing here ever claims to know what an item will cost. An item whose endpoint
    /// declares no price adds nothing, which makes this a floor — it can let a batch overspend past
    /// its cap, and never the reverse.
    spent: Mutex<f64>,
    /// Set once the cap has stopped the run, so the batch's terminal status can say why.
    budget_stopped: AtomicBool,
}

impl Job {
    /// Record a settled item in the in-memory counters.
    fn record(&self, result: &ItemResult) {
        let mut meta = lock(&self.meta);
        if result.ok {
            meta.completed += 1;
        } else {
            meta.failed += 1;
        }
        // The cap is enforced against what has settled, so the ledger is updated in the same critical
        // section that counts the item: a dispatcher that read the two separately could see a count
        // of one with a spend of zero and let an item through.
        let spent = item_cost(result);
        drop(meta);
        if spent > 0.0 {
            *lock(&self.spent) += spent;
        }
    }

    /// Whether this batch has spent up to its cap.
    fn over_budget(&self) -> bool {
        self.over_budget_at(self.spent_at())
    }

    /// Dollars spent by settled items so far.
    fn spent_at(&self) -> f64 {
        *lock(&self.spent)
    }

    /// Whether a spend of `spent` has reached this batch's cap.
    ///
    /// Split out from [`Self::over_budget`] so a recovered batch can ask the question with the total
    /// it reloaded rather than the one in memory.
    fn over_budget_at(&self, spent: f64) -> bool {
        match lock(&self.meta).max_cost_usd {
            // A cap of zero cannot be written through the API — `parse_max_cost_usd` refuses it — but a
            // hand-edited job.json could hold one, and a batch that must spend nothing should stop.
            Some(cap) => spent >= cap,
            None => false,
        }
    }
}

/// One item waiting to be dispatched.
struct Task {
    job: Arc<Job>,
    index: usize,
}

/// The ready queue.
struct Queue {
    pending: Mutex<VecDeque<Task>>,
    notify: Notify,
    stopped: AtomicBool,
}

impl Queue {
    fn new() -> Self {
        Self {
            pending: Mutex::new(VecDeque::new()),
            notify: Notify::new(),
            stopped: AtomicBool::new(false),
        }
    }

    /// Add one item to the back of the queue.
    fn push(&self, job: Arc<Job>, index: usize) {
        lock(&self.pending).push_back(Task { job, index });
        // `notify_one`, not `notify_waiters`: it stores a permit when no worker is waiting, so a push
        // that lands between a worker's empty check and its `notified().await` is not lost. This is why
        // the wakeup needs no second lock and no polling timeout.
        self.notify.notify_one();
    }

    /// Take the next item, waiting until there is one or the queue is stopped.
    async fn pop(&self) -> Option<Task> {
        loop {
            if self.stopped.load(Ordering::SeqCst) {
                return None;
            }
            let next = lock(&self.pending).pop_front();
            if let Some(task) = next {
                return Some(task);
            }
            self.notify.notified().await;
        }
    }

    /// Stop the workers. Queued items stay queued and are not run.
    fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
}

/// The batch lane: files, batches, a worker pool and the cost report.
///
/// One instance per relay process, holding one lock-protected map of jobs. It is not a singleton so
/// that a test can have its own, rooted at its own directory.
pub struct BatchLane {
    /// The directory everything under this lane lives in.
    root: PathBuf,
    /// The provider admission pool. Shared with the request path, so a batch and an interactive request
    /// compete for the same KV budget instead of each believing it has the endpoint.
    pool: Arc<AdmissionPool>,
    /// How to plan and dispatch an item.
    executor: Arc<dyn ItemExecutor>,
    /// Files by id. Populated at startup from disk and updated on every write.
    files: Mutex<HashMap<String, FileMeta>>,
    /// Jobs by id.
    jobs: Mutex<HashMap<String, Arc<Job>>>,
    /// The ready queue.
    queue: Arc<Queue>,
    /// Whether workers have been started, so that starting them twice cannot double the pool.
    workers_started: AtomicBool,
}

impl BatchLane {
    /// Open a lane rooted at `root`, recovering whatever a previous process left behind.
    ///
    /// Recovery is deliberate and synchronous: it happens once, before the relay serves, and it reads
    /// only file metadata and unfinished batches. `workers` is the worker count to start; starting
    /// none is legitimate — the relay starts them after the executor can reach the shared state, see
    /// [`Self::start_workers`] — and the lane is functionally complete without them (submissions
    /// queue, nothing runs).
    pub fn open(
        root: PathBuf,
        pool: Arc<AdmissionPool>,
        executor: Arc<dyn ItemExecutor>,
        workers: usize,
    ) -> Arc<Self> {
        let lane = Arc::new(Self {
            root,
            pool,
            executor,
            files: Mutex::new(HashMap::new()),
            jobs: Mutex::new(HashMap::new()),
            queue: Arc::new(Queue::new()),
            workers_started: AtomicBool::new(false),
        });
        lane.recover();
        lane.start_workers(workers);
        lane
    }

    /// A lane with no disk state, no workers and no executor worth calling.
    ///
    /// Exists for embedding code that must hold an `AppState` but never serves a batch request — the
    /// relay's own unit tests. Its root is an unused directory under the system temporary directory and
    /// it never reads the operator's real state directory, so a test cannot see, or write, another's
    /// batches.
    pub fn inert() -> Arc<Self> {
        Arc::new(Self {
            root: std::env::temp_dir()
                .join(format!("manarouter-batch-inert-{}", mr_core::ids::new_id())),
            pool: Arc::new(AdmissionPool::new()),
            executor: Arc::new(InertExecutor),
            files: Mutex::new(HashMap::new()),
            jobs: Mutex::new(HashMap::new()),
            queue: Arc::new(Queue::new()),
            workers_started: AtomicBool::new(false),
        })
    }

    /// Start `workers` worker tasks, once.
    ///
    /// Separate from [`Self::open`] because the relay builds its state in two steps: the lane has to
    /// exist before the `AppState` can hold it, and the executor cannot dispatch anything until the
    /// `AppState` exists. Starting the workers after that handshake means a recovered batch can never
    /// be run by a worker that cannot reach the relay.
    pub fn start_workers(self: &Arc<Self>, workers: usize) {
        if workers == 0 || self.workers_started.swap(true, Ordering::SeqCst) {
            return;
        }
        if tokio::runtime::Handle::try_current().is_err() {
            // No runtime to spawn on. This is honest rather than fatal: the lane keeps working as a
            // queue that nothing drains, and a caller that can run tasks calls this again later.
            self.workers_started.store(false, Ordering::SeqCst);
            tracing::warn!(
                "the batch lane was asked to start workers outside a Tokio runtime; items will queue until one is available"
            );
            return;
        }
        for worker in 0..workers {
            let lane = Arc::clone(self);
            tokio::spawn(async move {
                tracing::debug!(worker, "batch worker started");
                while let Some(task) = lane.queue.pop().await {
                    lane.run_task(task).await;
                }
                tracing::debug!(worker, "batch worker stopped");
            });
        }
        tracing::info!(workers, "the batch lane started its workers");
    }

    /// Stop the workers. Queued items remain queued and are not run.
    pub fn shutdown(&self) {
        self.queue.stop();
    }

    /// Where this lane keeps its files. Exposed for diagnostics and tests.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// How many items are waiting to be dispatched.
    pub fn queued(&self) -> usize {
        lock(&self.queue.pending).len()
    }

    // --- Startup recovery ------------------------------------------------------------------

    /// Load files, batches and unfinished work from disk.
    ///
    /// Synchronous by design: it runs once, before the relay serves, from `startup.rs`. Doing it here
    /// rather than in a task means there is no window in which the relay answers
    /// `GET /v1/batches/{id}` for a batch it has not yet noticed.
    fn recover(&self) {
        self.recover_files();
        let jobs_dir = self.root.join("jobs");
        let entries = match std::fs::read_dir(&jobs_dir) {
            Ok(entries) => entries,
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(
                        dir = %jobs_dir.display(),
                        error = %error,
                        "the batch directory could not be listed; no batches were recovered"
                    );
                }
                return;
            }
        };
        let mut recovered = 0usize;
        for entry in entries.flatten() {
            if self.recover_job(&entry.path()) {
                recovered += 1;
            }
        }
        if recovered > 0 {
            tracing::info!(batches = recovered, "recovered unfinished batches");
        }
    }

    /// Load the file metadata index.
    fn recover_files(&self) {
        let files_dir = self.root.join("files");
        let Ok(entries) = std::fs::read_dir(&files_dir) else {
            return;
        };
        let mut files = lock(&self.files);
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            match std::fs::read(&path)
                .ok()
                .and_then(|raw| serde_json::from_slice::<FileMeta>(&raw).ok())
            {
                Some(meta) => {
                    files.insert(meta.id.clone(), meta);
                }
                None => {
                    tracing::warn!(path = %path.display(), "a file record could not be read and was skipped")
                }
            }
        }
    }

    /// Register a batch that will not change again: its items are not re-read, because nothing will
    /// run them, and `cancelled` is set so that no path can accidentally start one.
    fn register_terminal(&self, dir: &Path, meta: JobMeta) {
        let job = Arc::new(Job {
            id: meta.id.clone(),
            dir: dir.to_path_buf(),
            items: Vec::new(),
            meta: Mutex::new(meta),
            cancelled: AtomicBool::new(true),
            cancelled_notify: Notify::new(),
            outstanding: AtomicUsize::new(0),
            spent: Mutex::new(0.0),
            budget_stopped: AtomicBool::new(false),
        });
        lock(&self.jobs).insert(job.id.clone(), job);
    }

    /// Load one batch directory, queueing what still has to run.
    ///
    /// Returns whether the batch was registered. Every failure path here is a batch that cannot run: a
    /// `job.json` that will not parse is not recoverable and is logged, while a batch whose input file
    /// has disappeared is marked `failed` so that the operator is told rather than left polling a
    /// batch that will never move.
    fn recover_job(&self, dir: &Path) -> bool {
        let meta_path = dir.join("job.json");
        let Some(raw) = std::fs::read(&meta_path).ok() else {
            return false;
        };
        let Ok(mut meta) = serde_json::from_slice::<JobMeta>(&raw) else {
            tracing::warn!(path = %meta_path.display(), "a batch record could not be parsed and was not recovered");
            return false;
        };
        let id = meta.id.clone();
        if meta.status.is_terminal() {
            self.register_terminal(dir, meta);
            return true;
        }

        let items = match std::fs::read(self.file_body_path(&meta.input_file_id)) {
            Ok(bytes) => match parse_input(&bytes) {
                Ok(items) => items,
                Err(refusal) => {
                    // The input file survived but no longer parses. That is not something this process
                    // can repair, so the batch is failed rather than silently re-created from nothing.
                    meta.status = Status::Failed;
                    meta.failed_at = Some(now_seconds());
                    meta.errors = Some(json!([{
                        "code": refusal.code,
                        "message": refusal.message,
                    }]));
                    let _ = write_atomic(&meta_path, &json_bytes(&meta));
                    tracing::error!(
                        batch = %id,
                        "a recovered batch's input file no longer holds valid items; the batch was marked failed"
                    );
                    self.register_terminal(dir, meta);
                    return true;
                }
            },
            Err(error) => {
                meta.status = Status::Failed;
                meta.failed_at = Some(now_seconds());
                meta.errors = Some(json!([{
                    "code": "input_file_missing",
                    "message": format!("the input file could not be read after a restart: {error}"),
                }]));
                let _ = write_atomic(&meta_path, &json_bytes(&meta));
                tracing::error!(
                    batch = %id,
                    error = %error,
                    "a recovered batch's input file could not be read; the batch was marked failed"
                );
                self.register_terminal(dir, meta);
                return true;
            }
        };

        let settled = load_settled_sync(&dir.join("items"));
        meta.total = items.len();
        // What the batches that settled before the crash already spent, so a resumed batch's cap counts
        // money that was really spent rather than starting again from zero.
        let spent_before: f64 = settled.values().map(item_cost).sum();
        let job = Arc::new(Job {
            id,
            dir: dir.to_path_buf(),
            items,
            meta: Mutex::new(meta),
            cancelled: AtomicBool::new(false),
            cancelled_notify: Notify::new(),
            outstanding: AtomicUsize::new(0),
            spent: Mutex::new(spent_before),
            budget_stopped: AtomicBool::new(false),
        });

        let mut remaining = 0usize;
        for index in 0..job.items.len() {
            if settled.contains_key(&index) {
                continue;
            }
            job.outstanding.fetch_add(1, Ordering::SeqCst);
            remaining += 1;
            self.queue.push(Arc::clone(&job), index);
        }
        if remaining == 0 {
            // Every item had settled before the crash; only the assembly was lost. One task does the
            // assembly and nothing else.
            job.outstanding.fetch_add(1, Ordering::SeqCst);
            self.queue.push(Arc::clone(&job), ASSEMBLE_ONLY);
        }
        tracing::info!(
            batch = %job.id,
            settled = settled.len(),
            remaining,
            "resumed a batch"
        );
        lock(&self.jobs).insert(job.id.clone(), job);
        true
    }

    // --- Files -----------------------------------------------------------------------------

    /// The path of a file's metadata record.
    fn file_meta_path(&self, id: &str) -> PathBuf {
        self.root.join("files").join(format!("{id}.json"))
    }

    /// The path of a file's bytes.
    fn file_body_path(&self, id: &str) -> PathBuf {
        self.root.join("files").join(format!("{id}.jsonl"))
    }

    /// Store a file, returning its record.
    ///
    /// The bytes are written before the record: a record whose bytes are missing would be a file the
    /// API lists and cannot serve, while bytes with no record are invisible, and invisible is the safer
    /// half of a crash.
    async fn publish_file(
        &self,
        bytes: Vec<u8>,
        filename: Option<String>,
        purpose: &str,
    ) -> Result<FileMeta, String> {
        let meta = FileMeta {
            id: format!("file_{}", mr_core::ids::new_id()),
            bytes: bytes.len() as u64,
            created_at: now_seconds(),
            filename,
            purpose: purpose.to_owned(),
            deleted: false,
        };
        write_atomic_async(self.file_body_path(&meta.id), bytes).await?;
        write_atomic_async(self.file_meta_path(&meta.id), json_bytes(&meta)).await?;
        lock(&self.files).insert(meta.id.clone(), meta.clone());
        Ok(meta)
    }

    /// Store a file and return the object a client sees.
    ///
    /// Shared by `POST /v1/files` and by the resumable upload surface, which assembles its parts and
    /// hands the result here so that a file completed by upload is the same record as a file posted
    /// in one request — same id shape, same fields, same retrievability. The *purpose* restriction
    /// lives in the HTTP handler (`require_supported_purpose`) rather than here: `batch` and
    /// `fine-tune` both start from a file, and the lane's job is to store bytes, not to police what a
    /// client intends to do with them.
    pub(crate) async fn put_file(
        &self,
        bytes: Vec<u8>,
        filename: Option<String>,
        purpose: Option<String>,
    ) -> Result<Value, Refusal> {
        let purpose = purpose.unwrap_or_else(|| "batch".to_owned());
        match self.publish_file(bytes, filename, &purpose).await {
            Ok(meta) => Ok(meta.view()),
            Err(error) => Err(Refusal::internal(format!(
                "the file could not be stored: {error}"
            ))),
        }
    }

    /// The file record, or `None` when it does not exist or was deleted.
    fn file(&self, id: &str) -> Option<FileMeta> {
        let files = lock(&self.files);
        files.get(id).filter(|meta| !meta.deleted).cloned()
    }

    /// `GET /v1/files/{id}`.
    fn file_view(&self, id: &str) -> Option<Value> {
        self.file(id).map(|meta| meta.view())
    }

    /// The filename a client uploaded a file under, when it gave one.
    ///
    /// Fine-tuning staging sends the bytes on to a provider, and the name the operator chose is the
    /// name the provider's own file listing should show. It is never load-bearing: a file without a
    /// name is uploaded as `<id>.jsonl`.
    pub(crate) fn file_name(&self, id: &str) -> Option<String> {
        self.file(id).and_then(|meta| meta.filename)
    }

    /// `GET /v1/files/{id}/content`.
    ///
    /// Public within the crate because fine-tuning staging needs the same bytes: a training file
    /// this lane stored is the file the provider must be sent, and reading it through a second path
    /// would be a second definition of "where a file's bytes live".
    pub(crate) async fn file_bytes(&self, id: &str) -> Option<Vec<u8>> {
        self.file(id)?;
        read_async(self.file_body_path(id)).await.ok()
    }

    /// `GET /v1/files`.
    fn files_view(&self, limit: usize) -> Value {
        let mut all: Vec<FileMeta> = lock(&self.files)
            .values()
            .filter(|meta| !meta.deleted)
            .cloned()
            .collect();
        // Newest first, then by id so that a page boundary is stable when two files share a second.
        all.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        let has_more = all.len() > limit;
        all.truncate(limit);
        json!({
            "object": "list",
            "data": all.iter().map(FileMeta::view).collect::<Vec<_>>(),
            "first_id": all.first().map(|meta| meta.id.clone()),
            "last_id": all.last().map(|meta| meta.id.clone()),
            "has_more": has_more,
        })
    }

    /// `DELETE /v1/files/{id}`.
    async fn delete_file(&self, id: &str) -> Option<Result<Value, Refusal>> {
        let mut meta = self.file(id)?;
        meta.deleted = true;
        // The bytes stay on disk: a batch that references this file may still be resumed after a
        // crash, and re-reading the input file is how a resume knows what the items were.
        if let Err(error) = write_atomic_async(self.file_meta_path(id), json_bytes(&meta)).await {
            return Some(Err(Refusal::internal(format!(
                "the file could not be deleted: {error}"
            ))));
        }
        lock(&self.files).insert(meta.id.clone(), meta);
        Some(Ok(json!({ "id": id, "object": "file", "deleted": true })))
    }

    // --- Submitting ------------------------------------------------------------------------

    /// `POST /v1/batches`: validate, persist and queue. Returns immediately.
    async fn submit(&self, spec: BatchSpec) -> Result<Value, Refusal> {
        if spec.endpoint != CHAT_COMPLETIONS_PATH {
            return Err(Refusal::bad_request(
                "unsupported_endpoint",
                format!(
                    "endpoint \"{}\" is not supported; this lane runs \"{CHAT_COMPLETIONS_PATH}\" only",
                    spec.endpoint
                ),
            ));
        }
        if spec.completion_window != "24h" {
            return Err(Refusal::bad_request(
                "invalid_request_error",
                format!(
                    "completion_window \"{}\" is not supported; the only accepted value is \"24h\"",
                    spec.completion_window
                ),
            ));
        }
        let Some(file) = self.file(&spec.input_file_id) else {
            return Err(Refusal::not_found("file", &spec.input_file_id));
        };
        if file.purpose != "batch" {
            return Err(Refusal::bad_request(
                "invalid_request_error",
                format!(
                    "file \"{}\" was uploaded with purpose \"{}\"; a batch input file must have purpose \"batch\"",
                    file.id, file.purpose
                ),
            ));
        }
        let bytes = read_async(self.file_body_path(&file.id))
            .await
            .map_err(|error| {
                Refusal::internal(format!("the input file could not be read: {error}"))
            })?;
        // Every line is validated before the batch is accepted, so that the client learns about a
        // malformed line inside the `POST` it made rather than by polling a batch that fails later.
        let items = parse_input(&bytes)?;

        let now = now_seconds();
        let id = format!("batch_{}", mr_core::ids::new_id());
        let dir = self.root.join("jobs").join(&id);
        let meta = JobMeta {
            id: id.clone(),
            input_file_id: file.id.clone(),
            endpoint: spec.endpoint,
            completion_window: spec.completion_window,
            created_at: now,
            in_progress_at: None,
            // Recorded, and not enforced: nothing expires a batch. See the module doc.
            expires_at: Some(now + 24 * 60 * 60),
            finalizing_at: None,
            completed_at: None,
            failed_at: None,
            cancelling_at: None,
            cancelled_at: None,
            status: Status::Validating,
            total: items.len(),
            completed: 0,
            failed: 0,
            metadata: spec.metadata,
            output_file_id: None,
            error_file_id: None,
            errors: None,
            usage: None,
            max_cost_usd: spec.max_cost_usd,
            budget_exceeded: false,
            stopped_reason: None,
        };
        let job = Arc::new(Job {
            id: id.clone(),
            dir: dir.clone(),
            items,
            meta: Mutex::new(meta.clone()),
            cancelled: AtomicBool::new(false),
            cancelled_notify: Notify::new(),
            outstanding: AtomicUsize::new(0),
            spent: Mutex::new(0.0),
            budget_stopped: AtomicBool::new(false),
        });
        // The record goes down before the first item is queued: a batch that exists in memory but not
        // on disk would vanish, and its items would be dispatched against nothing.
        write_atomic_async(dir.join("job.json"), json_bytes(&meta))
            .await
            .map_err(|error| {
                Refusal::internal(format!("the batch could not be stored: {error}"))
            })?;
        lock(&self.jobs).insert(id, Arc::clone(&job));
        for index in 0..job.items.len() {
            job.outstanding.fetch_add(1, Ordering::SeqCst);
            self.queue.push(Arc::clone(&job), index);
        }
        tracing::info!(
            batch = %job.id,
            items = job.items.len(),
            queued = self.queued(),
            "accepted a batch"
        );
        Ok(meta.view())
    }

    // --- Reading and cancelling ------------------------------------------------------------

    /// `GET /v1/batches/{id}`.
    fn batch_view(&self, id: &str) -> Option<Value> {
        let job = lock(&self.jobs).get(id).cloned()?;
        let meta = lock(&job.meta).clone();
        Some(meta.view())
    }

    /// `GET /v1/batches`.
    fn batches_view(&self, limit: usize, after: Option<&str>) -> Value {
        let mut all: Vec<JobMeta> = lock(&self.jobs)
            .values()
            .map(|job| lock(&job.meta).clone())
            .collect();
        all.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        if let Some(after) = after {
            if let Some(position) = all.iter().position(|meta| meta.id == after) {
                all.drain(..=position);
            }
        }
        let has_more = all.len() > limit;
        all.truncate(limit);
        json!({
            "object": "list",
            "data": all.iter().map(JobMeta::view).collect::<Vec<_>>(),
            "first_id": all.first().map(|meta| meta.id.clone()),
            "last_id": all.last().map(|meta| meta.id.clone()),
            "has_more": has_more,
        })
    }

    /// `POST /v1/batches/{id}/cancel`.
    ///
    /// Cancellation stops the queue, not the wire: an item already dispatched is allowed to finish and
    /// is kept, because it was paid for. What is *not* run is everything still queued.
    async fn cancel(&self, id: &str) -> Option<Result<Value, Refusal>> {
        let job = lock(&self.jobs).get(id).cloned()?;
        if lock(&job.meta).status.is_terminal() {
            let status = lock(&job.meta).status.as_str().to_owned();
            return Some(Err(Refusal::bad_request(
                "invalid_request_error",
                format!("batch \"{id}\" is already {status} and cannot be cancelled"),
            )));
        }
        job.cancelled.store(true, Ordering::SeqCst);
        job.cancelled_notify.notify_waiters();
        let snapshot = {
            let mut meta = lock(&job.meta);
            if meta.cancelling_at.is_none() {
                meta.cancelling_at = Some(now_seconds());
            }
            meta.clone()
        };
        if let Err(error) = self.save_job(&job, &snapshot).await {
            tracing::warn!(batch = %job.id, error = %error, "the cancellation could not be written to disk");
        }
        tracing::info!(batch = %job.id, "batch cancelled; items already dispatched will finish");
        Some(Ok(snapshot.view()))
    }

    // --- Running ---------------------------------------------------------------------------

    /// Dispatch one queued item, then retire it.
    async fn run_task(&self, task: Task) {
        let Task { job, index } = task;
        if job.cancelled.load(Ordering::SeqCst) {
            // A cancelled batch runs nothing more. The item is retired without a result file, so it is
            // counted neither as completed nor as failed: it never happened.
            self.retire(&job).await;
            return;
        }
        if job.over_budget() {
            // The same treatment for a batch that has spent its cap: the item that was not dispatched
            // has no result, so it neither counts nor fails. Items already in flight when the cap was
            // reached are not interrupted — their cost is being incurred either way, and killing one
            // would not refund it.
            job.budget_stopped.store(true, Ordering::SeqCst);
            self.retire(&job).await;
            return;
        }
        self.mark_in_progress(&job).await;
        // `ASSEMBLE_ONLY` lands here: there is no such item, so the task becomes nothing but a retire.
        let Some(item) = job.items.get(index).cloned() else {
            self.retire(&job).await;
            return;
        };
        if let Some(result) = self.run_item(&job, index, &item).await {
            if let Err(error) = self.persist_item(&job, index, &result).await {
                // The item is not counted either: a result that is not on disk would be re-run after a
                // restart and counted twice, and a wrong total is worse than a missing one.
                tracing::error!(
                    batch = %job.id,
                    index,
                    error = %error,
                    "a batch item result could not be written; the item will be re-run if the relay restarts"
                );
                self.retire(&job).await;
                return;
            }
            job.record(&result);
        }
        self.retire(&job).await;
    }

    /// One item has stopped occupying a worker. The last one assembles the batch.
    async fn retire(&self, job: &Arc<Job>) {
        if job.outstanding.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.finish(job).await;
        }
    }

    /// Move a batch from `validating` to `in_progress` the first time an item runs.
    async fn mark_in_progress(&self, job: &Arc<Job>) {
        let snapshot = {
            let mut meta = lock(&job.meta);
            if meta.status != Status::Validating {
                return;
            }
            meta.status = Status::InProgress;
            meta.in_progress_at = Some(now_seconds());
            meta.clone()
        };
        if let Err(error) = self.save_job(job, &snapshot).await {
            tracing::warn!(batch = %job.id, error = %error, "the batch state could not be written");
        }
    }

    /// Persist one settled item. Atomic: temporary file, then rename.
    async fn persist_item(
        &self,
        job: &Arc<Job>,
        index: usize,
        result: &ItemResult,
    ) -> Result<(), String> {
        let path = job.dir.join("items").join(format!("{index}.json"));
        write_atomic_async(path, json_bytes(result)).await
    }

    /// Run one item to a settled outcome, or to `None` when the batch was cancelled under it.
    ///
    /// The whole retry policy of the lane lives here: which failures are worth another attempt, how long
    /// to wait, and when waiting turns into failing.
    async fn run_item(&self, job: &Arc<Job>, index: usize, item: &BatchItem) -> Option<ItemResult> {
        let mut attempts: u32 = 0;
        let mut gpu_seconds = 0.0f64;
        let mut cold_starts = 0u32;
        let mut admission_waited = Duration::ZERO;

        loop {
            if job.cancelled.load(Ordering::SeqCst) {
                return None;
            }
            attempts += 1;
            // The id is the item's identity in the GPU-second ledger. It is derived from the index and
            // the attempt number rather than randomised so that a resumed attempt writes the same row:
            // `record_gpu_usage` ignores a duplicate, so a crash-and-resume cannot bill the same wall
            // clock twice.
            let request_id = format!("batch:{}:{}:{}", job.id, index, attempts);
            let planned = self.executor.plan(&item.body);
            let facts = PlanFacts::of(planned.as_ref());

            let mut lease: Option<KvBudgetLease> = match planned.as_ref() {
                Some(plan) => match self
                    .admit(job, plan, &request_id, &mut admission_waited)
                    .await
                {
                    Admitted::Leased(lease) => Some(lease),
                    Admitted::Refused(error) => {
                        let mut result = ItemResult::start(item, &facts, &request_id, attempts);
                        result.gpu_seconds = gpu_seconds;
                        result.cold_starts = cold_starts;
                        result.error = Some(error);
                        return Some(result);
                    }
                    Admitted::Cancelled => return None,
                },
                // No single endpoint to reserve against: a combo, or a model this build cannot
                // resolve. The item is still dispatched — the request path applies its own admission —
                // but it claims no cost attribution.
                None => None,
            };

            let sent = Instant::now();
            let reply = self
                .executor
                .execute(ItemRequest {
                    uri: chat_uri(),
                    body: item.body.clone(),
                    pre_admitted: facts.provider_id.clone(),
                })
                .await;
            // The wait between reservation and answer, plus the answer itself. A cold start is inside
            // this number, and so is a failed attempt: the card was occupied either way.
            let elapsed = sent.elapsed().as_secs_f64();
            gpu_seconds += elapsed;
            cold_starts += reply.cold_starts;

            if reply.status.is_success() {
                // Settle the ledger row before the lease is dropped, so a successful attempt is billed
                // and a failed one is not. Everything else about the lease is unchanged.
                if let Some(lease) = lease.as_mut() {
                    lease.mark_success();
                }
            }
            // Released here, before the retry decision: the endpoint's KV tokens must not stay reserved
            // while the item sleeps off a failure.
            drop(lease.take());

            let body = response_body(&reply.body);
            let (prompt_tokens, completion_tokens) = usage_tokens(&body);
            if reply.status.is_success() {
                let mut result = ItemResult::start(item, &facts, &request_id, attempts);
                result.ok = true;
                result.status = reply.status.as_u16();
                result.gpu_seconds = gpu_seconds;
                result.cold_starts = cold_starts;
                result.prompt_tokens = prompt_tokens;
                result.completion_tokens = completion_tokens;
                result.response = Some(body);
                return Some(result);
            }

            let error = item_error(&reply, &body);
            if attempts >= MAX_ATTEMPTS || !retryable(&reply, &error) {
                let mut result = ItemResult::start(item, &facts, &request_id, attempts);
                result.status = reply.status.as_u16();
                result.gpu_seconds = gpu_seconds;
                result.cold_starts = cold_starts;
                result.response = Some(body);
                result.error = Some(error);
                return Some(result);
            }
            let delay = retry_delay(attempts, &reply.headers);
            tracing::warn!(
                batch = %job.id,
                index,
                status = reply.status.as_u16(),
                attempt = attempts,
                wait_ms = delay.as_millis() as u64,
                "a batch item attempt failed transiently; retrying"
            );
            wait_or_cancel(job, delay).await;
        }
    }

    /// Reserve the endpoint's KV tokens for one attempt, waiting out a full endpoint.
    async fn admit(
        &self,
        job: &Arc<Job>,
        plan: &ItemPlan,
        request_id: &str,
        waited: &mut Duration,
    ) -> Admitted {
        loop {
            let billing = self.executor.store().map(|store| GpuBilling {
                store,
                provider_id: plan.provider_id.clone(),
                model: plan.model.clone(),
                price_per_second: plan.price_per_second,
                started: Instant::now(),
            });
            let denial = match self.pool.reserve(
                plan.provider_id.clone(),
                request_id.to_owned(),
                plan.tokens,
                plan.limits,
                billing,
            ) {
                Ok(lease) => return Admitted::Leased(lease),
                Err(denial) => denial,
            };

            // Two of the four denials are properties of the item, not of the moment: the item will
            // never fit, so waiting would only delay the report.
            if matches!(denial, KvBudgetDenial::ContextExceeded) {
                return Admitted::Refused(ItemError {
                    code: "kv_budget_exceeded".to_owned(),
                    message: format!(
                        "the item is larger than the model context configured for endpoint \"{}\" ({} tokens estimated)",
                        plan.provider_id, plan.tokens
                    ),
                });
            }
            if matches!(denial, KvBudgetDenial::PoolTooSmall) {
                return Admitted::Refused(ItemError {
                    code: "kv_budget_exceeded".to_owned(),
                    message: format!(
                        "the KV-token budget configured for endpoint \"{}\" is smaller than this item ({} tokens estimated)",
                        plan.provider_id, plan.tokens
                    ),
                });
            }

            // The endpoint is full *now*. Waiting is the correct answer: the batch's whole purpose is to
            // use the card it is paying for, and refusing an item because the other items are using it
            // would make the pool smaller than the operator configured. A concurrency limit reads the
            // same way from here.
            let reason = if matches!(denial, KvBudgetDenial::PoolFull) {
                "pool-full"
            } else {
                "concurrency-full"
            };
            let delay = backoff(*waited + RETRY_BASE);
            if *waited + delay > ADMISSION_WAIT {
                return Admitted::Refused(ItemError {
                    code: "kv_budget_exceeded".to_owned(),
                    message: format!(
                        "endpoint \"{}\" stayed at its KV-token limit for {} seconds, so the item was never dispatched",
                        plan.provider_id,
                        ADMISSION_WAIT.as_secs()
                    ),
                });
            }
            tracing::debug!(
                batch = %job.id,
                provider = %plan.provider_id,
                reason,
                wait_ms = delay.as_millis() as u64,
                "waiting for KV-token admission"
            );
            wait_or_cancel(job, delay).await;
            if job.cancelled.load(Ordering::SeqCst) {
                return Admitted::Cancelled;
            }
            *waited += delay;
        }
    }

    // --- Finishing -------------------------------------------------------------------------

    /// Assemble output, error file and the cost report for a batch whose items have all settled.
    async fn finish(&self, job: &Arc<Job>) {
        let settled = self.load_settled(job).await;
        let results: Vec<ItemResult> = settled.values().cloned().collect();

        let mut output: Vec<u8> = Vec::new();
        let mut errors: Vec<u8> = Vec::new();
        let mut completed = 0usize;
        let mut failed = 0usize;
        for index in 0..job.items.len() {
            let Some(result) = settled.get(&index) else {
                // Never dispatched: the batch was cancelled, or the relay died and the run was resumed
                // without it. Either way it is not a result.
                continue;
            };
            if result.ok {
                completed += 1;
            } else {
                failed += 1;
                tracing::debug!(
                    batch = %job.id,
                    index,
                    attempts = result.attempts,
                    code = result.error.as_ref().map(|error| error.code.as_str()).unwrap_or("unknown"),
                    "a batch item ended in an error"
                );
            }
            let mut line = match serde_json::to_vec(&result.line()) {
                Ok(line) => line,
                Err(error) => {
                    tracing::warn!(batch = %job.id, index, error = %error, "an item result could not be encoded");
                    continue;
                }
            };
            line.push(b'\n');
            // Order is guaranteed by this loop, not by the workers: the output file is a projection of
            // the item files in index order, so the results come back in the order they were submitted
            // with their original `custom_id`s.
            if result.ok {
                output.extend_from_slice(&line);
            } else {
                errors.extend_from_slice(&line);
            }
        }

        let output_file_id = if completed + failed > 0 {
            match self
                .publish_file(
                    output,
                    Some(format!("{}-output.jsonl", job.id)),
                    "batch_output",
                )
                .await
            {
                Ok(meta) => Some(meta.id),
                Err(error) => {
                    tracing::error!(batch = %job.id, error = %error, "the batch output file could not be written");
                    None
                }
            }
        } else {
            None
        };
        // An error file exists only when something failed: an empty one would make a client do a
        // pointless download to learn there was nothing to read.
        let error_file_id = if failed > 0 {
            match self
                .publish_file(
                    errors,
                    Some(format!("{}-errors.jsonl", job.id)),
                    "batch_output",
                )
                .await
            {
                Ok(meta) => Some(meta.id),
                Err(error) => {
                    tracing::error!(batch = %job.id, error = %error, "the batch error file could not be written");
                    None
                }
            }
        } else {
            None
        };

        let usage = summarize(&results);
        let cancelled = job.cancelled.load(Ordering::SeqCst);
        // Derived from the ledger as well as from the flag: a batch that spent exactly its cap with no
        // item left to skip never set the flag, and it still ran to its limit.
        let budget_exceeded = job.budget_stopped.load(Ordering::SeqCst) || job.over_budget();
        let now = now_seconds();
        let snapshot = {
            let mut meta = lock(&job.meta);
            meta.completed = completed;
            meta.failed = failed;
            meta.output_file_id = output_file_id;
            meta.error_file_id = error_file_id;
            meta.usage = usage;
            meta.finalizing_at = Some(now);
            if cancelled || budget_exceeded {
                // `cancelled` is the vocabulary this API has for "stopped before every item ran", and it
                // fits: the batch was not finished, it was cut off. The reason tells the two apart.
                meta.status = Status::Cancelled;
                meta.cancelled_at = Some(now);
                meta.budget_exceeded = budget_exceeded;
                meta.stopped_reason = Some(if budget_exceeded {
                    "max_cost_usd_exceeded".to_owned()
                } else {
                    "cancelled".to_owned()
                });
            } else {
                meta.status = Status::Completed;
                meta.completed_at = Some(now);
            }
            meta.clone()
        };
        if let Err(error) = self.save_job(job, &snapshot).await {
            tracing::error!(batch = %job.id, error = %error, "the finished batch could not be written to disk");
        }
        tracing::info!(
            batch = %job.id,
            status = snapshot.status.as_str(),
            completed,
            failed,
            gpu_seconds = snapshot.usage.as_ref().map(|usage| usage.gpu_seconds).unwrap_or(0.0),
            "a batch finished"
        );
    }

    /// Read every settled item of a job from disk.
    async fn load_settled(&self, job: &Arc<Job>) -> BTreeMap<usize, ItemResult> {
        let dir = job.dir.join("items");
        match tokio::task::spawn_blocking(move || load_settled_sync(&dir)).await {
            Ok(settled) => settled,
            Err(error) => {
                tracing::error!(batch = %job.id, error = %error, "the item results could not be read back");
                BTreeMap::new()
            }
        }
    }

    /// Write a batch's metadata.
    async fn save_job(&self, job: &Arc<Job>, meta: &JobMeta) -> Result<(), String> {
        write_atomic_async(job.dir.join("job.json"), json_bytes(meta)).await
    }
}

// --- Free helpers --------------------------------------------------------------------------

/// The result of asking the pool for room.
enum Admitted {
    /// Room was reserved.
    Leased(KvBudgetLease),
    /// The item cannot run, and waiting will not change that.
    Refused(ItemError),
    /// The batch was cancelled while the item waited.
    Cancelled,
}

/// The routing facts of one attempt that the result must remember.
#[derive(Debug, Default, Clone)]
struct PlanFacts {
    provider_id: Option<String>,
    model: Option<String>,
    price_per_second: Option<f64>,
}

impl PlanFacts {
    fn of(plan: Option<&ItemPlan>) -> Self {
        match plan {
            Some(plan) => Self {
                provider_id: Some(plan.provider_id.clone()),
                model: Some(plan.model.clone()),
                price_per_second: plan.price_per_second,
            },
            None => Self::default(),
        }
    }
}

/// The URI every item is dispatched to.
///
/// `from_static` panics only for a literal that is not a valid URI, which is a programming error caught
/// by the first test that runs, not a runtime-fallible path.
fn chat_uri() -> Uri {
    Uri::from_static(CHAT_COMPLETIONS_PATH)
}

/// The upstream reply's body as JSON, or as text when it is not JSON at all.
///
/// A batch item's answer is written to a file the client reads, so it must be preserved even when it is
/// not parseable: dropping it would leave a client with a status code and no reason.
fn response_body(bytes: &[u8]) -> Value {
    match serde_json::from_slice(bytes) {
        Ok(body) => body,
        Err(_) => Value::String(String::from_utf8_lossy(bytes).into_owned()),
    }
}

/// The token counts a provider reported, if it reported any.
fn usage_tokens(body: &Value) -> (Option<u64>, Option<u64>) {
    let usage = body.get("usage");
    let get = |name: &str| -> Option<u64> {
        usage
            .and_then(|usage| usage.get(name))
            .and_then(Value::as_u64)
    };
    (get("prompt_tokens"), get("completion_tokens"))
}

/// Turn a failed reply into an item error.
///
/// The message is the provider's own, truncated, and only ever the *error* text: an error body is the
/// one part of an exchange that is safe to keep, and the item's request body — which is the client's
/// prompt — is never copied into a log or an error message.
fn item_error(reply: &ItemReply, body: &Value) -> ItemError {
    let error = body.get("error");
    // OpenAI's shape is `{error: {message, type, code}}`; Anthropic's, after translation, is
    // `{error: {type, message}}`. Both are read, because either can arrive here.
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            error
                .and_then(|error| error.get("type"))
                .and_then(Value::as_str)
                .map(|kind| format!("the provider reported \"{kind}\""))
        })
        .unwrap_or_else(|| {
            let text = String::from_utf8_lossy(&reply.body).trim().to_owned();
            if text.is_empty() {
                format!("the provider returned {} with no body", reply.status)
            } else {
                text.chars().take(200).collect()
            }
        });
    let code = error
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("upstream_error_{}", reply.status.as_u16()));
    ItemError { code, message }
}

/// Whether a failed attempt is worth repeating.
///
/// The rule is the operator's: a 5xx is the provider saying "not now"; a 4xx is the provider saying
/// "this request is wrong", and repeating it spends money to be told so again. Two exceptions, both
/// about *whose* fault the status is:
///
/// - the relay's own `429 no_usable_accounts` (every account of the endpoint is cooling down) is
///   relay-side transience wearing a client-error status, so it is retried;
/// - a `503 kv_budget_exceeded` is local admission refusing the item at dispatch time, which is also a
///   property of the moment. It is retried with the standard backoff, and [`BatchLane::admit`] is what
///   waits when the endpoint is genuinely full.
fn retryable(reply: &ItemReply, error: &ItemError) -> bool {
    if reply.status.is_server_error() || reply.status == StatusCode::REQUEST_TIMEOUT {
        return true;
    }
    if reply.status == StatusCode::TOO_MANY_REQUESTS {
        return error.code == "no_usable_accounts";
    }
    error.code == "kv_budget_exceeded" && reply.status == StatusCode::SERVICE_UNAVAILABLE
}

/// How long to wait before an attempt's retry.
///
/// A provider that says how long to wait is obeyed, capped: the cap exists because a mistaken
/// `retry-after` must not park a worker for the rest of the batch.
fn retry_delay(attempt: u32, headers: &HeaderMap) -> Duration {
    headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|seconds| Duration::from_secs(seconds).min(RETRY_CAP))
        .unwrap_or_else(|| backoff(RETRY_BASE * attempt.max(1)))
}

/// Exponential backoff, capped.
fn backoff(waited: Duration) -> Duration {
    waited.saturating_mul(2).min(RETRY_CAP)
}

/// Wait, or return early when the batch is cancelled.
///
/// A notification that arrives between a worker's cancellation check and its await is not remembered,
/// so the flag is checked again by the caller afterwards. The cost of that race is one backoff step —
/// at most [`RETRY_CAP`] — not a stuck worker.
async fn wait_or_cancel(job: &Job, delay: Duration) {
    tokio::select! {
        () = tokio::time::sleep(delay) => {}
        () = job.cancelled_notify.notified() => {}
    }
}

/// Read every settled item of a batch directory, keyed by input index.
///
/// A file that cannot be read or parsed is treated as absent, which means the item is re-run. That is
/// the right direction to fail in: re-running an item costs one call, while treating a corrupt result
/// as a result would put a wrong answer in the output file forever.
fn load_settled_sync(dir: &Path) -> BTreeMap<usize, ItemResult> {
    let mut settled = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return settled;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(index) = name
            .strip_suffix(".json")
            .and_then(|index| index.parse::<usize>().ok())
        else {
            continue;
        };
        match std::fs::read(entry.path())
            .ok()
            .and_then(|raw| serde_json::from_slice::<ItemResult>(&raw).ok())
        {
            Some(result) => {
                settled.insert(index, result);
            }
            None => tracing::warn!(
                path = %entry.path().display(),
                "an item result could not be read; the item will be re-run"
            ),
        }
    }
    settled
}

/// Summarise settled items into the cost report.
///
/// Returns `None` when nothing settled, because a batch that produced no results has no cost to
/// report — and `0.0` would be a lie, since the endpoint may well have booted for nothing.
/// What one settled item cost, by the same arithmetic the report uses.
///
/// `gpu_seconds * price` plus the boot fee when the declared price identifies a measured card. A
/// cold start whose card this build has no measurement for contributes its GPU seconds and no boot
/// fee, so the figure is a floor rather than a guess. `None` when the endpoint declared no price:
/// an unpriced item has no cost this module can state, and inventing one would move the cap.
fn item_cost(result: &ItemResult) -> f64 {
    let Some(price) = result.price_per_second else {
        return 0.0;
    };
    let boot = boot_profile(price)
        .map(|profile| f64::from(result.cold_starts) * profile.boot_seconds * price)
        .unwrap_or(0.0);
    result.gpu_seconds * price + boot
}

fn summarize(results: &[ItemResult]) -> Option<BatchUsage> {
    if results.is_empty() {
        return None;
    }
    let mut usage = BatchUsage::default();
    let mut cost = 0.0f64;
    let mut priced = true;
    let mut boot_cost = 0.0f64;
    let mut boots_known = true;
    let mut card: Option<String> = None;
    let mut card_mixed = false;
    let mut provider: Option<String> = None;
    let mut provider_mixed = false;
    let mut model: Option<String> = None;
    let mut model_mixed = false;

    for result in results {
        usage.gpu_seconds += result.gpu_seconds;
        usage.cold_starts += u64::from(result.cold_starts);
        usage.prompt_tokens += result.prompt_tokens.unwrap_or(0);
        usage.completion_tokens += result.completion_tokens.unwrap_or(0);

        match result.price_per_second {
            Some(price) => {
                // The two halves of the fee, kept apart so the report says which one the money went to.
                // Their sum is exactly `gpu_seconds * price`.
                cost += result.gpu_seconds * price;
                match boot_profile(price) {
                    Some(profile) => {
                        boot_cost += f64::from(result.cold_starts) * profile.boot_seconds * price;
                        match &card {
                            None => card = Some(profile.card.to_owned()),
                            Some(current) if current != profile.card => card_mixed = true,
                            _ => {}
                        }
                    }
                    // A price this build has no measurement for: the running cost is still known, the
                    // boot fee is not, and the report says so rather than guessing.
                    None => boots_known = false,
                }
            }
            None => {
                priced = false;
                boots_known = false;
            }
        }

        match (&provider, &result.provider_id) {
            (_, None) => provider_mixed = true,
            (None, Some(value)) => provider = Some(value.clone()),
            (Some(current), Some(value)) if current != value => provider_mixed = true,
            _ => {}
        }
        match (&model, &result.model) {
            (_, None) => model_mixed = true,
            (None, Some(value)) => model = Some(value.clone()),
            (Some(current), Some(value)) if current != value => model_mixed = true,
            _ => {}
        }
    }

    usage.total_tokens = usage.prompt_tokens + usage.completion_tokens;
    // A partial total presented as a total is worse than no total, so an unpriced item makes the whole
    // cost figure unknown.
    usage.cost_usd = if priced { Some(cost) } else { None };
    // Zero cold starts is a fee of exactly zero at every rate, so it is reported as zero rather than as
    // unknown: not booting anything is not a price this build fails to recognise. Otherwise the fee
    // needs both a price and a known boot time, and is absent rather than guessed when either is missing.
    usage.boot_cost_usd = if usage.cold_starts == 0 {
        Some(0.0)
    } else if priced && boots_known {
        Some(boot_cost)
    } else {
        None
    };
    usage.card = if card_mixed { None } else { card };
    usage.provider = if provider_mixed { None } else { provider };
    usage.model = if model_mixed { None } else { model };
    Some(usage)
}

/// Parse an input file into items.
///
/// Permissive about what the body contains and strict about the three fields the lane itself needs: a
/// `custom_id` to join on, a `body` object to send, and a `model` to route by. A `stream: true` item is
/// refused because a batch item that streams cannot be persisted, and silently downgrading it would
/// change what the client asked for.
fn parse_input(bytes: &[u8]) -> Result<Vec<BatchItem>, Refusal> {
    let mut items: Vec<BatchItem> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    for (number, line) in bytes.split(|byte| *byte == b'\n').enumerate() {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.iter().all(|byte| byte.is_ascii_whitespace()) {
            continue;
        }
        let value: Value = serde_json::from_slice(line).map_err(|error| {
            Refusal::bad_request(
                "invalid_request_error",
                format!("line {} is not valid JSON: {error}", number + 1),
            )
        })?;
        let custom_id = value
            .get("custom_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                Refusal::bad_request(
                    "invalid_request_error",
                    format!("line {} has no string \"custom_id\"", number + 1),
                )
            })?;
        if let Some(first) = seen.insert(custom_id.to_owned(), number + 1) {
            return Err(Refusal::bad_request(
                "invalid_request_error",
                format!(
                    "line {} repeats custom_id \"{custom_id}\", which line {first} already used; results are joined on custom_id and it must be unique",
                    number + 1
                ),
            ));
        }
        let body = value
            .get("body")
            .filter(|body| body.is_object())
            .ok_or_else(|| {
                Refusal::bad_request(
                    "invalid_request_error",
                    format!("line {} has no object \"body\"", number + 1),
                )
            })?;
        if body.get("model").and_then(Value::as_str).is_none() {
            return Err(Refusal::bad_request(
                "invalid_request_error",
                format!("line {} has no string \"body.model\"", number + 1),
            ));
        }
        if body.get("stream").and_then(Value::as_bool) == Some(true) {
            return Err(Refusal::bad_request(
                "invalid_request_error",
                format!(
                    "line {} asks for \"stream\": true; a batch item must be a non-streaming request",
                    number + 1
                ),
            ));
        }
        let body = serde_json::to_vec(body).map_err(|error| {
            Refusal::bad_request(
                "invalid_request_error",
                format!(
                    "line {} has a body that cannot be encoded: {error}",
                    number + 1
                ),
            )
        })?;
        // Refused here rather than when the item runs: a body over the cap cannot be answered within
        // the memory this lane will spend on one item, and finding that out after 900 items have
        // already been charged is worse than finding it out at submit time.
        if body.len() > MAX_ITEM_BODY_BYTES {
            return Err(Refusal::bad_request(
                "invalid_request_error",
                format!(
                    "line {} has a request body of {} bytes, over the {MAX_ITEM_BODY_BYTES}-byte cap for one batch item",
                    number + 1,
                    body.len()
                ),
            ));
        }
        items.push(BatchItem {
            custom_id: custom_id.to_owned(),
            body: Bytes::from(body),
        });
        if items.len() > MAX_ITEMS {
            return Err(Refusal::bad_request(
                "invalid_request_error",
                format!("the input file holds more than {MAX_ITEMS} items"),
            ));
        }
    }
    if items.is_empty() {
        return Err(Refusal::bad_request(
            "invalid_request_error",
            "the input file holds no items",
        ));
    }
    Ok(items)
}

// --- A refusal the client sees --------------------------------------------------------------

/// A refusal from the batch lane, in the shape the relay's error responses take.
///
/// Shared with the router's other local surfaces (uploads, vector stores, responses, fine-tuning)
/// so that every one of them answers an error in the same envelope as the request path.
#[derive(Debug)]
pub(crate) struct Refusal {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl Refusal {
    pub(crate) fn bad_request(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code,
            message: message.into(),
        }
    }

    pub(crate) fn not_found(what: &str, id: &str) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "invalid_request_error",
            message: format!("no {what} with id \"{id}\""),
        }
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal_error",
            message: message.into(),
        }
    }

    /// A refusal with a status the three constructors above do not cover.
    ///
    /// Used by surfaces that share this envelope but answer with a status of their own — a vector
    /// store with no embedder is `409`, an attach past the chunk cap is `413` — so that every error
    /// on the relay still has the same shape.
    pub(crate) fn with_status(
        status: StatusCode,
        code: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    pub(crate) fn into_response(self) -> Response {
        crate::server::error_response(self.status, Dialect::OpenAi, self.code, &self.message)
    }
}

// --- HTTP handlers --------------------------------------------------------------------------

/// `POST /v1/files`.
///
/// Two body shapes are accepted, because both are in real use. `multipart/form-data` is what OpenAI's
/// own examples and every SDK send (`-F purpose=batch -F file=@input.jsonl`), and a raw body with the
/// filename and purpose in the query string is what a shell client sends when it would rather not
/// build a multipart envelope. The multipart form wins when both are present: it is the documented
/// shape, so it is the one that decides.
///
/// The file's *name* is metadata here, not a path. It is stored next to the file id and never used to
/// open anything, so a name containing `../` cannot escape the state directory.
pub async fn create_file(
    State(app): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<FileParams>,
    request: Request,
) -> Response {
    if let Some(denied) = crate::server::authorize_batch_request(&app, &headers) {
        return denied;
    }
    let is_form = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim_start().starts_with("multipart/form-data"))
        .unwrap_or(false);
    let (bytes, filename, purpose) = if is_form {
        let form = match Multipart::from_request(request, &()).await {
            Ok(form) => form,
            Err(error) => {
                return Refusal::bad_request(
                    "invalid_request_error",
                    format!("the upload form could not be read: {error}"),
                )
                .into_response()
            }
        };
        match read_upload_form(form).await {
            Ok(parts) => parts,
            Err(refusal) => return refusal.into_response(),
        }
    } else {
        let bytes = match crate::server::read_body(request.into_body(), MAX_FILE_BYTES).await {
            Ok(bytes) => bytes,
            Err(error) => return error.into_response(),
        };
        (bytes.to_vec(), params.filename, params.purpose)
    };
    if let Err(refusal) = require_supported_purpose(purpose.as_deref()) {
        return refusal.into_response();
    }
    match app.batch.put_file(bytes, filename, purpose).await {
        Ok(view) => crate::server::json_response(StatusCode::OK, view),
        Err(refusal) => refusal.into_response(),
    }
}

/// A file uploaded to this lane must declare a purpose the lane can serve.
///
/// Two purposes are served, because two families start from a file: `batch` (a JSONL of requests,
/// read line by line when a job runs) and `fine-tune` (a JSONL of examples, stored here and staged
/// into a provider's own `/v1/files` when a job is created — see `server::fine_tuning_body`). A file
/// with any other purpose would be accepted and then have nothing that could ever read it, so it is
/// refused at the door with the two purposes named.
///
/// The check sits at the HTTP layer, not in [`BatchLane::put_file`], because the resumable upload
/// surface completes into the same function after its own longer-lived session has been created.
fn require_supported_purpose(purpose: Option<&str>) -> Result<(), Refusal> {
    match purpose {
        None | Some("batch") | Some("fine-tune") => Ok(()),
        Some(purpose) => Err(Refusal::bad_request(
            "invalid_request_error",
            format!(
                "purpose \"{purpose}\" is not supported here; a file must be uploaded with purpose \"batch\" or \"fine-tune\""
            ),
        )),
    }
}

/// Read an OpenAI-style upload form into `(bytes, filename, purpose)`.
///
/// Only the `file` part carries bytes; `purpose` is a short text field. Extra parts are read and
/// dropped rather than refused, because a client library that adds a field this lane does not use is
/// still sending a valid upload.
async fn read_upload_form(
    mut form: Multipart,
) -> Result<(Vec<u8>, Option<String>, Option<String>), Refusal> {
    let mut bytes: Option<Vec<u8>> = None;
    let mut filename: Option<String> = None;
    let mut purpose: Option<String> = None;
    loop {
        let field = match form.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => {
                return Err(Refusal::bad_request(
                    "invalid_request_error",
                    format!("the upload form could not be read: {error}"),
                ))
            }
        };
        let name = field.name().unwrap_or_default().to_owned();
        match name.as_str() {
            "file" => {
                if bytes.is_some() {
                    return Err(Refusal::bad_request(
                        "invalid_request_error",
                        "the upload form must contain exactly one \"file\" part",
                    ));
                }
                filename = field.file_name().map(str::to_owned);
                match field.bytes().await {
                    Ok(part) => bytes = Some(part.to_vec()),
                    Err(error) => {
                        return Err(Refusal::bad_request(
                            "invalid_request_error",
                            format!("the \"file\" part could not be read: {error}"),
                        ))
                    }
                }
            }
            "purpose" => match field.text().await {
                Ok(text) => purpose = Some(text),
                Err(error) => {
                    return Err(Refusal::bad_request(
                        "invalid_request_error",
                        format!("the \"purpose\" field could not be read: {error}"),
                    ))
                }
            },
            _ => {}
        }
    }
    match bytes {
        Some(bytes) => Ok((bytes, filename, purpose)),
        None => Err(Refusal::bad_request(
            "invalid_request_error",
            "the upload form has no \"file\" part",
        )),
    }
}

/// `GET /v1/files`.
pub async fn list_files(
    State(app): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<ListParams>,
) -> Response {
    if let Some(denied) = crate::server::authorize_batch_request(&app, &headers) {
        return denied;
    }
    crate::server::json_response(StatusCode::OK, app.batch.files_view(limit(&params)))
}

/// `GET /v1/files/{id}`.
pub async fn retrieve_file(
    State(app): State<AppState>,
    headers: HeaderMap,
    RoutePath(id): RoutePath<String>,
) -> Response {
    if let Some(denied) = crate::server::authorize_batch_request(&app, &headers) {
        return denied;
    }
    match app.batch.file_view(&id) {
        Some(view) => crate::server::json_response(StatusCode::OK, view),
        None => Refusal::not_found("file", &id).into_response(),
    }
}

/// `GET /v1/files/{id}/content`.
pub async fn file_content(
    State(app): State<AppState>,
    headers: HeaderMap,
    RoutePath(id): RoutePath<String>,
) -> Response {
    if let Some(denied) = crate::server::authorize_batch_request(&app, &headers) {
        return denied;
    }
    match app.batch.file_bytes(&id).await {
        Some(bytes) => {
            let mut response = Response::new(AxumBody::from(bytes));
            *response.status_mut() = StatusCode::OK;
            response.headers_mut().insert(
                "content-type",
                axum::http::HeaderValue::from_static("application/jsonl"),
            );
            response
        }
        None => Refusal::not_found("file", &id).into_response(),
    }
}

/// `DELETE /v1/files/{id}`.
pub async fn delete_file(
    State(app): State<AppState>,
    headers: HeaderMap,
    RoutePath(id): RoutePath<String>,
) -> Response {
    if let Some(denied) = crate::server::authorize_batch_request(&app, &headers) {
        return denied;
    }
    match app.batch.delete_file(&id).await {
        Some(Ok(view)) => crate::server::json_response(StatusCode::OK, view),
        Some(Err(refusal)) => refusal.into_response(),
        None => Refusal::not_found("file", &id).into_response(),
    }
}

/// `POST /v1/batches`.
pub async fn create_batch(
    State(app): State<AppState>,
    headers: HeaderMap,
    body: AxumBody,
) -> Response {
    if let Some(denied) = crate::server::authorize_batch_request(&app, &headers) {
        return denied;
    }
    let bytes = match crate::server::read_body(body, MAX_FILE_BYTES).await {
        Ok(bytes) => bytes,
        Err(error) => return error.into_response(),
    };
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(error) => {
            return Refusal::bad_request(
                "invalid_request_error",
                format!("the request body is not valid JSON: {error}"),
            )
            .into_response()
        }
    };
    let Some(input_file_id) = value.get("input_file_id").and_then(Value::as_str) else {
        return Refusal::bad_request(
            "invalid_request_error",
            "the request body has no string \"input_file_id\"",
        )
        .into_response();
    };
    let spec = BatchSpec {
        input_file_id: input_file_id.to_owned(),
        endpoint: value
            .get("endpoint")
            .and_then(Value::as_str)
            .unwrap_or(CHAT_COMPLETIONS_PATH)
            .to_owned(),
        completion_window: value
            .get("completion_window")
            .and_then(Value::as_str)
            .unwrap_or("24h")
            .to_owned(),
        metadata: value.get("metadata").cloned(),
        max_cost_usd: match parse_max_cost_usd(value.get("max_cost_usd")) {
            Ok(cap) => cap,
            Err(refusal) => return refusal.into_response(),
        },
    };
    match app.batch.submit(spec).await {
        Ok(view) => crate::server::json_response(StatusCode::OK, view),
        Err(refusal) => refusal.into_response(),
    }
}

/// `GET /v1/batches`.
pub async fn list_batches(
    State(app): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<ListParams>,
) -> Response {
    if let Some(denied) = crate::server::authorize_batch_request(&app, &headers) {
        return denied;
    }
    crate::server::json_response(
        StatusCode::OK,
        app.batch
            .batches_view(limit(&params), params.after.as_deref()),
    )
}

/// `GET /v1/batches/{id}`.
pub async fn retrieve_batch(
    State(app): State<AppState>,
    headers: HeaderMap,
    RoutePath(id): RoutePath<String>,
) -> Response {
    if let Some(denied) = crate::server::authorize_batch_request(&app, &headers) {
        return denied;
    }
    match app.batch.batch_view(&id) {
        Some(view) => crate::server::json_response(StatusCode::OK, view),
        None => Refusal::not_found("batch", &id).into_response(),
    }
}

/// `POST /v1/batches/{id}/cancel`.
pub async fn cancel_batch(
    State(app): State<AppState>,
    headers: HeaderMap,
    RoutePath(id): RoutePath<String>,
) -> Response {
    if let Some(denied) = crate::server::authorize_batch_request(&app, &headers) {
        return denied;
    }
    match app.batch.cancel(&id).await {
        Some(Ok(view)) => crate::server::json_response(StatusCode::OK, view),
        Some(Err(refusal)) => refusal.into_response(),
        None => Refusal::not_found("batch", &id).into_response(),
    }
}

/// What `POST /v1/batches` accepted.
struct BatchSpec {
    input_file_id: String,
    endpoint: String,
    completion_window: String,
    metadata: Option<Value>,
    /// The operator's spending ceiling for this batch, in dollars, or `None` for no ceiling.
    max_cost_usd: Option<f64>,
}

/// The largest cap this lane will accept, in dollars.
///
/// A cap exists to stop a batch from costing more than the operator expected, so the number has to be
/// a real one. It is deliberately not read from the environment or a config file: a ceiling that a
/// deployment sets for everybody is a different feature from one the operator sets for this batch.
const MAX_COST_CEILING: f64 = 1000.0;

/// Read `max_cost_usd` from a batch request body.
///
/// Absent and `null` both mean "no cap", which is why they are not an error. Everything else must be
/// a finite number of dollars in `(0, MAX_COST_CEILING]`: a negative cap would stop before the first
/// item, a zero one would do the same while looking like a valid request, and `NaN` compares false
/// against every limit, so a cap that could never trip is worse than a refusal.
fn parse_max_cost_usd(value: Option<&Value>) -> Result<Option<f64>, Refusal> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let Some(cap) = value.as_f64() else {
        return Err(Refusal::bad_request(
            "invalid_request_error",
            format!("\"max_cost_usd\" must be a number of dollars, not {value}"),
        ));
    };
    if !cap.is_finite() || cap <= 0.0 || cap > MAX_COST_CEILING {
        return Err(Refusal::bad_request(
            "invalid_request_error",
            format!(
                "\"max_cost_usd\" must be greater than zero and at most {MAX_COST_CEILING}, not {cap}"
            ),
        ));
    }
    Ok(Some(cap))
}

/// Query parameters of the file upload form this lane accepts.
#[derive(Debug, Deserialize)]
pub struct FileParams {
    filename: Option<String>,
    purpose: Option<String>,
}

/// Query parameters of the list routes.
#[derive(Debug, Deserialize)]
pub struct ListParams {
    limit: Option<usize>,
    after: Option<String>,
}

/// The page size, bounded so that one request cannot ask for an unbounded list.
fn limit(params: &ListParams) -> usize {
    params.limit.unwrap_or(20).clamp(1, 100)
}

// --- Small shared utilities -----------------------------------------------------------------

/// One line of an input file.
#[derive(Debug, Clone)]
struct BatchItem {
    custom_id: String,
    body: Bytes,
}

/// A lock, ignoring poisoning.
///
/// A panic inside a critical section is a bug, but the state it guards here is a map of ids and a
/// metadata record: reading it is still coherent, and refusing to serve the batch lane forever after
/// one panic elsewhere would be a worse outcome than continuing with slightly stale metadata.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The current wall clock, in seconds. Saturates at the epoch rather than failing.
fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

/// Serialise a value that is known to be serialisable.
///
/// The lane's own records are plain structs of strings, numbers and JSON values, so a failure here is
/// impossible in practice; falling back to an empty object keeps a record unreadable-but-present
/// rather than failing the request that was in flight.
fn json_bytes<T: Serialize>(value: &T) -> Vec<u8> {
    serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec())
}

/// Write a file atomically: a temporary file in the same directory, then a rename.
///
/// The rename is what makes an item file the truth: a reader either sees the whole record or no record,
/// never half of one. The temporary name is per-write so that two workers writing different items of
/// the same batch cannot collide.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let temporary = parent.join(format!(".{}.tmp", mr_core::ids::new_id()));
    std::fs::write(&temporary, bytes).map_err(|error| error.to_string())?;
    match std::fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            // Leaving the temporary file behind would accumulate junk; failing to remove it is not
            // worth reporting twice, so it is ignored.
            let _ = std::fs::remove_file(&temporary);
            Err(error.to_string())
        }
    }
}

/// [`write_atomic`], off the async runtime's worker threads.
///
/// Batch files can be tens of megabytes; writing one inside an async task would block a thread that the
/// interactive request path shares. `spawn_blocking` moves it to the blocking pool, which is what that
/// pool exists for.
async fn write_atomic_async(path: PathBuf, bytes: Vec<u8>) -> Result<(), String> {
    match tokio::task::spawn_blocking(move || write_atomic(&path, &bytes)).await {
        Ok(result) => result,
        Err(error) => Err(format!("the write task failed: {error}")),
    }
}

/// Read a file, off the async runtime's worker threads.
async fn read_async(path: PathBuf) -> Result<Vec<u8>, String> {
    match tokio::task::spawn_blocking(move || std::fs::read(&path)).await {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(error)) => Err(error.to_string()),
        Err(error) => Err(format!("the read task failed: {error}")),
    }
}

/// Round to a fixed number of decimal places, for a stable report.
fn round_to(value: f64, scale: f64) -> f64 {
    (value * scale).round() / scale
}

// --- The executor used by a lane that never runs anything -----------------------------------

/// An executor for [`BatchLane::inert`]: it plans nothing and dispatches nothing.
///
/// A lane built this way exists only so that embedding code can hold a `BatchLane`; a request that
/// reached it would get a 500 saying so rather than a panic.
struct InertExecutor;

impl ItemExecutor for InertExecutor {
    fn plan(&self, _body: &[u8]) -> Option<ItemPlan> {
        None
    }

    fn store(&self) -> Option<Arc<mr_core::store::Store>> {
        None
    }

    fn execute<'a>(&'a self, _request: ItemRequest) -> ItemFuture<'a> {
        Box::pin(async {
            ItemReply {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                headers: HeaderMap::new(),
                body: Bytes::from_static(
                    br#"{"error":{"message":"the batch lane is not wired to a relay","type":"internal_error","code":"internal_error"}}"#,
                ),
                cold_starts: 0,
            }
        })
    }
}

// --- Tests ---------------------------------------------------------------------------------

/// Tests for the batch lane.
///
/// The lane runs unattended, so nothing here is mocked above the lane: each test opens a real lane on
/// a temporary directory, uploads a real JSONL file through the real upload path, submits it through
/// the real submission path, and lets real worker tasks run the items. The only stand-in is the
/// upstream itself — [`Stub`] answers each attempt with a scripted status, a delay, or a park — which
/// keeps every test off the network, off a GPU, and away from the operator's state directory.
#[cfg(test)]
mod tests {
    use super::*;
    use mr_core::ids::Millis;
    use mr_core::store::Store;
    use tokio::sync::Semaphore;

    /// How long a test waits for the lane to reach a state before declaring it stuck.
    const PATIENCE: Duration = Duration::from_secs(30);

    /// KV limits that never deny anything: the tests that care about admission say so explicitly.
    const UNLIMITED: KvBudgetLimits = KvBudgetLimits {
        pool_tokens: None,
        max_context: None,
        max_concurrent: None,
    };

    type Planner = Arc<dyn Fn(&[u8]) -> Option<ItemPlan> + Send + Sync>;
    type Answerer = Arc<dyn Fn(&str, usize, &ItemRequest) -> Answer + Send + Sync>;

    /// What the stub upstream does with one attempt.
    enum Answer {
        /// Answer at once.
        Now(ItemReply),
        /// Take this long, then answer: lets a test put later items ahead of earlier ones.
        Later(Duration, ItemReply),
        /// Hold the attempt until the test releases a permit: keeps an item in flight on purpose.
        Parked(ItemReply),
    }

    /// A scripted stand-in for the relay's upstream.
    ///
    /// It answers by `body.model`, which each test uses as an item tag, and counts attempts per tag so
    /// that "fail twice, then succeed" is a two-line test rather than a fake provider.
    struct Stub {
        planner: Planner,
        answerer: Answerer,
        store: Option<Arc<Store>>,
        calls: AtomicUsize,
        seen: Mutex<HashMap<String, usize>>,
        in_flight: AtomicUsize,
        peak: AtomicUsize,
        parked: AtomicUsize,
        gate: Arc<Semaphore>,
        admitted: Mutex<Vec<Option<String>>>,
        models: Mutex<Vec<String>>,
    }

    impl Stub {
        fn new(planner: Planner, answerer: Answerer) -> Self {
            Self {
                planner,
                answerer,
                store: None,
                calls: AtomicUsize::new(0),
                seen: Mutex::new(HashMap::new()),
                in_flight: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
                parked: AtomicUsize::new(0),
                gate: Arc::new(Semaphore::new(0)),
                admitted: Mutex::new(Vec::new()),
                models: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl ItemExecutor for Stub {
        fn plan(&self, body: &[u8]) -> Option<ItemPlan> {
            (self.planner)(body)
        }

        fn store(&self) -> Option<Arc<Store>> {
            self.store.clone()
        }

        fn execute<'a>(&'a self, request: ItemRequest) -> ItemFuture<'a> {
            Box::pin(async move {
                let model = model_of(&request.body);
                let nth = {
                    let mut seen = lock(&self.seen);
                    let nth = seen.entry(model.clone()).or_insert(0);
                    *nth += 1;
                    *nth - 1
                };
                self.calls.fetch_add(1, Ordering::SeqCst);
                lock(&self.models).push(model.clone());
                lock(&self.admitted).push(request.pre_admitted.clone());
                let in_flight = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(in_flight, Ordering::SeqCst);

                let reply = match (self.answerer)(&model, nth, &request) {
                    Answer::Now(reply) => reply,
                    Answer::Later(delay, reply) => {
                        tokio::time::sleep(delay).await;
                        reply
                    }
                    Answer::Parked(reply) => {
                        let _ = self.parked.fetch_add(1, Ordering::SeqCst);
                        // A permit is the test's permission for this attempt to finish. `forget` is
                        // deliberate: a released attempt must not make the next parked one pass for
                        // free, so each release costs exactly one permit.
                        if let Ok(permit) = self.gate.acquire().await {
                            permit.forget();
                        }
                        let _ = self.parked.fetch_sub(1, Ordering::SeqCst);
                        reply
                    }
                };
                self.in_flight.fetch_sub(1, Ordering::SeqCst);
                reply
            })
        }
    }

    // --- Helpers ---------------------------------------------------------------------------

    fn model_of(body: &[u8]) -> String {
        let value: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
        value["model"].as_str().unwrap_or_default().to_owned()
    }

    fn reply(status: u16, body: Value) -> ItemReply {
        ItemReply {
            status: StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            headers: HeaderMap::new(),
            body: Bytes::from(serde_json::to_vec(&body).unwrap_or_default()),
            cold_starts: 0,
        }
    }

    fn ok_reply(model: &str, prompt: u64, completion: u64) -> ItemReply {
        reply(
            200,
            json!({
                "id": format!("chatcmpl-{model}"),
                "object": "chat.completion",
                "model": model,
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "ok"},
                    "finish_reason": "stop",
                }],
                "usage": {
                    "prompt_tokens": prompt,
                    "completion_tokens": completion,
                    "total_tokens": prompt + completion,
                },
            }),
        )
    }

    fn failure_reply(status: u16, code: &str, message: &str) -> ItemReply {
        reply(
            status,
            json!({"error": {"message": message, "type": "upstream_error", "code": code}}),
        )
    }

    fn with_cold_start(mut reply: ItemReply, cold_starts: u32) -> ItemReply {
        reply.cold_starts = cold_starts;
        reply
    }

    /// A planner that gives every item the same estimate, limits and price, and its own model name.
    fn following(tokens: u64, limits: KvBudgetLimits, price: Option<f64>) -> Planner {
        Arc::new(move |body: &[u8]| {
            Some(ItemPlan {
                provider_id: "local".to_owned(),
                model: model_of(body),
                tokens,
                limits,
                price_per_second: price,
            })
        })
    }

    fn input_line(custom_id: &str, model: &str) -> String {
        serde_json::to_string(&json!({
            "custom_id": custom_id,
            "method": "POST",
            "url": CHAT_COMPLETIONS_PATH,
            "body": {"model": model, "messages": [{"role": "user", "content": "hello"}]},
        }))
        .unwrap_or_default()
    }

    fn input_body(items: &[(&str, &str)]) -> Vec<u8> {
        let mut body = String::new();
        for (custom_id, model) in items {
            body.push_str(&input_line(custom_id, model));
            body.push('\n');
        }
        body.into_bytes()
    }

    fn stub_lane(
        root: &Path,
        pool: Arc<AdmissionPool>,
        stub: &Arc<Stub>,
        workers: usize,
    ) -> Arc<BatchLane> {
        let executor: Arc<dyn ItemExecutor> = Arc::clone(stub) as Arc<dyn ItemExecutor>;
        BatchLane::open(root.to_path_buf(), pool, executor, workers)
    }

    async fn submit(lane: &Arc<BatchLane>, items: &[(&str, &str)]) -> String {
        let file = lane
            .put_file(input_body(items), Some("input.jsonl".to_owned()), None)
            .await
            .ok()
            .expect("the input file is accepted");
        let input_file_id = file["id"]
            .as_str()
            .expect("a stored file has an id")
            .to_owned();
        let view = lane
            .submit(BatchSpec {
                max_cost_usd: None,
                input_file_id,
                endpoint: CHAT_COMPLETIONS_PATH.to_owned(),
                completion_window: "24h".to_owned(),
                metadata: None,
            })
            .await
            .ok()
            .expect("the batch is accepted");
        assert_eq!(view["object"], "batch");
        // Submission is a queue operation: it returns before anything has run.
        assert_eq!(view["status"], "validating");
        assert_eq!(count_of(&view, "total"), items.len() as u64);
        view["id"].as_str().expect("a batch has an id").to_owned()
    }

    async fn wait_for(lane: &Arc<BatchLane>, id: &str, want: impl Fn(&Value) -> bool) -> Value {
        let start = Instant::now();
        loop {
            let view = lane.batch_view(id).expect("the batch is known");
            if want(&view) {
                return view;
            }
            assert!(
                start.elapsed() < PATIENCE,
                "the batch did not reach the expected state within {PATIENCE:?}: {view}"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    async fn wait_terminal(lane: &Arc<BatchLane>, id: &str) -> Value {
        wait_for(lane, id, |view| {
            matches!(
                view["status"].as_str(),
                Some("completed" | "failed" | "cancelled" | "expired")
            )
        })
        .await
    }

    async fn wait_until(label: &str, mut condition: impl FnMut() -> bool) {
        let start = Instant::now();
        while !condition() {
            assert!(start.elapsed() < PATIENCE, "timed out waiting for {label}");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    async fn result_lines(lane: &Arc<BatchLane>, file_id: &str) -> Vec<Value> {
        let bytes = lane
            .file_bytes(file_id)
            .await
            .expect("a result file is readable");
        let text = String::from_utf8_lossy(&bytes).into_owned();
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("a result line is JSON"))
            .collect()
    }

    fn custom_ids(lines: &[Value]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line["custom_id"]
                    .as_str()
                    .expect("a result line carries its custom_id")
                    .to_owned()
            })
            .collect()
    }

    fn count_of(view: &Value, name: &str) -> u64 {
        view["request_counts"][name].as_u64().unwrap_or(u64::MAX)
    }

    fn item_record(job_dir: &Path, index: usize) -> ItemResult {
        let path = job_dir.join("items").join(format!("{index}.json"));
        let raw = std::fs::read(&path).unwrap_or_default();
        serde_json::from_slice(&raw)
            .unwrap_or_else(|error| panic!("{} is not an item record: {error}", path.display()))
    }

    fn refusal_of_input(body: &[u8]) -> Refusal {
        match parse_input(body) {
            Ok(items) => panic!("the input file was accepted with {} items", items.len()),
            Err(refusal) => refusal,
        }
    }

    fn refusal_of_submit(outcome: Result<Value, Refusal>) -> Refusal {
        match outcome {
            Ok(view) => panic!("the submission was accepted: {view}"),
            Err(refusal) => refusal,
        }
    }

    /// A settled item result, for the accounting tests that do not need a lane.
    fn settled(
        index: usize,
        model: &str,
        seconds: f64,
        cold_starts: u32,
        price: Option<f64>,
        prompt: Option<u64>,
        completion: Option<u64>,
    ) -> ItemResult {
        let item = BatchItem {
            custom_id: format!("item-{index}"),
            body: Bytes::from_static(b"{}"),
        };
        let facts = PlanFacts {
            provider_id: Some("local".to_owned()),
            model: Some(model.to_owned()),
            price_per_second: price,
        };
        let mut result = ItemResult::start(&item, &facts, "request", 1);
        result.ok = true;
        result.gpu_seconds = seconds;
        result.cold_starts = cold_starts;
        result.prompt_tokens = prompt;
        result.completion_tokens = completion;
        result
    }

    // --- Parsing ---------------------------------------------------------------------------

    #[test]
    fn parse_input_keeps_the_items_and_tolerates_a_windows_file() {
        let items = parse_input(&input_body(&[("a", "m1"), ("b", "m2")]))
            .ok()
            .expect("two well-formed lines are accepted");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].custom_id, "a");
        assert_eq!(items[1].custom_id, "b");
        // The body is re-serialised from the `body` object: the envelope's `method`/`url` fields are
        // not forwarded upstream, and a client cannot smuggle a different one past the lane.
        assert_eq!(model_of(&items[0].body), "m1");

        // CRLF and blank lines are tolerated: a file written on Windows is still a file.
        let windows = format!(
            "{}\r\n\r\n{}\r\n",
            input_line("a", "m1"),
            input_line("b", "m2")
        );
        assert_eq!(
            parse_input(windows.as_bytes())
                .ok()
                .expect("crlf is accepted")
                .len(),
            2
        );
    }

    #[test]
    fn parse_input_names_the_line_it_refuses() {
        let missing_custom_id = refusal_of_input(br#"{"body":{"model":"m"}}"#);
        assert_eq!(missing_custom_id.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            missing_custom_id.message,
            "line 1 has no string \"custom_id\""
        );

        let not_json = refusal_of_input(b"not json\n");
        assert!(
            not_json.message.starts_with("line 1 is not valid JSON"),
            "{}",
            not_json.message
        );

        let missing_body = refusal_of_input(br#"{"custom_id":"a"}"#);
        assert_eq!(missing_body.message, "line 1 has no object \"body\"");

        let missing_model = refusal_of_input(br#"{"custom_id":"a","body":{}}"#);
        assert_eq!(missing_model.message, "line 1 has no string \"body.model\"");

        // Streaming is refused here rather than half-served later: a batch result is one line, and a
        // streamed answer has no single line to be.
        let streaming =
            refusal_of_input(br#"{"custom_id":"a","body":{"model":"m","stream":true}}"#);
        assert!(
            streaming
                .message
                .contains("must be a non-streaming request"),
            "{}",
            streaming.message
        );
    }

    #[test]
    fn parse_input_refuses_a_repeated_custom_id_and_says_which_line_used_it_first() {
        let repeated = refusal_of_input(
            format!("{}\n{}\n", input_line("dup", "m"), input_line("dup", "m")).as_bytes(),
        );
        assert_eq!(repeated.status, StatusCode::BAD_REQUEST);
        // Results are joined on custom_id, so a duplicate is not a formatting nit: it makes the output
        // ambiguous, and the message has to say which line to fix.
        assert_eq!(
            repeated.message,
            "line 2 repeats custom_id \"dup\", which line 1 already used; results are joined on custom_id and it must be unique"
        );
    }

    #[test]
    fn parse_input_refuses_an_empty_file() {
        let empty = refusal_of_input(b"\n\n  \n");
        assert_eq!(empty.message, "the input file holds no items");
    }

    // --- The fixed fee -------------------------------------------------------------

    #[test]
    fn boot_profiles_are_the_two_measured_cards_and_nothing_else() {
        let rtx = boot_profile(3.03 / 3600.0).expect("the RTX PRO 6000 measurement");
        assert_eq!(rtx.card, "RTX PRO 6000");
        assert_eq!(rtx.boot_seconds, 180.0);
        // 180 s at $3.03/h, the operator's own measurement. This is a fixed fee, not a rate.
        assert!((rtx.boot_seconds * rtx.hourly_usd / 3600.0 - 0.1515).abs() < 1e-12);

        let l40s = boot_profile(1.95 / 3600.0).expect("the L40S measurement");
        assert_eq!(l40s.card, "L40S");
        assert_eq!(l40s.boot_seconds, 110.0);
        assert!((l40s.boot_seconds * l40s.hourly_usd / 3600.0 - 0.0596).abs() < 1e-4);

        // A price this build has no measurement for is not attributed to a card, and no boot fee is
        // invented for it.
        assert!(boot_profile(2.5 / 3600.0).is_none());
        assert!(boot_profile(0.0).is_none());
    }

    #[test]
    fn cost_and_boot_fee_are_reported_apart() {
        let price = 3.03 / 3600.0;
        let usage = summarize(&[
            settled(0, "m", 200.0, 1, Some(price), Some(10), Some(2)),
            settled(1, "m", 100.0, 0, Some(price), Some(30), Some(4)),
        ])
        .expect("two settled items have a report");
        assert_eq!(usage.prompt_tokens, 40);
        assert_eq!(usage.completion_tokens, 6);
        assert_eq!(usage.total_tokens, 46);
        assert!((usage.gpu_seconds - 300.0).abs() < 1e-9);
        // 300 s of card time.
        let cost = usage.cost_usd.expect("the endpoint declares a price");
        assert!((cost - 300.0 * price).abs() < 1e-9);
        // One cold start at 180 s of boot, on its own line of the report.
        let boot = usage
            .boot_cost_usd
            .expect("the price matches a measured card");
        // 180 s of boot at the same rate: a fixed fee, reported on its own line and not folded into the
        // per-second cost.
        assert!((boot - 180.0 * price).abs() < 1e-9);
        assert!((boot - 0.1515).abs() < 1e-12);
        assert!((cost - 200.0 * price - 100.0 * price).abs() < 1e-9);
        assert_eq!(usage.card.as_deref(), Some("RTX PRO 6000"));
        assert_eq!(usage.provider.as_deref(), Some("local"));
        assert_eq!(usage.model.as_deref(), Some("m"));
        assert_eq!(usage.cold_starts, 1);
    }

    #[test]
    fn the_cost_per_million_output_tokens_is_derived_and_never_guessed() {
        let price = 3.03 / 3600.0;
        let usage = summarize(&[
            settled(0, "m", 200.0, 1, Some(price), Some(10), Some(2)),
            settled(1, "m", 100.0, 0, Some(price), Some(30), Some(4)),
        ])
        .expect("two settled items have a report")
        .view();
        let cost = 300.0 * price;
        let boot = 180.0 * price;
        // 6 output tokens over 300 s of card time: both figures are the measured bill divided by the
        // measured token count, so they must agree with that division exactly, not approximately.
        let all_in = usage["usd_per_million_output_tokens"]
            .as_f64()
            .expect("a priced batch with output tokens has a figure");
        assert!((all_in - cost / 6.0 * 1_000_000.0).abs() < 1e-6);
        let serving = usage["serving_usd_per_million_output_tokens"]
            .as_f64()
            .expect("a priced batch with a measured boot fee has a figure");
        assert!((serving - (cost - boot) / 6.0 * 1_000_000.0).abs() < 1e-6);
        assert!(
            all_in > serving,
            "the boot fee is part of the all-in figure and not of the serving one"
        );

        // No output tokens: there is no denominator, and a division by zero would be infinity rather
        // than an answer. Both figures are absent, not zero.
        let silent = summarize(&[settled(0, "m", 200.0, 1, Some(price), Some(10), Some(0))])
            .expect("one settled item has a report")
            .view();
        assert!(silent["usd_per_million_output_tokens"].is_null());
        assert!(silent["serving_usd_per_million_output_tokens"].is_null());

        // No declared price: nothing to divide, so nothing is stated.
        let unpriced = summarize(&[settled(0, "m", 200.0, 1, None, Some(10), Some(2))])
            .expect("one settled item has a report")
            .view();
        assert!(unpriced["usd_per_million_output_tokens"].is_null());
        assert!(unpriced["serving_usd_per_million_output_tokens"].is_null());

        // A genuinely free endpoint costs zero per million, which is a known zero rather than unknown.
        // The serving figure stays unknown even here: a price of zero matches no measured card, so
        // the boot fee is not known to subtract, and the report says so instead of assuming the boot
        // was free too.
        let free = summarize(&[settled(0, "m", 200.0, 1, Some(0.0), Some(10), Some(2))])
            .expect("one settled item has a report")
            .view();
        assert_eq!(free["usd_per_million_output_tokens"], 0.0);
        assert!(free["boot_cost_usd"].is_null());
        assert!(free["serving_usd_per_million_output_tokens"].is_null());
    }

    #[test]
    fn the_report_says_whether_the_boot_or_the_work_paid_for_the_batch() {
        let price = 3.03 / 3600.0;

        // 200 s of card time with a 180 s boot inside it: 90 % of the bill was starting the container.
        let small = summarize(&[settled(0, "m", 200.0, 1, Some(price), Some(10), Some(2))])
            .expect("one settled item has a report")
            .view();
        assert_eq!(small["cold_start_verdict"], "boot_dominates");
        assert_eq!(small["boot_share_pct"], 90.0);
        assert_eq!(small["boot_cost_usd"], 0.1515);

        // An hour of card time with the same single boot: the boot is under a tenth of it.
        let large = summarize(&[settled(0, "m", 3600.0, 1, Some(price), Some(10), Some(2))])
            .expect("one settled item has a report")
            .view();
        assert_eq!(large["cold_start_verdict"], "work_dominates");
        assert_eq!(large["boot_share_pct"], 5.0);

        // Zero cost is not a verdict of "all boot": there is no denominator, so the report says
        // nothing rather than something confident and wrong.
        let free = summarize(&[settled(0, "m", 60.0, 1, Some(0.0), Some(4), Some(1))])
            .expect("one settled item has a report")
            .view();
        assert_eq!(free["cold_start_verdict"], "unknown");
        assert!(free["boot_share_pct"].is_null());

        // An unpriced batch has no bill to divide either, and a zero boot fee on a priced batch is a
        // real zero share rather than an unknown one.
        let unpriced = summarize(&[settled(0, "m", 60.0, 1, None, Some(4), Some(1))])
            .expect("one settled item has a report")
            .view();
        assert_eq!(unpriced["cold_start_verdict"], "unknown");
        let warm = summarize(&[settled(0, "m", 60.0, 0, Some(price), Some(4), Some(1))])
            .expect("one settled item has a report")
            .view();
        assert_eq!(warm["cold_start_verdict"], "work_dominates");
        assert_eq!(warm["boot_share_pct"], 0.0);
    }

    #[test]
    fn an_unknown_or_mixed_price_makes_the_report_honest_rather_than_wrong() {
        let unknown = 2.5 / 3600.0;
        let usage = summarize(&[settled(0, "m", 60.0, 2, Some(unknown), Some(5), Some(1))])
            .expect("one settled item has a report");
        // The running cost is known; the boot fee is not, and is reported as unknown instead of zero.
        assert!(usage.cost_usd.is_some());
        assert_eq!(usage.boot_cost_usd, None);
        assert_eq!(usage.card, None);

        let unpriced = summarize(&[settled(0, "m", 60.0, 1, None, Some(5), Some(1))])
            .expect("one settled item has a report");
        assert_eq!(unpriced.cost_usd, None);
        assert_eq!(unpriced.boot_cost_usd, None);

        // A priced batch with no cold start paid no boot fee, and says so with a zero rather than an
        // absent field.
        let warm = summarize(&[settled(0, "m", 60.0, 0, Some(unknown), Some(5), Some(1))])
            .expect("one settled item has a report");
        assert_eq!(warm.boot_cost_usd, Some(0.0));

        // Two providers in one batch is not one provider: the shared field goes null.
        let mut other = settled(1, "m", 1.0, 0, Some(unknown), None, None);
        other.provider_id = Some("elsewhere".to_owned());
        let mixed = summarize(&[settled(0, "m", 1.0, 0, Some(unknown), None, None), other])
            .expect("two settled items have a report");
        assert_eq!(mixed.provider, None);
    }

    #[test]
    fn retry_is_for_transience_and_never_for_a_verdict() {
        let error = |code: &str| ItemError {
            code: code.to_owned(),
            message: "x".to_owned(),
        };
        let status = |code: u16| ItemReply {
            status: StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            headers: HeaderMap::new(),
            body: Bytes::new(),
            cold_starts: 0,
        };

        assert!(retryable(&status(500), &error("upstream_error_500")));
        assert!(retryable(&status(503), &error("overloaded")));
        assert!(retryable(&status(408), &error("timeout")));
        // A 4xx is the provider's verdict on the request: sending it again would fail identically and
        // cost another attempt.
        assert!(!retryable(&status(400), &error("invalid_request")));
        assert!(!retryable(&status(422), &error("invalid_prompt")));
        assert!(!retryable(&status(401), &error("auth_failed")));
        assert!(!retryable(&status(404), &error("not_found")));
        // The two documented exceptions are both "the relay had nowhere to send it", not "the request
        // was wrong".
        assert!(retryable(&status(429), &error("no_usable_accounts")));
        assert!(!retryable(&status(429), &error("rate_limit_exceeded")));
        assert!(retryable(&status(503), &error("kv_budget_exceeded")));

        let mut headers = HeaderMap::new();
        headers.insert(
            "retry-after",
            "900".parse().expect("a header value is constructible"),
        );
        // A provider asking for a quarter of an hour must not stall a batch: the cap wins.
        assert_eq!(retry_delay(1, &headers), RETRY_CAP);
        headers.insert(
            "retry-after",
            "2".parse().expect("a header value is constructible"),
        );
        assert_eq!(retry_delay(1, &headers), Duration::from_secs(2));
        // With no hint the wait grows with the attempt, and stays under the cap.
        assert_eq!(retry_delay(1, &HeaderMap::new()), RETRY_BASE * 2);
        assert!(RETRY_BASE * 8 <= RETRY_CAP);
    }

    // --- The whole lane --------------------------------------------------------------------

    #[tokio::test]
    async fn a_batch_answers_in_submission_order_and_isolates_its_failures() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let store = Arc::new(Store::open_in_memory().expect("an in-memory ledger"));
        let price = 3.03 / 3600.0;

        // One scripted upstream covering every case the acceptance criteria name: a plain success, a
        // transient failure that recovers, a permanent 4xx, a slow success, a permanent 5xx, and a
        // success that reports a cold start.
        let answerer: Answerer =
            Arc::new(
                |model: &str, nth: usize, _request: &ItemRequest| match model {
                    "item-0" => Answer::Now(ok_reply(model, 10, 5)),
                    "item-1" if nth < 2 => {
                        Answer::Now(failure_reply(503, "overloaded", "the model is booting"))
                    }
                    "item-1" => Answer::Now(ok_reply(model, 20, 7)),
                    "item-2" => Answer::Now(failure_reply(
                        422,
                        "invalid_prompt",
                        "the prompt was rejected",
                    )),
                    "item-3" => Answer::Later(Duration::from_millis(40), ok_reply(model, 30, 3)),
                    "item-4" => {
                        Answer::Now(failure_reply(500, "upstream_error", "the model crashed"))
                    }
                    "item-5" => Answer::Now(with_cold_start(ok_reply(model, 40, 4), 1)),
                    other => Answer::Now(ok_reply(other, 0, 0)),
                },
            );
        let mut stub = Stub::new(following(50, UNLIMITED, Some(price)), answerer);
        stub.store = Some(Arc::clone(&store));
        let stub = Arc::new(stub);
        let lane = stub_lane(root.path(), Arc::new(AdmissionPool::new()), &stub, 3);

        let id = submit(
            &lane,
            &[
                ("item-0", "item-0"),
                ("item-1", "item-1"),
                ("item-2", "item-2"),
                ("item-3", "item-3"),
                ("item-4", "item-4"),
                ("item-5", "item-5"),
            ],
        )
        .await;
        let view = wait_terminal(&lane, &id).await;

        // Finished, with failures, is `completed`. That is the distinction the operator needs: a batch
        // that ran every item and could not answer two of them is not a crashed batch.
        assert_eq!(view["status"], "completed");
        assert!(view["completed_at"].is_number());
        assert!(view["failed_at"].is_null());
        assert_eq!(count_of(&view, "total"), 6);
        assert_eq!(count_of(&view, "completed"), 4);
        assert_eq!(count_of(&view, "failed"), 2);

        let output = view["output_file_id"]
            .as_str()
            .expect("a batch with settled items has an output file");
        let lines = result_lines(&lane, output).await;
        // Submission order, not completion order: item-3 answered last of the successes and is still
        // third, because `finish` walks the item indices rather than the order workers finished.
        assert_eq!(
            custom_ids(&lines),
            vec!["item-0", "item-1", "item-3", "item-5"]
        );
        assert_eq!(lines[0]["response"]["status_code"], 200);
        assert_eq!(
            lines[0]["response"]["body"]["choices"][0]["message"]["content"],
            "ok"
        );

        let errors = view["error_file_id"]
            .as_str()
            .expect("a batch with failed items has an error file");
        let failed = result_lines(&lane, errors).await;
        assert_eq!(custom_ids(&failed), vec!["item-2", "item-4"]);
        // The upstream's own words, not a generic "the item failed".
        assert_eq!(failed[0]["error"]["message"], "the prompt was rejected");
        assert_eq!(failed[0]["error"]["code"], "invalid_prompt");
        assert_eq!(failed[1]["error"]["message"], "the model crashed");

        // Retry is bounded and per item. item-1 recovered on its third attempt; item-4 never did and
        // stopped there; item-2 was judged bad and was not retried at all.
        let job_dir = root.path().join("jobs").join(&id);
        assert_eq!(item_record(&job_dir, 1).attempts, MAX_ATTEMPTS);
        assert_eq!(item_record(&job_dir, 2).attempts, 1);
        assert_eq!(item_record(&job_dir, 4).attempts, MAX_ATTEMPTS);
        let executed = 1 + MAX_ATTEMPTS as usize + 1 + 1 + MAX_ATTEMPTS as usize + 1;
        assert_eq!(stub.calls(), executed);
        // Nothing failed forever: the batch reached a terminal state with the pool drained.
        assert_eq!(lane.queued(), 0);

        // The cost report is the batch's own, in dollars, with the boot fee kept separate.
        let usage = &view["usage"];
        assert_eq!(usage["prompt_tokens"].as_u64(), Some(100));
        assert_eq!(usage["completion_tokens"].as_u64(), Some(19));
        assert_eq!(usage["total_tokens"].as_u64(), Some(119));
        assert_eq!(usage["cold_starts"].as_u64(), Some(1));
        assert_eq!(usage["provider"], "local");
        assert_eq!(usage["card"], "RTX PRO 6000");
        // The items ran six different models, so no single model can be claimed for the batch.
        assert!(usage["model"].is_null());
        assert!(usage["gpu_seconds"].as_f64().unwrap_or_default() > 0.0);
        let cost = usage["cost_usd"]
            .as_f64()
            .expect("a priced batch reports a cost");
        assert!(cost > 0.0);
        let boot = usage["boot_cost_usd"]
            .as_f64()
            .expect("the declared price matches a measured card");
        assert!((boot - 0.1515).abs() < 1e-6);

        // Four of the six items succeeded, and the shared GPU ledger has exactly four rows: a lease
        // records its row when it is marked successful, so an attempt that ended in a retry released its
        // tokens without a row. The batch's own `gpu_seconds` above counts every attempt, which is why
        // the two figures differ and why the batch report is the one to reconcile against.
        let totals = store
            .gpu_usage_totals(Millis::now().plus_secs(-60), Millis::now().plus_secs(60))
            .expect("the ledger is readable");
        assert_eq!(totals.requests, 4);
    }

    #[tokio::test]
    async fn an_item_that_cannot_fit_the_budget_fails_alone_and_nothing_is_sent() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let planner: Planner = Arc::new(|body: &[u8]| {
            let model = model_of(body);
            // 5000 tokens against a 100-token pool: waiting cannot help, because the item is bigger
            // than the whole pool.
            let limits = if model == "too-big-for-pool" {
                KvBudgetLimits {
                    pool_tokens: Some(100),
                    max_context: None,
                    max_concurrent: None,
                }
            } else {
                // 5000 tokens against a 4096-token context: also hopeless, and rejected before the
                // pool is even consulted.
                KvBudgetLimits {
                    pool_tokens: None,
                    max_context: Some(4096),
                    max_concurrent: None,
                }
            };
            Some(ItemPlan {
                provider_id: "local".to_owned(),
                model,
                tokens: 5000,
                limits,
                price_per_second: None,
            })
        });
        let stub = Arc::new(Stub::new(
            planner,
            Arc::new(|model: &str, _nth: usize, _request: &ItemRequest| {
                Answer::Now(ok_reply(model, 1, 1))
            }),
        ));
        let pool = Arc::new(AdmissionPool::new());
        let lane = stub_lane(root.path(), Arc::clone(&pool), &stub, 2);
        let id = submit(
            &lane,
            &[("a", "too-big-for-pool"), ("b", "too-big-for-context")],
        )
        .await;
        let view = wait_terminal(&lane, &id).await;

        assert_eq!(view["status"], "completed");
        assert_eq!(count_of(&view, "completed"), 0);
        assert_eq!(count_of(&view, "failed"), 2);
        // Nothing was dispatched: a request that cannot be admitted is not worth paying for.
        assert_eq!(stub.calls(), 0);
        assert_eq!(pool.count("local"), 0);
        let errors = result_lines(
            &lane,
            view["error_file_id"].as_str().expect("both items failed"),
        )
        .await;
        assert_eq!(custom_ids(&errors), vec!["a", "b"]);
        assert_eq!(errors[0]["error"]["code"], "kv_budget_exceeded");
        assert!(
            errors[0]["error"]["message"]
                .as_str()
                .expect("a message")
                .contains("KV-token budget"),
            "{}",
            errors[0]["error"]["message"]
        );
        assert!(
            errors[1]["error"]["message"]
                .as_str()
                .expect("a message")
                .contains("larger than the model context"),
            "{}",
            errors[1]["error"]["message"]
        );
    }

    #[tokio::test]
    async fn the_pool_admits_tokens_and_a_released_lease_returns_them() {
        let root = tempfile::tempdir().expect("a temporary directory");
        // Both items are worth 80 tokens and the endpoint holds 100: exactly one may be in flight.
        let limits = KvBudgetLimits {
            pool_tokens: Some(100),
            max_context: None,
            max_concurrent: None,
        };
        let stub = Arc::new(Stub::new(
            following(80, limits, None),
            Arc::new(|model: &str, _nth: usize, _request: &ItemRequest| {
                Answer::Parked(ok_reply(model, 1, 1))
            }),
        ));
        let pool = Arc::new(AdmissionPool::new());
        let lane = stub_lane(root.path(), Arc::clone(&pool), &stub, 2);
        let id = submit(&lane, &[("a", "m"), ("b", "m")]).await;

        wait_until("the first item to be admitted", || pool.count("local") == 1).await;
        wait_until("the first attempt to be in flight", || {
            stub.parked.load(Ordering::SeqCst) == 1
        })
        .await;
        assert_eq!(
            stub.calls(),
            1,
            "the second item must be queued, not admitted"
        );
        assert_eq!(stub.peak.load(Ordering::SeqCst), 1);

        // Releasing the first item hands its 80 tokens to the second, and only to the second.
        stub.gate.add_permits(1);
        wait_until("the second item to be admitted", || stub.calls() == 2).await;
        assert_eq!(pool.count("local"), 1);
        assert_eq!(
            stub.peak.load(Ordering::SeqCst),
            1,
            "two items must never be in flight at once"
        );

        stub.gate.add_permits(1);
        let view = wait_terminal(&lane, &id).await;
        assert_eq!(count_of(&view, "completed"), 2);
        // Both leases came back. A pool that never releases would deadlock the next item, so this is
        // the assertion the whole feature rests on.
        assert_eq!(pool.count("local"), 0);
        let probe = pool
            .reserve("local".to_owned(), "probe".to_owned(), 80, limits, None)
            .expect("the tokens were returned to the pool");
        drop(probe);

        // Every attempt was admitted for the provider it was planned against, which is what tells the
        // request path that this reservation is already held.
        assert_eq!(
            lock(&stub.admitted).clone(),
            vec![Some("local".to_owned()), Some("local".to_owned())]
        );
    }

    // --- Recovery --------------------------------------------------------------------------

    #[tokio::test]
    async fn a_batch_resumes_from_disk_and_never_redoes_a_settled_item() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let first = Arc::new(Stub::new(
            following(10, UNLIMITED, None),
            Arc::new(|model: &str, _nth: usize, _request: &ItemRequest| {
                Answer::Parked(ok_reply(model, 1, 1))
            }),
        ));
        let dying = stub_lane(root.path(), Arc::new(AdmissionPool::new()), &first, 1);
        let names: Vec<(String, String)> = (0..10)
            .map(|index| (format!("item-{index}"), format!("item-{index}")))
            .collect();
        let items: Vec<(&str, &str)> = names
            .iter()
            .map(|(custom_id, model)| (custom_id.as_str(), model.as_str()))
            .collect();
        let id = submit(&dying, &items).await;

        // Eight items finish, then the ninth parks in flight. That is the crash window: eight item files
        // are on disk, the ninth has no record and no answer.
        first.gate.add_permits(8);
        wait_until("eight items to settle", || {
            dying
                .batch_view(&id)
                .map(|view| count_of(&view, "completed") == 8)
                .unwrap_or(false)
        })
        .await;
        wait_until("the ninth attempt to be in flight", || first.calls() == 9).await;
        let before = dying.batch_view(&id).expect("the batch is known");
        assert_eq!(before["status"], "in_progress");
        // Nothing is flushed on the way out: the item files are already the truth, which is why a
        // process that dies here loses at most the one item that was in flight.
        dying.shutdown();

        // A fresh process: new admission pool, new executor, same directory.
        let second = Arc::new(Stub::new(
            following(10, UNLIMITED, None),
            Arc::new(|model: &str, _nth: usize, _request: &ItemRequest| {
                Answer::Now(ok_reply(model, 1, 1))
            }),
        ));
        let resumed = stub_lane(root.path(), Arc::new(AdmissionPool::new()), &second, 1);
        // Only the two unsettled items were queued, and they were queued before any worker could run.
        assert_eq!(resumed.queued(), 2);
        let view = wait_terminal(&resumed, &id).await;

        // Exactly the two unsettled items ran again. The eight that had settled were not re-executed,
        // which is the whole point of persisting each item as it finishes.
        assert_eq!(second.calls(), 2);
        assert_eq!(
            lock(&second.models).clone(),
            vec!["item-8".to_owned(), "item-9".to_owned()]
        );
        assert_eq!(view["status"], "completed");
        assert_eq!(count_of(&view, "completed"), 10);
        assert_eq!(count_of(&view, "failed"), 0);
        let output = result_lines(
            &resumed,
            view["output_file_id"]
                .as_str()
                .expect("a completed batch has an output file"),
        )
        .await;
        // The eight results written by the earlier process are still there, joined in order with the two
        // the resume produced.
        assert_eq!(
            custom_ids(&output),
            (0..10)
                .map(|index| format!("item-{index}"))
                .collect::<Vec<_>>()
        );
        assert_eq!(resumed.queued(), 0);
    }

    #[tokio::test]
    async fn a_batch_whose_input_vanished_is_failed_not_completed() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let id = "batch_orphan";
        // A batch record whose input file no longer exists. Written by hand because the only way to
        // reach this state is a crash plus an operator deleting a file.
        let meta = JobMeta {
            max_cost_usd: None,
            budget_exceeded: false,
            stopped_reason: None,
            id: id.to_owned(),
            input_file_id: "file_gone".to_owned(),
            endpoint: CHAT_COMPLETIONS_PATH.to_owned(),
            completion_window: "24h".to_owned(),
            created_at: now_seconds(),
            in_progress_at: Some(now_seconds()),
            expires_at: None,
            finalizing_at: None,
            completed_at: None,
            failed_at: None,
            cancelling_at: None,
            cancelled_at: None,
            status: Status::InProgress,
            total: 3,
            completed: 1,
            failed: 0,
            metadata: None,
            output_file_id: None,
            error_file_id: None,
            errors: None,
            usage: None,
        };
        write_atomic(
            &root.path().join("jobs").join(id).join("job.json"),
            &json_bytes(&meta),
        )
        .expect("the batch record is written");

        let stub = Arc::new(Stub::new(
            following(10, UNLIMITED, None),
            Arc::new(|model: &str, _nth: usize, _request: &ItemRequest| {
                Answer::Now(ok_reply(model, 1, 1))
            }),
        ));
        let lane = stub_lane(root.path(), Arc::new(AdmissionPool::new()), &stub, 2);
        let view = lane.batch_view(id).expect("the batch was recovered");

        // Failed, with a reason. A batch that cannot run is never reported as completed with nothing in
        // it: those two states mean different things to the operator.
        assert_eq!(view["status"], "failed");
        assert!(view["failed_at"].is_number());
        assert!(view["completed_at"].is_null());
        assert_eq!(view["errors"][0]["code"], "input_file_missing");
        assert!(view["output_file_id"].is_null());
        assert_eq!(stub.calls(), 0);
        assert_eq!(lane.queued(), 0);

        // The failure was written back, so the next restart does not try again.
        let reopened = stub_lane(root.path(), Arc::new(AdmissionPool::new()), &stub, 2);
        assert_eq!(reopened.batch_view(id).expect("known")["status"], "failed");
        assert_eq!(reopened.queued(), 0);
    }

    // --- Cancelling ------------------------------------------------------------------------

    #[tokio::test]
    async fn cancelling_stops_new_items_and_keeps_what_was_paid_for() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let stub = Arc::new(Stub::new(
            following(10, UNLIMITED, None),
            Arc::new(|model: &str, _nth: usize, _request: &ItemRequest| {
                Answer::Parked(ok_reply(model, 1, 1))
            }),
        ));
        let lane = stub_lane(root.path(), Arc::new(AdmissionPool::new()), &stub, 1);
        let id = submit(
            &lane,
            &[
                ("item-0", "item-0"),
                ("item-1", "item-1"),
                ("item-2", "item-2"),
                ("item-3", "item-3"),
                ("item-4", "item-4"),
            ],
        )
        .await;

        stub.gate.add_permits(2);
        wait_until("two items to settle", || {
            lane.batch_view(&id)
                .map(|view| count_of(&view, "completed") == 2)
                .unwrap_or(false)
        })
        .await;
        wait_until("the third attempt to be in flight", || stub.calls() == 3).await;

        let cancelling = match lane.cancel(&id).await {
            Some(Ok(view)) => view,
            Some(Err(refusal)) => panic!("the cancel was refused: {}", refusal.message),
            None => panic!("the batch is not known"),
        };
        // Cancelling is reported at once, and the item already on the wire is not thrown away.
        assert_eq!(cancelling["status"], "in_progress");
        assert!(cancelling["cancelling_at"].is_number());
        assert!(cancelling["cancelled_at"].is_null());

        // Let the in-flight item land. It is the last one that will ever be dispatched.
        stub.gate.add_permits(1);
        let view = wait_terminal(&lane, &id).await;
        assert_eq!(view["status"], "cancelled");
        assert!(view["cancelled_at"].is_number());
        assert_eq!(count_of(&view, "total"), 5);
        assert_eq!(count_of(&view, "completed"), 3);
        assert_eq!(count_of(&view, "failed"), 0);
        // Items 3 and 4 were never dispatched and are not errors: the operator asked for them to stop.
        assert_eq!(stub.calls(), 3);
        assert_eq!(lane.queued(), 0);
        let output = result_lines(
            &lane,
            view["output_file_id"]
                .as_str()
                .expect("the three finished items are kept"),
        )
        .await;
        assert_eq!(custom_ids(&output), vec!["item-0", "item-1", "item-2"]);
        assert!(view["error_file_id"].is_null());

        let again = match lane.cancel(&id).await {
            Some(Err(refusal)) => refusal,
            Some(Ok(_)) => panic!("a cancelled batch cannot be cancelled twice"),
            None => panic!("the batch is not known"),
        };
        assert_eq!(again.status, StatusCode::BAD_REQUEST);
        assert!(
            again.message.contains("already cancelled"),
            "{}",
            again.message
        );

        // The persisted record is terminal too, so a restart will not pick the batch up again.
        let raw = std::fs::read(root.path().join("jobs").join(&id).join("job.json"))
            .expect("the record is on disk");
        let persisted: JobMeta = serde_json::from_slice(&raw).expect("the record is readable");
        assert!(persisted.status.is_terminal());
        assert_eq!(persisted.status.as_str(), "cancelled");
    }

    // --- The file surface and submission validation -----------------------------------------

    #[tokio::test]
    async fn files_are_stored_viewed_listed_and_hidden_and_submission_is_validated() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let stub = Arc::new(Stub::new(
            following(10, UNLIMITED, None),
            Arc::new(|model: &str, _nth: usize, _request: &ItemRequest| {
                Answer::Now(ok_reply(model, 1, 1))
            }),
        ));
        // No workers yet: submission has to be a queue, not a dispatch.
        let lane = stub_lane(root.path(), Arc::new(AdmissionPool::new()), &stub, 0);
        let body = input_body(&[("a", "m")]);

        let file = lane
            .put_file(body.clone(), Some("input.jsonl".to_owned()), None)
            .await
            .ok()
            .expect("the upload is accepted");
        let file_id = file["id"]
            .as_str()
            .expect("a stored file has an id")
            .to_owned();
        assert_eq!(file["object"], "file");
        assert_eq!(file["purpose"], "batch");
        assert_eq!(file["filename"], "input.jsonl");
        assert_eq!(file["bytes"].as_u64(), Some(body.len() as u64));
        assert!(file["created_at"].is_number());
        // The live view carries no `deleted` flag.
        assert!(file.get("deleted").is_none());

        // The bytes come back byte for byte.
        assert_eq!(
            lane.file_bytes(&file_id)
                .await
                .expect("the bytes are stored"),
            body
        );
        assert_eq!(
            lane.file_view(&file_id).expect("the record is stored")["id"],
            file_id.as_str()
        );

        // The lane itself stores any purpose, because the upload surface completes files through
        // the same call. The purpose it will serve is enforced at the HTTP layer, and a fine-tune
        // file is one of the two it must accept: a fine-tuning job is created from one.
        let staged = match lane
            .put_file(body.clone(), None, Some("fine-tune".to_owned()))
            .await
        {
            Ok(stored) => stored,
            Err(refusal) => panic!("a fine-tune file was refused: {}", refusal.message),
        };
        assert_eq!(staged["purpose"], "fine-tune");
        match require_supported_purpose(Some("fine-tune")) {
            Ok(()) => {}
            Err(refusal) => panic!("fine-tune was not a served purpose: {}", refusal.message),
        }

        // A purpose nothing here can read is refused, and the refusal names what was asked for and
        // what is served, so a client is not left guessing which spelling would work.
        let refused = match require_supported_purpose(Some("assistants")) {
            Ok(()) => panic!("a purpose no family here serves was accepted"),
            Err(refusal) => refusal,
        };
        assert_eq!(refused.status, StatusCode::BAD_REQUEST);
        assert!(
            refused.message.contains("assistants")
                && refused.message.contains("\"batch\"")
                && refused.message.contains("\"fine-tune\""),
            "{}",
            refused.message
        );

        // A second upload, so that a one-item page has more behind it.
        let second = lane
            .put_file(
                input_body(&[("b", "m")]),
                Some("b.jsonl".to_owned()),
                Some("batch".to_owned()),
            )
            .await
            .ok()
            .expect("the upload is accepted");
        let second_id = second["id"].as_str().expect("an id").to_owned();
        let page = lane.files_view(1);
        assert_eq!(page["object"], "list");
        assert_eq!(page["data"].as_array().map(Vec::len), Some(1));
        assert_eq!(page["has_more"], true);
        assert_eq!(page["first_id"], page["data"][0]["id"]);
        assert_eq!(page["last_id"], page["data"][0]["id"]);

        // Deleting hides the file from the API but keeps its bytes: a resumed batch re-reads its input,
        // and a finished batch's output has to stay downloadable.
        let deleted = match lane.delete_file(&file_id).await {
            Some(Ok(view)) => view,
            Some(Err(refusal)) => panic!("the delete was refused: {}", refusal.message),
            None => panic!("the file is not known"),
        };
        assert_eq!(deleted["object"], "file");
        assert_eq!(deleted["deleted"], true);
        assert!(lane.file(&file_id).is_none());
        assert!(lane.file_body_path(&file_id).exists());
        assert!(lane.delete_file(&file_id).await.is_none());
        // Reads of a deleted file behave as if it never existed.
        assert!(lane.file_bytes(&file_id).await.is_none());

        let spec = |input_file_id: &str, endpoint: &str, window: &str| BatchSpec {
            max_cost_usd: None,
            input_file_id: input_file_id.to_owned(),
            endpoint: endpoint.to_owned(),
            completion_window: window.to_owned(),
            metadata: None,
        };

        let unsupported =
            refusal_of_submit(lane.submit(spec(&second_id, "/v1/embeddings", "24h")).await);
        assert_eq!(unsupported.status, StatusCode::BAD_REQUEST);
        assert_eq!(unsupported.code, "unsupported_endpoint");

        let window = refusal_of_submit(
            lane.submit(spec(&second_id, CHAT_COMPLETIONS_PATH, "1h"))
                .await,
        );
        assert_eq!(window.status, StatusCode::BAD_REQUEST);
        assert!(window.message.contains("24h"), "{}", window.message);

        let unknown = refusal_of_submit(
            lane.submit(spec("file_nope", CHAT_COMPLETIONS_PATH, "24h"))
                .await,
        );
        assert_eq!(unknown.status, StatusCode::NOT_FOUND);
        assert!(unknown.message.contains("file_nope"), "{}", unknown.message);

        // A malformed line is refused in the POST that submitted it, not by a batch that fails later.
        let duplicate = lane
            .put_file(input_body(&[("dup", "m"), ("dup", "m")]), None, None)
            .await
            .ok()
            .expect("the upload is accepted");
        let duplicate_id = duplicate["id"].as_str().expect("an id").to_owned();
        let repeated = refusal_of_submit(
            lane.submit(spec(&duplicate_id, CHAT_COMPLETIONS_PATH, "24h"))
                .await,
        );
        assert_eq!(repeated.status, StatusCode::BAD_REQUEST);
        assert!(
            repeated.message.contains("line 2 repeats custom_id"),
            "{}",
            repeated.message
        );

        // A valid submission is accepted, stored and queued — and with no workers running, nothing is
        // dispatched. That is the difference between a queue and a pass-through.
        let view = lane
            .submit(spec(&second_id, CHAT_COMPLETIONS_PATH, "24h"))
            .await
            .ok()
            .expect("the batch is accepted");
        let id = view["id"].as_str().expect("an id").to_owned();
        assert_eq!(view["status"], "validating");
        assert_eq!(view["input_file_id"], second_id.as_str());
        assert_eq!(view["endpoint"], CHAT_COMPLETIONS_PATH);
        assert_eq!(view["completion_window"], "24h");
        assert!(view["in_progress_at"].is_null());
        assert_eq!(view["request_counts"]["total"].as_u64(), Some(1));
        assert_eq!(lane.queued(), 1);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            stub.calls(),
            0,
            "a submitted batch must not be dispatched inline"
        );

        // Starting the pool is all it takes to drain the queue.
        lane.start_workers(1);
        let view = wait_terminal(&lane, &id).await;
        assert_eq!(view["status"], "completed");
        assert_eq!(count_of(&view, "completed"), 1);
        assert_eq!(stub.calls(), 1);
        // The batch list shows it as finished.
        let list = lane.batches_view(10, None);
        assert_eq!(list["object"], "list");
        assert_eq!(list["data"][0]["id"], id.as_str());
    }

    // --- Uploading a file --------------------------------------------------------------------

    /// A `multipart/form-data` body with the given parts, as a client library would build it.
    fn upload_form(parts: &[(&str, Option<&str>, &str)]) -> Request {
        const BOUNDARY: &str = "----manarouter-test-boundary";
        let mut body = String::new();
        for (name, filename, value) in parts {
            body.push_str(&format!("--{BOUNDARY}\r\n"));
            match filename {
                Some(filename) => body.push_str(&format!(
                    "Content-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\n\
                     Content-Type: application/octet-stream\r\n\r\n"
                )),
                None => body.push_str(&format!(
                    "Content-Disposition: form-data; name=\"{name}\"\r\n\r\n"
                )),
            }
            body.push_str(value);
            body.push_str("\r\n");
        }
        body.push_str(&format!("--{BOUNDARY}--\r\n"));
        Request::builder()
            .header(
                axum::http::header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .body(AxumBody::from(body))
            .expect("a request is built")
    }

    #[tokio::test]
    async fn an_upload_form_is_read_from_its_file_and_purpose_parts() {
        let request = upload_form(&[
            ("purpose", None, "batch"),
            ("file", Some("in.jsonl"), "{\"a\":1}\n"),
            // A field this lane does not use is read and dropped, not refused: a client library that
            // adds one is still sending a valid upload.
            ("unused", None, "whatever"),
        ]);
        let form = Multipart::from_request(request, &())
            .await
            .expect("the body is a valid form");
        let (bytes, filename, purpose) = read_upload_form(form).await.ok().expect("the form reads");
        assert_eq!(bytes, b"{\"a\":1}\n");
        assert_eq!(filename.as_deref(), Some("in.jsonl"));
        assert_eq!(purpose.as_deref(), Some("batch"));
    }

    #[tokio::test]
    async fn an_upload_form_without_a_file_is_refused() {
        let request = upload_form(&[("purpose", None, "batch")]);
        let form = Multipart::from_request(request, &())
            .await
            .expect("the body is a valid form");
        let error = read_upload_form(form)
            .await
            .expect_err("a form with no file is refused");
        assert_eq!(error.code, "invalid_request_error");
        assert!(error.message.contains("no \"file\" part"));
    }

    // --- The spending cap ------------------------------------------------------------------

    /// Submit a batch with a cap, for the tests that trip one.
    async fn submit_capped(lane: &Arc<BatchLane>, items: &[(&str, &str)], cap: f64) -> String {
        let file = lane
            .put_file(input_body(items), Some("input.jsonl".to_owned()), None)
            .await
            .ok()
            .expect("the input file is accepted");
        let input_file_id = file["id"]
            .as_str()
            .expect("a stored file has an id")
            .to_owned();
        let view = lane
            .submit(BatchSpec {
                input_file_id,
                endpoint: CHAT_COMPLETIONS_PATH.to_owned(),
                completion_window: "24h".to_owned(),
                metadata: None,
                max_cost_usd: Some(cap),
            })
            .await
            .ok()
            .expect("the batch is accepted");
        // The cap is echoed from the first response, not only once it has been spent.
        assert_eq!(view["max_cost_usd"], json!(cap));
        assert_eq!(view["budget_exceeded"], json!(false));
        view["id"].as_str().expect("a batch has an id").to_owned()
    }

    #[test]
    fn a_cost_cap_is_read_as_a_positive_number_of_dollars() {
        // Absent and null both mean "no cap": a client that omits the field is not refused, and one
        // that sends an explicit null is sending the same request.
        assert_eq!(parse_max_cost_usd(None).ok(), Some(None));
        assert_eq!(parse_max_cost_usd(Some(&Value::Null)).ok(), Some(None));
        assert_eq!(parse_max_cost_usd(Some(&json!(0.5))).ok(), Some(Some(0.5)));
        assert_eq!(
            parse_max_cost_usd(Some(&json!(25.0))).ok(),
            Some(Some(25.0))
        );
        // The ceiling itself is allowed; above it is not.
        assert_eq!(
            parse_max_cost_usd(Some(&json!(MAX_COST_CEILING))).ok(),
            Some(Some(MAX_COST_CEILING))
        );

        for refused in [
            json!(0),
            json!(-1),
            json!(MAX_COST_CEILING + 0.01),
            json!("5"),
            json!(true),
            json!([1]),
        ] {
            let error = parse_max_cost_usd(Some(&refused))
                .err()
                .unwrap_or_else(|| panic!("{refused} must not be accepted as a cap"));
            assert_eq!(error.code, "invalid_request_error");
        }
        // A cap of zero is refused rather than obeyed: it would stop before the first item while
        // looking like a request that runs a batch.
        assert!(parse_max_cost_usd(Some(&json!(0))).is_err());
    }

    #[test]
    fn the_worker_count_can_be_set_but_not_to_zero_or_a_thousand() {
        // The default is what an operator who configures nothing gets.
        assert_eq!(parse_workers(""), DEFAULT_WORKERS);
        assert_eq!(parse_workers("nonsense"), DEFAULT_WORKERS);
        assert_eq!(parse_workers("3"), 3);
        assert_eq!(parse_workers("  3  "), 3);
        // The two values that would be a mistake rather than a preference.
        assert_eq!(parse_workers("0"), DEFAULT_WORKERS);
        assert_eq!(parse_workers("1000"), 64);
        assert_eq!(parse_workers("64"), 64);
    }

    #[test]
    fn an_item_costs_its_gpu_seconds_plus_the_boot_fee() {
        let price = 3.03 / 3600.0;
        let facts = PlanFacts {
            provider_id: Some("local".to_owned()),
            model: Some("m".to_owned()),
            price_per_second: Some(price),
        };
        let item = BatchItem {
            custom_id: "c".to_owned(),
            body: Bytes::new(),
        };
        let mut result = ItemResult::start(&item, &facts, "req", 1);
        result.gpu_seconds = 3600.0;
        result.cold_starts = 1;
        // One card-hour at the RTX PRO 6000 rate, plus one 180-second boot at the same rate.
        let cost = item_cost(&result);
        assert!(
            (cost - (3.03 + 180.0 * price)).abs() < 1e-9,
            "cost was {cost}"
        );

        // Cold starts on a card this build has no measurement for contribute their GPU seconds and no
        // boot fee, which is a floor rather than a guess.
        result.price_per_second = Some(5.0);
        result.cold_starts = 3;
        let cost = item_cost(&result);
        assert!((cost - 18000.0).abs() < 1e-6, "cost was {cost}");

        // An endpoint that declares no price has no cost this module can state, so it adds nothing to
        // the ledger rather than a made-up figure.
        result.price_per_second = None;
        assert_eq!(item_cost(&result), 0.0);
    }

    #[tokio::test]
    async fn a_batch_that_reaches_its_cap_stops_and_says_so() {
        let root = tempfile::tempdir().expect("a temporary directory");
        // A dollar a second, so one item that takes tens of milliseconds costs whole dollars and the
        // test does not depend on the resolution of the wall clock.
        let price = 1.0;
        let answerer: Answerer = Arc::new(|model: &str, _nth: usize, _request: &ItemRequest| {
            Answer::Later(Duration::from_millis(30), ok_reply(model, 1, 1))
        });
        let stub = Arc::new(Stub::new(following(10, UNLIMITED, Some(price)), answerer));
        // One worker, so the queue is drained in order and the first settled item is item-0.
        let lane = stub_lane(root.path(), Arc::new(AdmissionPool::new()), &stub, 1);

        let id = submit_capped(
            &lane,
            &[
                ("item-0", "item-0"),
                ("item-1", "item-1"),
                ("item-2", "item-2"),
            ],
            0.01,
        )
        .await;
        let view = wait_terminal(&lane, &id).await;

        // The batch stopped rather than completed: it was cut off below its item count, and the cap is
        // the reason. Nothing predicted the cost — the ledger was read after item-0 settled.
        assert_eq!(view["status"], "cancelled");
        assert_eq!(view["budget_exceeded"], json!(true));
        assert_eq!(view["stopped_reason"], json!("max_cost_usd_exceeded"));
        assert!(view["cancelled_at"].is_number());
        assert_eq!(count_of(&view, "total"), 3);
        // Exactly one item ran: the other two were never dispatched, so they are neither completed nor
        // failed — the same treatment a cancellation gives them.
        assert_eq!(count_of(&view, "completed") + count_of(&view, "failed"), 1);
        assert_eq!(stub.calls(), 1);
        // A single item at a dollar a second is well past the one-cent cap, and the report says how far.
        assert!(view["usage"]["cost_usd"].as_f64().unwrap_or(0.0) > 0.01);
        // The item that ran is on disk with its result; the ones that did not are absent, not failed.
        let output = view["output_file_id"]
            .as_str()
            .expect("the item that ran has an output file");
        assert_eq!(custom_ids(&result_lines(&lane, output).await).len(), 1);
    }

    #[tokio::test]
    async fn a_batch_below_its_cap_runs_every_item_and_reports_no_stop() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let price = 3.03 / 3600.0;
        let answerer: Answerer = Arc::new(|model: &str, _nth: usize, _request: &ItemRequest| {
            Answer::Now(ok_reply(model, 5, 5))
        });
        let stub = Arc::new(Stub::new(following(10, UNLIMITED, Some(price)), answerer));
        let lane = stub_lane(root.path(), Arc::new(AdmissionPool::new()), &stub, 2);

        let id = submit_capped(&lane, &[("item-0", "item-0"), ("item-1", "item-1")], 1000.0).await;
        let view = wait_terminal(&lane, &id).await;

        // Under its cap the batch is ordinary: every item ran, and the report says nothing stopped it.
        assert_eq!(view["status"], "completed");
        assert_eq!(view["budget_exceeded"], json!(false));
        assert_eq!(view["max_cost_usd"], json!(1000.0));
        assert!(view["stopped_reason"].is_null());
        assert_eq!(count_of(&view, "completed"), 2);
        assert_eq!(count_of(&view, "failed"), 0);
        assert_eq!(stub.calls(), 2);
    }

    #[tokio::test]
    async fn a_batch_with_no_cap_reports_a_null_cap() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let answerer: Answerer = Arc::new(|model: &str, _nth: usize, _request: &ItemRequest| {
            Answer::Now(ok_reply(model, 1, 1))
        });
        let stub = Arc::new(Stub::new(following(10, UNLIMITED, None), answerer));
        let lane = stub_lane(root.path(), Arc::new(AdmissionPool::new()), &stub, 1);
        let id = submit(&lane, &[("item-0", "item-0")]).await;
        let view = wait_terminal(&lane, &id).await;
        assert_eq!(view["status"], "completed");
        assert!(view["max_cost_usd"].is_null());
        assert_eq!(view["budget_exceeded"], json!(false));
        assert!(view["stopped_reason"].is_null());
    }
}
