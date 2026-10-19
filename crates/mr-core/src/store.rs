//! The runtime store: SQLite for everything the router must still know after a restart.
//!
//! ## What lives here — and what deliberately does not
//!
//! * **Usage records** — which account served which model, how many prompt and completion tokens
//!   the upstream reported, how long it took, and which hedge participant won and which lost.
//! * **Account health** — the cooldown state machine, its reset instant, its consecutive-failure
//!   counter and its transient/permanent failure classification, so a permanently rejected
//!   account is not retried forever.
//! * **OAuth credential metadata** — credential id, platform, the vault reference, scopes, expiry
//!   and a single-use marker. **Never a token value**: the access and refresh tokens live
//!   encrypted in the vault (`crate::secret`), and this table only ever names their id.
//!   `docs/21-security.md` §3 makes "a leaked database file contains no credential material" a
//!   testable claim, and a test below reads the raw database bytes to check it.
//! * **An `oplog`** — the append-only write log that v2 cross-instance sync will replay
//!   (`plan.md` §18). v1 writes it from day one and only ever counts its pending rows.
//!
//! The *authored* configuration (providers, accounts, combos) is **not** here: that is JSON the
//! operator can hand-edit, kept in `config.json` so the database is never the only description of
//! the routing policy. This file is pure runtime state, so a corrupted or deleted database costs
//! history and cooldowns, never the configuration.
//!
//! ## The four v2 invariants, enforced from the first migration
//!
//! 1. Every row is keyed by a UUIDv7 string (`crate::ids::new_id`), never an autoincrement, so two
//!    instances can merge without id collisions.
//! 2. Every row carries `created_at`/`updated_at` (milliseconds since the epoch, UTC) and a
//!    monotonic `revision`, both maintained here — the repository layer is the only writer
//!    (`plan.md` §18 invariant 4), which is the choke point a replication log hooks into.
//! 3. Removal is a `deleted_at` **tombstone**, never `DELETE FROM …`. A hard delete is
//!    indistinguishable from "the peer never had it": if instance A deletes a credential and
//!    instance B never had one, replaying nothing keeps B's copy alive forever, silently. A
//!    tombstone is an event with a timestamp, so a merge can tell "removed" from "never existed"
//!    and the losing value stays visible instead of being silently lost (`plan.md` §18
//!    invariant 2).
//! 4. Timestamps are milliseconds (`crate::ids::Millis`) because quota windows are minutes long
//!    and latency reporting needs sub-second resolution — seconds would make a hedge race and its
//!    usage row disagree about when things happened.
//!
//! ## The bounded usage channel
//!
//! The request path must never wait on a disk. [`Store::usage_queue`] hands out a bounded,
//! non-blocking channel: [`UsageChannel::try_record`] either queues instantly or drops the
//! record — but a drop is **counted and surfaced** through [`UsageStats`], never silent. The
//! tradeoff is deliberate: losing a *count* under overload is a smaller failure than a stalled
//! request. Quota counters are eventually consistent by design (`plan.md` §11 — deltas flush
//! after the response), whereas a request path that blocks on `fsync` turns one slow disk into a
//! p99 incident for every client at once. A drop *without* a counter, though, would be the worst
//! of both worlds: an undercounted quota that looks exact — so the drop count is part of the
//! API, and `pending() = accepted − written − failed` never lies about where records went.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::restrict_directory;
use crate::dialect::AccountState;
use crate::error::{Error, Result};
use crate::ids::{new_id, AccountId, Millis, OAuthCredentialId, Revision};
use crate::secret::looks_like_a_credential;
use crate::SCHEMA_VERSION;

/// How long a connection waits for a lock held by another process before failing.
///
/// The conductor and the panel are separate processes over the same file; without a timeout the
/// loser of a write race gets an immediate `SQLITE_BUSY` and the operator sees a random
/// "database is locked" error. This bounds the wait, it does not change who wins: SQLite's
/// locking guarantees the write is either applied or not, never half-applied.
const BUSY_TIMEOUT: Duration = Duration::from_millis(5_000);

/// Every migration this build knows how to apply, in order.
///
/// The migration for schema version *n* lives at index *n − 1*, and `PRAGMA user_version` holds
/// how many have already been applied, so the runner is simply `migrations[user_version..]`.
///
/// Each migration must be **idempotent** (`IF NOT EXISTS`): the version bump rides in the same
/// transaction as the migration it names, but if a future edit ever splits them, a re-run after a
/// crash must not fail — a migration that errors on its second attempt bricks `open` for a
/// database whose contents are actually fine.
const MIGRATIONS: &[&str] = &[MIGRATION_V1, MIGRATION_V2, MIGRATION_V3];

/// Schema v1: four tables, their indexes, and the revision/tombstone columns every row carries
/// for v2 sync (see the module docs).
///
/// ### Why the `oplog` is written now, in v1, when v1 only counts it
///
/// The oplog is append-only history of every mutation: `(entity, id, revision, op, updated_at,
/// payload_hash)`. v2 cross-instance sync replays it. If v1 did not write it, then at v2 the
/// router would face a **backfill**: invent history it never kept, which means guessing revisions
/// and timestamps for every existing row — or shipping a sync that treats every record as "new",
/// so the first sync between two instances duplicates or clobbers work that was already done.
/// Writing one small append-only row per mutation from day one costs microseconds today and is
/// the difference between a sync layer with provenance and one that starts amnesia-struck.
///
/// The oplog stores a **hash** of the payload, not the payload: it cannot leak a secret to a peer,
/// and the losing value of a merge is already retained by the tombstone/revision columns of the
/// row itself. v1 reads this table for exactly one thing: [`Store::pending_changes`].
///
/// Usage rows are deliberately **not** oplogged: usage is instance-local telemetry (each instance
/// has its own traffic; replicating it would attribute requests to machines that never served
/// them), and `plan.md` §18 scopes sync to credentials, combos and settings. Every sync-relevant
/// entity is logged.
const MIGRATION_V1: &str = r#"
CREATE TABLE IF NOT EXISTS account_health (
    account_id           TEXT PRIMARY KEY,
    state                TEXT NOT NULL,
    reset_at             INTEGER,
    consecutive_failures INTEGER NOT NULL DEFAULT 0,
    last_failure_kind    TEXT,
    last_failure_reason  TEXT,
    created_at           INTEGER NOT NULL,
    updated_at           INTEGER NOT NULL,
    revision             INTEGER NOT NULL DEFAULT 1,
    deleted_at           INTEGER
);

CREATE TABLE IF NOT EXISTS usage (
    id                TEXT PRIMARY KEY,
    account_id        TEXT NOT NULL,
    model             TEXT NOT NULL,
    prompt_tokens     INTEGER,
    completion_tokens INTEGER,
    duration_ms       INTEGER NOT NULL,
    winner_step       INTEGER,
    loser_steps       TEXT NOT NULL DEFAULT '[]',
    occurred_at       INTEGER NOT NULL,
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL,
    revision          INTEGER NOT NULL DEFAULT 1,
    deleted_at        INTEGER
);

CREATE INDEX IF NOT EXISTS usage_by_time ON usage(occurred_at);
CREATE INDEX IF NOT EXISTS usage_by_account ON usage(account_id, occurred_at);
CREATE INDEX IF NOT EXISTS usage_by_model ON usage(model, occurred_at);

CREATE TABLE IF NOT EXISTS oauth_credential (
    id          TEXT PRIMARY KEY,
    platform    TEXT NOT NULL,
    vault_ref   TEXT NOT NULL,
    label       TEXT NOT NULL DEFAULT '',
    scopes      TEXT NOT NULL DEFAULT '[]',
    expires_at  INTEGER,
    single_use  INTEGER NOT NULL DEFAULT 0,
    consumed_at INTEGER,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    revision    INTEGER NOT NULL DEFAULT 1,
    deleted_at  INTEGER
);

CREATE INDEX IF NOT EXISTS oauth_by_platform ON oauth_credential(platform);

-- WITHOUT ROWID: the log is only ever read back through its primary key, so the B-tree key *is*
-- the row and the append path does not pay for a second indirection.
CREATE TABLE IF NOT EXISTS oplog (
    entity       TEXT NOT NULL,
    id           TEXT NOT NULL,
    revision     INTEGER NOT NULL,
    op           TEXT NOT NULL,
    updated_at   INTEGER NOT NULL,
    payload_hash TEXT NOT NULL,
    synced_at    INTEGER,
    PRIMARY KEY (entity, id, revision)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS oplog_pending ON oplog(synced_at);
"#;

/// Schema v2: the per-key usage ledger and the provider quota cache.
///
/// ### Two new tables, not an `ALTER`
///
/// `ALTER TABLE ... ADD COLUMN` is **not idempotent** in SQLite — running it twice fails with
/// "duplicate column name" — and the migration runner's invariant is that a re-run after a crash
/// must never fail. A migration that errors on its second attempt bricks `open` for a database
/// whose contents are actually fine. So v2 adds tables rather than columns.
///
/// ### `key_usage` is a ledger, not a counter
///
/// Each row is one request's token spend, stamped with when it happened. A stored counter plus a
/// `window_started_at` would be a **tumbling** window: it resets mid-traffic and silently hands
/// back quota the operator has already spent. Rolling windows need the history, so the history is
/// what is kept.
///
/// ### `provider_quota_state` is a cache, not a source of truth
///
/// The provider's own quota is fetched over the network and is only as fresh as the last fetch.
/// Rows are keyed by `(provider_id, account_id)` and carry the fetched payload verbatim, so the
/// panel can show the last known state without a network call.
const MIGRATION_V2: &str = r#"
CREATE TABLE IF NOT EXISTS key_usage (
    id                TEXT PRIMARY KEY,
    client_key_id     TEXT NOT NULL,
    prompt_tokens     INTEGER,
    completion_tokens INTEGER,
    occurred_at       INTEGER NOT NULL,
    created_at        INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS key_usage_by_key_time ON key_usage(client_key_id, occurred_at);

CREATE TABLE IF NOT EXISTS provider_quota_state (
    provider_id TEXT NOT NULL,
    account_id  TEXT NOT NULL,
    fetched_at  INTEGER NOT NULL,
    source      TEXT NOT NULL,
    payload     TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    PRIMARY KEY (provider_id, account_id)
);
"#;

/// Schema v3: the per-usage token side table.
///
/// `usage` carries prompt and completion tokens. This table carries what the *provider* reported
/// about those tokens — how many were served from the prompt cache, how many the request wrote
/// into the cache, and how long the generation window was — because those numbers are not
/// available for every provider and are needed to divide tokens by time honestly.
///
/// It is a side table rather than more columns on `usage` for the same reason v2 used new tables:
/// `ALTER TABLE ... ADD COLUMN` is not idempotent, and the migration runner must survive a
/// re-run.
const MIGRATION_V3: &str = r#"
CREATE TABLE IF NOT EXISTS usage_tokens (
    usage_id           TEXT PRIMARY KEY,
    cached_tokens      INTEGER,
    cache_write_tokens INTEGER,
    generation_ms      INTEGER,
    created_at         INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS usage_tokens_by_usage ON usage_tokens(usage_id);
"#;

/// Compile-time guarantee that every migration has a schema version and vice versa.
///
/// A mismatch is a programmer error, not a runtime condition, so the compiler catches it rather
/// than a panic on the open path — this build must never open a database whose version it cannot
/// account for.
const _: () = assert!(
    MIGRATIONS.len() == SCHEMA_VERSION as usize,
    "MIGRATIONS and SCHEMA_VERSION must be bumped together"
);

// -------------------------------------------------------------------------------------------------
// Failure classification
// -------------------------------------------------------------------------------------------------

/// How an account attempt failed, which decides whether it is ever tried again automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FailureKind {
    /// The upstream said "later": a timeout, a 5xx, a 429. Retry after a cooldown.
    Transient,
    /// The credential itself is refused: a 401 that survived a refresh, a revoked grant, an
    /// account removed from the plan. Retrying this forever would burn latency on an answer that
    /// will not change until a human intervenes — the classification exists so that "permanent"
    /// maps to a state with **no** reset instant and therefore no automatic retry.
    Permanent,
}

impl FailureKind {
    /// The stable string stored in the database.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Transient => "transient",
            Self::Permanent => "permanent",
        }
    }

    /// Parse the stored string; `None` for anything else.
    ///
    /// An unknown value can only mean corruption (a newer build is refused at `open`), and it
    /// must surface as an error rather than being silently classified as retryable:
    /// misclassifying a permanent failure as transient is exactly the "retried forever" bug this
    /// enum exists to prevent.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "transient" => Some(Self::Transient),
            "permanent" => Some(Self::Permanent),
            _ => None,
        }
    }
}

impl fmt::Display for FailureKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// -------------------------------------------------------------------------------------------------
// Account health
// -------------------------------------------------------------------------------------------------

/// The persisted health of one account: its state, when it may be tried again, and how it failed.
///
/// This is the row the scheduler reads so a rate-limited account is skipped until its reset and a
/// permanently dead one is skipped until an operator looks at it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountHealth {
    /// The account this describes.
    pub account_id: AccountId,
    /// Current state (`ready`, `backoff`, `auth-failed`, …), from the config vocabulary.
    pub state: AccountState,
    /// When the cooldown expires, or `None` when there is no scheduled retry.
    ///
    /// `None` on a *failure* row means permanent: nothing re-arms it automatically, which is how
    /// a rejected OAuth grant stops being retried forever. `None` on a `ready` row simply means
    /// "no cooldown".
    pub reset_at: Option<Millis>,
    /// Failures since the last success. The panel's "flapping account" warning reads this; it
    /// resets only on a success, never on time passing (an account that fails every other hour
    /// must not look healthy just because its cooldowns expired).
    pub consecutive_failures: u32,
    /// Whether the last failure was transient or permanent.
    pub last_failure_kind: Option<FailureKind>,
    /// Short human-readable reason (HTTP status, provider error code) for the panel.
    pub last_failure_reason: Option<String>,
    /// When this row was first created.
    pub created_at: Millis,
    /// Last write, per `plan.md` §18 invariant 2.
    pub updated_at: Millis,
    /// Monotonic revision, bumped by every write through the repository layer.
    pub revision: Revision,
    /// Tombstone instant; the row is kept so a sync peer can see the removal.
    pub deleted_at: Option<Millis>,
}

impl AccountHealth {
    /// Whether a cooldown has elapsed as of `now` — i.e. the scheduler may probe again.
    ///
    /// A row with `reset_at: None` is never due. That is the permanent-failure path: an
    /// `AuthFailed` account stays out of rotation until a human or a successful token refresh
    /// writes a new state, instead of being re-probed until the end of time.
    pub fn retry_due(&self, now: Millis) -> bool {
        self.deleted_at.is_none() && self.reset_at.is_some_and(|at| at <= now)
    }
}

/// One row of [`AccountHealth`] as SQLite holds it: strings and integers, no interpretation.
///
/// Kept separate from the typed struct so a parse failure is reported as [`Error::Store`] with
/// the offending value, not swallowed inside a rusqlite conversion error.
#[derive(Debug)]
struct RawAccountHealth {
    account_id: String,
    state: String,
    reset_at: Option<i64>,
    consecutive_failures: i64,
    last_failure_kind: Option<String>,
    last_failure_reason: Option<String>,
    created_at: i64,
    updated_at: i64,
    revision: i64,
    deleted_at: Option<i64>,
}

impl RawAccountHealth {
    fn into_typed(self) -> Result<AccountHealth> {
        let state = parse_state(&self.state)?;
        let kind = match self.last_failure_kind.as_deref() {
            None => None,
            Some(s) => Some(
                FailureKind::parse(s)
                    .ok_or_else(|| Error::Store(format!("unknown failure kind {s:?}")))?,
            ),
        };
        Ok(AccountHealth {
            account_id: AccountId::from_raw(self.account_id),
            state,
            reset_at: self.reset_at.map(Millis::from_millis),
            // A negative or absurd streak can only come from a corrupted row; clamp rather than
            // let a wrapped integer display as "four billion failures".
            consecutive_failures: u32::try_from(
                self.consecutive_failures.clamp(0, i64::from(u32::MAX)),
            )
            .unwrap_or(u32::MAX),
            last_failure_kind: kind,
            last_failure_reason: self.last_failure_reason,
            created_at: Millis::from_millis(self.created_at),
            updated_at: Millis::from_millis(self.updated_at),
            revision: u64::try_from(self.revision).unwrap_or(0),
            deleted_at: self.deleted_at.map(Millis::from_millis),
        })
    }
}

/// Parse an [`AccountState`] from its stable string form (`AccountState::as_str`).
fn parse_state(s: &str) -> Result<AccountState> {
    match s {
        "ready" => Ok(AccountState::Ready),
        "disabled" => Ok(AccountState::Disabled),
        "rate-limited" => Ok(AccountState::RateLimited),
        "backoff" => Ok(AccountState::Backoff),
        "auth-failed" => Ok(AccountState::AuthFailed),
        "quota-exhausted" => Ok(AccountState::QuotaExhausted),
        // A newer build cannot have written this — `open` refuses newer schema versions — so an
        // unknown state is corruption, and defaulting it to `Ready` would route live traffic at a
        // broken account. Fail the read instead: a visible error beats a silent wrong answer.
        other => Err(Error::Store(format!("unknown account state {other:?}"))),
    }
}

/// The `account_health` columns, in the order [`row_to_raw_account`] reads them.
const ACCOUNT_COLUMNS: &str = "account_id, state, reset_at, consecutive_failures, \
     last_failure_kind, last_failure_reason, created_at, updated_at, revision, deleted_at";

fn row_to_raw_account(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawAccountHealth> {
    Ok(RawAccountHealth {
        account_id: row.get(0)?,
        state: row.get(1)?,
        reset_at: row.get(2)?,
        consecutive_failures: row.get(3)?,
        last_failure_kind: row.get(4)?,
        last_failure_reason: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
        revision: row.get(8)?,
        deleted_at: row.get(9)?,
    })
}

/// What changed about an account, so all three entry points share one write path — and one
/// oplog-append path. A write that bypassed this enum would bypass the log, and a gap in the log
/// is a silent divergence between instances in v2.
enum AccountChange {
    /// An attempt failed: bump the streak, classify, and arm (or deliberately do not arm) a
    /// cooldown.
    Failure {
        kind: FailureKind,
        reason: String,
        cooldown_millis: i64,
    },
    /// An attempt succeeded: usable again, streak over, failure fields cleared.
    Success,
    /// An explicit state change — operator disable, a 429 with `retry-after`, a quota reset.
    SetState {
        state: AccountState,
        reset_at: Option<Millis>,
    },
}

// -------------------------------------------------------------------------------------------------
// Usage
// -------------------------------------------------------------------------------------------------

/// One request, recorded after the upstream call finished.
///
/// Built on the request path but written by the writer thread. `prompt_tokens` and
/// `completion_tokens` are both optional because a streamed response may report none: the relay
/// sees `usage` in the final SSE frame only if the upstream chose to send one. A type that forced
/// `0` would conflate "reported zero tokens" with "reported nothing", and quota math that
/// silently treats unknown as zero under-reports real consumption.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageRecord {
    /// UUIDv7, minted once. Usage rows are never updated, so the id never changes either.
    pub id: String,
    /// Which account served (or was debited for) the request. Quota attribution keys on this.
    pub account_id: AccountId,
    /// The model as the client asked for it — a combo name or `provider/model`.
    pub model: String,
    /// Prompt tokens, or `None` when the upstream did not report any.
    pub prompt_tokens: Option<u64>,
    /// Completion tokens, or `None` when the upstream did not report any.
    pub completion_tokens: Option<u64>,
    /// How long the upstream call took, in milliseconds.
    pub duration_ms: u64,
    /// Which hedge participant won: the combo-step index (as in `HedgeSpec::participants`) whose
    /// reply was committed to the client, or `None` when the request was not part of a race.
    pub winner_step: Option<u16>,
    /// The participants that lost the race.
    ///
    /// Recorded alongside the winner because a loser is not free: under
    /// `winner_metric = first_complete_token` every loser has already produced billable output,
    /// and reconciliation has to account for tokens no client ever saw (`docs/07-hedged-race.md`).
    pub loser_steps: Vec<u16>,
    /// When the request happened, in milliseconds since the epoch.
    pub occurred_at: Millis,
}

impl UsageRecord {
    /// A record with a fresh UUIDv7 id and the current timestamp.
    pub fn new(
        account_id: impl Into<AccountId>,
        model: impl Into<String>,
        duration_ms: u64,
    ) -> Self {
        Self {
            id: new_id(),
            account_id: account_id.into(),
            model: model.into(),
            prompt_tokens: None,
            completion_tokens: None,
            duration_ms,
            winner_step: None,
            loser_steps: Vec::new(),
            occurred_at: Millis::now(),
        }
    }

    /// Set the token counts the upstream actually reported; `None` stays "unknown".
    pub fn with_tokens(mut self, prompt: Option<u64>, completion: Option<u64>) -> Self {
        self.prompt_tokens = prompt;
        self.completion_tokens = completion;
        self
    }

    /// Record which participant won and which lost.
    pub fn with_hedge(mut self, winner: Option<u16>, losers: Vec<u16>) -> Self {
        self.winner_step = winner;
        self.loser_steps = losers;
        self
    }

    /// Pin the timestamp — for tests, and for replaying a record captured earlier.
    pub fn at(mut self, at: Millis) -> Self {
        self.occurred_at = at;
        self
    }
}

/// One row of [`UsageRecord`] as SQLite holds it, before parsing.
#[derive(Debug)]
// The four audit columns are selected and bound but never read back into the typed record: they
// exist for the sync protocol in `docs/22-sync-design.md`, which reads them through its own query.
// Silencing this here is deliberate — removing the fields would silently change `USAGE_COLUMNS` and
// therefore the column order that `row_to_raw_usage` indexes by position.
#[allow(dead_code)]
struct RawUsage {
    id: String,
    account_id: String,
    model: String,
    prompt_tokens: Option<i64>,
    completion_tokens: Option<i64>,
    duration_ms: i64,
    winner_step: Option<i64>,
    loser_steps: String,
    occurred_at: i64,
    created_at: i64,
    updated_at: i64,
    revision: i64,
    deleted_at: Option<i64>,
}

impl RawUsage {
    fn into_typed(self) -> Result<UsageRecord> {
        let loser_steps: Vec<u16> = serde_json::from_str(&self.loser_steps)
            .map_err(|e| Error::Store(format!("usage.loser_steps is not JSON: {e}")))?;
        // A negative token count is not "unknown", it is corruption — and wrapping it into a
        // huge unsigned value would poison every sum built on this table in a way no later query
        // could detect. Unknown and corrupt both fold to `None`.
        Ok(UsageRecord {
            id: self.id,
            account_id: AccountId::from_raw(self.account_id),
            model: self.model,
            prompt_tokens: self.prompt_tokens.and_then(|v| u64::try_from(v).ok()),
            completion_tokens: self.completion_tokens.and_then(|v| u64::try_from(v).ok()),
            duration_ms: u64::try_from(self.duration_ms).unwrap_or(0),
            winner_step: self.winner_step.and_then(|v| u16::try_from(v).ok()),
            loser_steps,
            occurred_at: Millis::from_millis(self.occurred_at),
        })
    }
}

/// The `usage` columns, in the order [`row_to_raw_usage`] reads them.
const USAGE_COLUMNS: &str = "id, account_id, model, prompt_tokens, completion_tokens, \
     duration_ms, winner_step, loser_steps, occurred_at, created_at, updated_at, revision, \
     deleted_at";

fn row_to_raw_usage(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawUsage> {
    Ok(RawUsage {
        id: row.get(0)?,
        account_id: row.get(1)?,
        model: row.get(2)?,
        prompt_tokens: row.get(3)?,
        completion_tokens: row.get(4)?,
        duration_ms: row.get(5)?,
        winner_step: row.get(6)?,
        loser_steps: row.get(7)?,
        occurred_at: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
        revision: row.get(11)?,
        deleted_at: row.get(12)?,
    })
}

/// Aggregates over a time range, computed by SQLite in a single pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageAggregate {
    /// Number of recorded requests in the range.
    pub requests: u64,
    /// Sum of *reported* prompt tokens. Requests that reported none contribute nothing: a `NULL`
    /// is unknown, so this struct reports sums and counts but no per-request average — an
    /// average would need a denominator of "requests that reported tokens", which would be
    /// silently wrong if folded into `requests`.
    pub prompt_tokens: u64,
    /// Sum of reported completion tokens.
    pub completion_tokens: u64,
    /// Total upstream time, for "where did the latency go" panels.
    pub total_duration_ms: u64,
    /// The slowest single request in the range.
    pub max_duration_ms: u64,
}

/// An aggregate grouped by one key: an account id or a model name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageGroup {
    /// The account id or model name this row covers.
    pub key: String,
    /// The aggregates for that key.
    pub totals: UsageAggregate,
}

// -------------------------------------------------------------------------------------------------
// Per-key usage ledger
// -------------------------------------------------------------------------------------------------

/// One request's token spend, attributed to a client key.
///
/// This is an immutable ledger row: it is written once, when the request finishes, and never
/// updated. Rolling windows are computed from it, which is what makes them *rolling* — a window
/// that started at 09:00 does not forget what happened at 08:55 when it is read at 09:30.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyUsageRecord {
    /// The row id (a UUIDv7 string).
    pub id: String,
    /// The client key that made the request.
    pub client_key_id: String,
    /// Prompt tokens the upstream reported, if it reported any.
    pub prompt_tokens: Option<u64>,
    /// Completion tokens the upstream reported, if it reported any.
    pub completion_tokens: Option<u64>,
    /// When the request happened, in milliseconds since the epoch.
    pub occurred_at: Millis,
    /// When the row was written, in milliseconds since the epoch.
    pub created_at: Millis,
}

impl KeyUsageRecord {
    /// Build a record for a key, stamping it with the current time.
    ///
    /// The id is a fresh UUIDv7, so two concurrent requests for the same key never collide.
    pub fn new(client_key_id: impl Into<String>, now: Millis) -> Self {
        Self {
            id: crate::ids::new_id(),
            client_key_id: client_key_id.into(),
            prompt_tokens: None,
            completion_tokens: None,
            occurred_at: now,
            created_at: now,
        }
    }

    /// Attach the tokens the upstream reported.
    pub fn with_tokens(mut self, prompt: Option<u64>, completion: Option<u64>) -> Self {
        self.prompt_tokens = prompt;
        self.completion_tokens = completion;
        self
    }

    /// Move the occurrence time, for a caller that already knows when the request happened.
    pub fn at(mut self, at: Millis) -> Self {
        self.occurred_at = at;
        self
    }
}

/// The token totals of one key over one window.
///
/// The three sums are kept separate rather than added together because the two limits are
/// different: a request cap counts *requests*, and a token cap counts tokens. A window that
/// collapsed them into one number could not answer either question.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyWindowUsage {
    /// Number of requests recorded in the window.
    pub requests: u64,
    /// Prompt tokens in the window.
    pub prompt_tokens: u64,
    /// Completion tokens in the window.
    pub completion_tokens: u64,
}

impl KeyWindowUsage {
    /// Prompt plus completion tokens.
    pub fn total_tokens(&self) -> u64 {
        self.prompt_tokens.saturating_add(self.completion_tokens)
    }
}

// -------------------------------------------------------------------------------------------------
// The per-usage token side table
// -------------------------------------------------------------------------------------------------

/// The token breakdown a provider reported for one request.
///
/// Every field is optional: a provider that reports cached tokens but not a generation window
/// leaves that field `None` rather than inventing a zero. `None` means "not reported"; `Some(0)`
/// means "reported as zero".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageTokenBreakdown {
    /// Tokens served from the prompt cache.
    pub cached_tokens: Option<u64>,
    /// Tokens the request wrote into the prompt cache.
    pub cache_write_tokens: Option<u64>,
    /// How long the generation window was, in milliseconds.
    pub generation_ms: Option<u64>,
}

// -------------------------------------------------------------------------------------------------
// Provider quota cache
// -------------------------------------------------------------------------------------------------

/// The last known quota state of one provider account.
///
/// The provider's quota is fetched over the network, so this is a **cache**: it is exactly as
/// fresh as `fetched_at` says, and the panel must present it as such. The payload is the
/// serialized fetch result, stored verbatim so a panel change does not need a migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderQuotaState {
    /// The provider this belongs to.
    pub provider_id: String,
    /// The account within that provider.
    pub account_id: AccountId,
    /// When the fetch happened, in milliseconds since the epoch.
    pub fetched_at: Millis,
    /// Where the numbers came from (`provider`, `manual`, …).
    pub source: String,
    /// The serialized fetch result.
    pub payload: String,
    /// When the row was first written.
    pub created_at: Millis,
    /// When the row was last updated.
    pub updated_at: Millis,
}

impl ProviderQuotaState {
    /// Build a cache row, stamped with the current time.
    pub fn new(
        provider_id: impl Into<String>,
        account_id: AccountId,
        source: impl Into<String>,
        payload: impl Into<String>,
    ) -> Self {
        let now = Millis::now();
        Self {
            provider_id: provider_id.into(),
            account_id,
            fetched_at: now,
            source: source.into(),
            payload: payload.into(),
            created_at: now,
            updated_at: now,
        }
    }
}

// -------------------------------------------------------------------------------------------------
// OAuth credential metadata
// -------------------------------------------------------------------------------------------------

/// The stored *metadata* of one OAuth credential set — never the tokens themselves.
///
/// The access and refresh tokens are in the vault under `vault_ref` (`docs/21-security.md` §3:
/// a leaked database file must contain no credential material). This table is what the panel and
/// the refresh scheduler need to find, refresh and expire credentials without ever holding a key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthCredentialRecord {
    /// UUIDv7 credential id; stable across export and import so two instances merge it as one
    /// record instead of creating a duplicate.
    pub id: OAuthCredentialId,
    /// Which platform this belongs to: `codex`, `copilot`, `antigravity`, …
    pub platform: String,
    /// The vault id the token material is stored under — for example
    /// `provider/openadapter/account/<uuid>`. **Never a token.**
    pub vault_ref: String,
    /// Account label (email, login, `key-3`) shown by the panel.
    pub label: String,
    /// Granted scopes, as returned by the provider.
    pub scopes: Vec<String>,
    /// Access-token expiry, when the provider declared one.
    pub expires_at: Option<Millis>,
    /// Whether this credential may be spent exactly once (an authorization code already
    /// exchanged, a device-code result).
    pub single_use: bool,
    /// When the single-use marker was spent; `None` while it is still usable.
    pub consumed_at: Option<Millis>,
    /// When this row was first created.
    pub created_at: Millis,
    /// The caller's last-write timestamp (informational); the store stamps its own `now` on
    /// write — see [`Store::put_oauth_credential`].
    pub updated_at: Millis,
    /// Monotonic revision.
    pub revision: Revision,
    /// Tombstone instant.
    pub deleted_at: Option<Millis>,
}

impl OAuthCredentialRecord {
    /// A new record pointing at `vault_ref` on `platform`.
    pub fn new(platform: impl Into<String>, vault_ref: impl Into<String>) -> Self {
        let now = Millis::now();
        Self {
            id: OAuthCredentialId::generate(),
            platform: platform.into(),
            vault_ref: vault_ref.into(),
            label: String::new(),
            scopes: Vec::new(),
            expires_at: None,
            single_use: false,
            consumed_at: None,
            created_at: now,
            updated_at: now,
            revision: 0,
            deleted_at: None,
        }
    }

    /// Set the granted scopes.
    pub fn with_scopes(mut self, scopes: Vec<String>) -> Self {
        self.scopes = scopes;
        self
    }

    /// Set the access-token expiry.
    pub fn with_expiry(mut self, expires_at: Millis) -> Self {
        self.expires_at = Some(expires_at);
        self
    }

    /// Mark the credential as single-use.
    pub fn single_use(mut self) -> Self {
        self.single_use = true;
        self
    }

    /// Set the display label.
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }
}

/// One row of [`OAuthCredentialRecord`] before parsing.
#[derive(Debug)]
struct RawOAuthCredential {
    id: String,
    platform: String,
    vault_ref: String,
    label: String,
    scopes: String,
    expires_at: Option<i64>,
    single_use: i64,
    consumed_at: Option<i64>,
    created_at: i64,
    updated_at: i64,
    revision: i64,
    deleted_at: Option<i64>,
}

impl RawOAuthCredential {
    fn into_typed(self) -> Result<OAuthCredentialRecord> {
        let scopes: Vec<String> = serde_json::from_str(&self.scopes)
            .map_err(|e| Error::Store(format!("oauth_credential.scopes is not JSON: {e}")))?;
        Ok(OAuthCredentialRecord {
            id: OAuthCredentialId::from_raw(self.id),
            platform: self.platform,
            vault_ref: self.vault_ref,
            label: self.label,
            scopes,
            expires_at: self.expires_at.map(Millis::from_millis),
            single_use: self.single_use != 0,
            consumed_at: self.consumed_at.map(Millis::from_millis),
            created_at: Millis::from_millis(self.created_at),
            updated_at: Millis::from_millis(self.updated_at),
            revision: u64::try_from(self.revision).unwrap_or(0),
            deleted_at: self.deleted_at.map(Millis::from_millis),
        })
    }
}

/// The `oauth_credential` columns, in the order [`row_to_raw_oauth`] reads them.
const OAUTH_COLUMNS: &str = "id, platform, vault_ref, label, scopes, expires_at, single_use, \
     consumed_at, created_at, updated_at, revision, deleted_at";

fn row_to_raw_oauth(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawOAuthCredential> {
    Ok(RawOAuthCredential {
        id: row.get(0)?,
        platform: row.get(1)?,
        vault_ref: row.get(2)?,
        label: row.get(3)?,
        scopes: row.get(4)?,
        expires_at: row.get(5)?,
        single_use: row.get(6)?,
        consumed_at: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
        revision: row.get(10)?,
        deleted_at: row.get(11)?,
    })
}

/// Reject a value that looks like credential material before it can reach a row.
///
/// The store's security posture is "the database contains references, never secrets"
/// (`docs/21-security.md` §3). A field that is itself a token is the mistake a future caller
/// *will* make — "just stuff the refresh token in the ref column for now" — and this turns that
/// mistake into an error at the repository boundary instead of a live key in every database dump,
/// backup and support bundle.
fn reject_credential_material(what: &str, value: &str, id: &str) -> Result<()> {
    if looks_like_a_credential(value) {
        return Err(Error::Store(format!(
            "{what} of credential {id} looks like a token, not a reference; store the token in \
             the vault and keep only its id here (docs/21-security.md §3)"
        )));
    }
    Ok(())
}

// -------------------------------------------------------------------------------------------------
// Oplog
// -------------------------------------------------------------------------------------------------

/// Hex-encoded SHA-256 of a payload — the oplog's representation of *what* changed.
///
/// A hash rather than the value: the log may one day be shipped to a peer or printed in a debug
/// dump, and neither must carry credential material or account labels. Identity is enough for
/// reconciliation ("did both sides see the same write?"), which is all v1 and v2 use it for.
pub fn payload_hash(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Append one oplog entry inside the caller's transaction.
///
/// Called from every write path *before* `commit`, never after: if the log and the write were
/// separate transactions, a crash between them would leave a committed change no sync peer ever
/// learns about — a divergence that surfaces weeks later as a "missing" record nobody can
/// explain. `INSERT OR IGNORE` makes a duplicate append (same entity/id/revision) a no-op instead
/// of an error.
fn append_oplog(
    conn: &Connection,
    entity: &str,
    id: &str,
    revision: Revision,
    op: &str,
    now: Millis,
    payload: &[u8],
) -> Result<()> {
    let hash = payload_hash(payload);
    // A revision beyond i64::MAX would mean nine quintillion writes; clamp rather than wrap into
    // a negative key that would collide with the log's earliest entries.
    let revision = i64::try_from(revision).unwrap_or(i64::MAX);
    conn.execute(
        "INSERT OR IGNORE INTO oplog (entity, id, revision, op, updated_at, payload_hash, \
         synced_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)",
        params![entity, id, revision, op, now.as_millis(), hash],
    )?;
    Ok(())
}

// -------------------------------------------------------------------------------------------------
// Usage statistics for the bounded channel
// -------------------------------------------------------------------------------------------------

/// What the usage channel and its writer are doing, so nothing is lost *silently*.
///
/// The four counters are distinct on purpose:
/// * `accepted` — queued successfully.
/// * `dropped`  — the queue was full (or its writer gone) and the record was refused: this is
///   the overload signal an operator must be able to see and alert on.
/// * `written`  — actually committed to SQLite.
/// * `failed`   — the writer took it but the disk refused it (a real I/O error, not overload).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageStats {
    /// Records accepted into the channel.
    pub accepted: u64,
    /// Records dropped because the channel was full or closed. Counted, never silent.
    pub dropped: u64,
    /// Records written to SQLite.
    pub written: u64,
    /// Records the writer could not persist (disk error).
    pub failed: u64,
}

impl UsageStats {
    /// Records accepted but neither written nor failed — sitting in the queue right now.
    pub fn pending(&self) -> u64 {
        self.accepted
            .saturating_sub(self.written.saturating_add(self.failed))
    }
}

/// Counters shared by every clone of a channel and by the writer draining it.
#[derive(Debug, Default)]
struct UsageCounters {
    accepted: AtomicU64,
    dropped: AtomicU64,
    written: AtomicU64,
    failed: AtomicU64,
}

impl UsageCounters {
    fn snapshot(&self) -> UsageStats {
        UsageStats {
            accepted: self.accepted.load(Ordering::Acquire),
            dropped: self.dropped.load(Ordering::Acquire),
            written: self.written.load(Ordering::Acquire),
            failed: self.failed.load(Ordering::Acquire),
        }
    }
}

/// The request-path side of the bounded usage channel.
///
/// [`try_record`](UsageChannel::try_record) never blocks, never touches SQLite and never awaits:
/// at most it increments a counter. Clone it freely — all clones share one queue and one set of
/// counters, so the drop count is correct no matter how many tasks report usage.
#[derive(Clone)]
pub struct UsageChannel {
    tx: SyncSender<UsageRecord>,
    capacity: usize,
    counters: Arc<UsageCounters>,
}

impl fmt::Debug for UsageChannel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UsageChannel")
            .field("capacity", &self.capacity)
            .field("stats", &self.counters.snapshot())
            .finish()
    }
}

impl UsageChannel {
    /// Hand one record to the writer without ever blocking.
    ///
    /// Returns `false` when the record was **dropped** — because the queue is full (the disk is
    /// slower than the traffic) or because the writer has gone away (a shutdown race). Both
    /// cases increment the `dropped` counter; nothing disappears without a trace. Blocking here
    /// instead would make a slow disk stall every request, which is precisely the coupling this
    /// channel exists to cut — the failure mode we accept is an undercounted quota delta, and
    /// the counter is what makes that undercount visible rather than invisible.
    pub fn try_record(&self, record: UsageRecord) -> bool {
        match self.tx.try_send(record) {
            Ok(()) => {
                self.counters.accepted.fetch_add(1, Ordering::AcqRel);
                true
            }
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.counters.dropped.fetch_add(1, Ordering::AcqRel);
                false
            }
        }
    }

    /// The shared counters — accepted, dropped, written, failed — for status reporting.
    pub fn stats(&self) -> UsageStats {
        self.counters.snapshot()
    }

    /// How many records this channel (all clones included) accepted.
    pub fn accepted(&self) -> u64 {
        self.counters.snapshot().accepted
    }

    /// How many records this channel dropped, counted precisely.
    pub fn dropped(&self) -> u64 {
        self.counters.snapshot().dropped
    }

    /// How many records are queued or in flight right now.
    pub fn pending(&self) -> u64 {
        self.counters.snapshot().pending()
    }

    /// The queue bound passed at creation (a capacity of `0` is clamped to `1`, because a queue
    /// nothing can ever enter would drop every record while looking perfectly healthy).
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

/// The writer side: owns the receiving end of the channel and the store it writes into.
///
/// Created by [`Store::usage_queue`]. Use [`UsageQueue::drain_now`] for synchronous draining (the
/// flusher's "every ~1 s or every 512 events", `plan.md` §11) or [`UsageQueue::spawn`] to hand
/// the loop to a background thread.
pub struct UsageQueue {
    rx: Receiver<UsageRecord>,
    counters: Arc<UsageCounters>,
    store: Store,
}

impl fmt::Debug for UsageQueue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The store is deliberately not printed. Nothing here holds a credential, but a `Debug` impl
        // that walks into a struct whose job is to own rows is a `Debug` impl that will one day print
        // one, and `finish_non_exhaustive` says so rather than leaving it to the reader.
        f.debug_struct("UsageQueue")
            .field("stats", &self.counters.snapshot())
            .finish_non_exhaustive()
    }
}

impl UsageQueue {
    /// Write every record currently queued, right now, on this thread; returns how many landed.
    ///
    /// Non-blocking with respect to the channel: it drains what is there and moves on.
    pub fn drain_now(&self) -> Result<u64> {
        let batch = self.collect();
        self.write_batch(&batch)
    }

    /// How many records are waiting in the queue.
    ///
    /// Taken from the channel's own counters rather than from the receiver. `std::sync::mpsc::Receiver`
    /// has no `len`, and reaching for one would have been the wrong fix even if it did: the counters
    /// are the authoritative record, because they also account for records currently being written by
    /// the background thread, which the receiver's queue length cannot see. `pending` is accepted
    /// minus written minus failed, so it is the number a caller actually means by "queued".
    pub fn len(&self) -> usize {
        self.counters.snapshot().pending() as usize
    }

    /// Whether the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.counters.snapshot().pending() == 0
    }

    /// The shared counters for this channel.
    pub fn stats(&self) -> UsageStats {
        self.counters.snapshot()
    }

    /// Move the queue to a background thread that writes records as they arrive.
    ///
    /// The thread exits once every [`UsageChannel`] clone has been dropped *and* the queue has
    /// drained — [`UsageWriter::join`] waits for exactly that, so shutdown order is: drop the
    /// channels, then join. Dropping the returned writer without joining detaches the thread,
    /// which is fine: it still drains what is queued and exits on its own.
    pub fn spawn(self) -> Result<UsageWriter> {
        let counters = Arc::clone(&self.counters);
        let handle = std::thread::Builder::new()
            .name("usage-writer".into())
            .spawn(move || self.run())
            // A thread that cannot be spawned is a system-level failure (out of memory, no file
            // descriptors). Reporting it — rather than `expect`ing — lets the caller fall back to
            // `drain_now` instead of feeding a queue nobody will ever read, where every record
            // would pile up until saturation and then drop invisibly.
            .map_err(|e| Error::Store(format!("cannot start the usage writer thread: {e}")))?;
        Ok(UsageWriter {
            handle: Some(handle),
            counters,
        })
    }

    /// The writer loop: block until a record arrives, then batch-drain everything queued behind
    /// it into one transaction. One fsync per burst instead of one per record is what keeps the
    /// disk from becoming the bottleneck this channel was built to hide.
    fn run(&self) {
        let mut disconnected = false;
        while !disconnected {
            let first = match self.rx.recv() {
                Ok(record) => record,
                // Every sender was dropped and the queue is empty: normal shutdown.
                Err(_) => break,
            };
            let mut batch = vec![first];
            loop {
                match self.rx.try_recv() {
                    Ok(record) => batch.push(record),
                    Err(TryRecvError::Empty) => break,
                    // `Disconnected` is only reported once the queue is empty, so this batch is
                    // complete and the thread can exit after writing it.
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
            // Failure is counted (`failed`) and logged inside `write_batch`; the loop keeps
            // running because one ENOSPC or lock timeout must not kill usage recording for the
            // rest of the process's life.
            let _ = self.write_batch(&batch);
        }
    }

    fn collect(&self) -> Vec<UsageRecord> {
        let mut batch = Vec::new();
        while let Ok(record) = self.rx.try_recv() {
            batch.push(record);
        }
        batch
    }

    fn write_batch(&self, batch: &[UsageRecord]) -> Result<u64> {
        if batch.is_empty() {
            return Ok(0);
        }
        match self.store.insert_usage_batch(batch) {
            Ok(n) => {
                self.counters.written.fetch_add(n, Ordering::AcqRel);
                Ok(n)
            }
            Err(e) => {
                // The records are gone — the channel already handed them over — but the loss is
                // counted and logged, which is the difference between an incident report and a
                // mystery. Counting them as `failed` (rather than leaving them "pending") also
                // keeps `pending()` honest instead of showing a queue that never empties.
                self.counters
                    .failed
                    .fetch_add(batch.len() as u64, Ordering::AcqRel);
                tracing::error!(error = %e, count = batch.len(), "usage batch write failed");
                Err(e)
            }
        }
    }
}

/// Handle to the background usage writer started by [`UsageQueue::spawn`].
#[derive(Debug)]
pub struct UsageWriter {
    handle: Option<std::thread::JoinHandle<()>>,
    counters: Arc<UsageCounters>,
}

impl UsageWriter {
    /// The shared counters — accepted, dropped, written, failed — for status reporting.
    pub fn stats(&self) -> UsageStats {
        self.counters.snapshot()
    }

    /// Wait for the writer to drain and exit, and report the final counters.
    ///
    /// Block until every [`UsageChannel`] clone has been dropped first: a live sender keeps the
    /// thread parked in `recv`, and joining with one open would wait forever — the failure mode
    /// of a shutdown that forgot to drop its channels, surfaced as a hang rather than a silent
    /// truncation of the log.
    pub fn join(mut self) -> Result<UsageStats> {
        if let Some(handle) = self.handle.take() {
            // A panic in the writer thread would otherwise be swallowed by `JoinHandle`'s drop;
            // "usage stopped being written" must never be invisible.
            handle
                .join()
                .map_err(|_| Error::Store("usage writer thread panicked while writing".into()))?;
        }
        Ok(self.counters.snapshot())
    }
}

// -------------------------------------------------------------------------------------------------
// The store
// -------------------------------------------------------------------------------------------------

/// The SQLite-backed runtime store.
///
/// Cheap to clone: clones share one connection behind one mutex. A pool was deliberately not
/// used — SQLite allows a single writer at a time regardless, WAL already lets readers run
/// alongside it, and one connection makes the `:memory:` store (where every connection would be a
/// *different* database) work with exactly the same code as a file.
#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
    path: Option<PathBuf>,
}

impl fmt::Debug for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Store")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl Store {
    /// Open or create a store at a path.
    ///
    /// Creates missing parent directories with `0700` — the state directory sits next to the
    /// vault, and while the database holds no secrets, its usage history and account labels are
    /// the operator's (`docs/21-security.md` §3). Then configures WAL, a busy timeout and
    /// foreign keys, and runs migrations.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    Error::Store(format!("cannot create {}: {e}", parent.display()))
                })?;
                restrict_directory(parent)?;
            }
        }
        let mut conn = Connection::open(path)?;
        configure(&mut conn, true)?;
        migrate(&mut conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            path: Some(path.to_path_buf()),
        })
    }

    /// Open an in-memory store, for tests and ephemeral state.
    ///
    /// WAL and `synchronous` are file-system concerns and are skipped: an in-memory database has
    /// no journal to put into write-ahead mode, and asking for one just returns `memory`.
    pub fn open_in_memory() -> Result<Self> {
        let mut conn = Connection::open_in_memory()?;
        configure(&mut conn, false)?;
        migrate(&mut conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            path: None,
        })
    }

    /// The schema version currently applied.
    pub fn schema_version(&self) -> Result<u32> {
        let guard = self.guard()?;
        let version: u32 = guard.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        Ok(version)
    }

    /// The file this store lives on, or `None` for an in-memory store.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    // -- usage -------------------------------------------------------------------------

    /// Record one usage row synchronously on this thread.
    ///
    /// The background path is [`Store::usage_queue`]; this is for tests, for import/replay, and
    /// for a caller that has already committed to doing its own I/O.
    pub fn record_usage(&self, record: &UsageRecord) -> Result<()> {
        self.insert_usage_batch(std::slice::from_ref(record))
            .map(|_| ())
    }

    /// Create the bounded usage channel and its writer end.
    ///
    /// `capacity` is the burst the queue absorbs before `try_record` starts dropping; it is
    /// clamped to at least one slot. The pair shares counters, so [`UsageStats`] always adds up:
    /// `accepted = written + failed + pending`, and `dropped` is everything that never made it
    /// in.
    pub fn usage_queue(&self, capacity: usize) -> (UsageChannel, UsageQueue) {
        let capacity = capacity.max(1);
        let counters = Arc::new(UsageCounters::default());
        let (tx, rx) = sync_channel(capacity);
        (
            UsageChannel {
                tx,
                capacity,
                counters: Arc::clone(&counters),
            },
            UsageQueue {
                rx,
                counters,
                store: self.clone(),
            },
        )
    }

    /// Aggregate usage over the half-open range `[from, to)`.
    ///
    /// Half-open on purpose: two adjacent ranges (`[0, x)` and `[x, y)`) then partition the data
    /// exactly, where an inclusive end would double-count the boundary instant under a steady
    /// stream of requests.
    pub fn usage_totals(&self, from: Millis, to: Millis) -> Result<UsageAggregate> {
        let guard = self.guard()?;
        let totals = guard.query_row(
            &format!("SELECT {AGGREGATE_COLUMNS} FROM usage WHERE {OCCURRED_RANGE}"),
            params![from.as_millis(), to.as_millis()],
            |row| aggregate_from(row, 0),
        )?;
        Ok(totals)
    }

    /// Aggregate usage grouped by account over `[from, to)`.
    pub fn usage_by_account(&self, from: Millis, to: Millis) -> Result<Vec<UsageGroup>> {
        self.usage_grouped("account_id", from, to)
    }

    /// Aggregate usage grouped by model over `[from, to)`.
    pub fn usage_by_model(&self, from: Millis, to: Millis) -> Result<Vec<UsageGroup>> {
        self.usage_grouped("model", from, to)
    }

    /// The usage rows that occurred in `[from, to)`, newest first, up to `limit`.
    ///
    /// The read-back path for the panel's recent-requests table and for the hedge report: the
    /// aggregate queries answer "how much", this one answers "which requests", including which
    /// participant won and which lost.
    pub fn usage_records(&self, from: Millis, to: Millis, limit: u32) -> Result<Vec<UsageRecord>> {
        let guard = self.guard()?;
        let sql = format!(
            "SELECT {USAGE_COLUMNS} FROM usage WHERE {OCCURRED_RANGE} \
             ORDER BY occurred_at DESC, id LIMIT ?3"
        );
        let mut stmt = guard.prepare(&sql)?;
        let rows = stmt.query_map(
            params![from.as_millis(), to.as_millis(), i64::from(limit)],
            row_to_raw_usage,
        )?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?.into_typed()?);
        }
        Ok(out)
    }

    // -- account health ----------------------------------------------------------------

    /// Record a failed attempt: bump the streak, classify the failure and arm a cooldown.
    ///
    /// A [`FailureKind::Transient`] failure lands in `backoff` with `reset_at = now + cooldown`;
    /// a [`FailureKind::Permanent`] one lands in `auth-failed` with **no** `reset_at`, which is
    /// how a rejected credential stops being retried forever (see [`AccountHealth::retry_due`]).
    pub fn record_account_failure(
        &self,
        account_id: &AccountId,
        kind: FailureKind,
        reason: impl Into<String>,
        cooldown: Duration,
        now: Millis,
    ) -> Result<AccountHealth> {
        let reason = reason.into();
        reject_credential_material("failure reason", &reason, account_id.as_str())?;
        let cooldown_millis = i64::try_from(cooldown.as_millis()).unwrap_or(i64::MAX);
        self.write_account_change(
            account_id,
            AccountChange::Failure {
                kind,
                reason,
                cooldown_millis,
            },
            now,
        )
    }

    /// Record a successful attempt: the account is usable again and the streak is cleared.
    ///
    /// The failure fields are cleared rather than kept "for history": the oplog already retains
    /// every previous state, and a `ready` row that still says `permanent` would mislead the
    /// panel into showing an account as healthy-but-doomed.
    pub fn record_account_success(
        &self,
        account_id: &AccountId,
        now: Millis,
    ) -> Result<AccountHealth> {
        self.write_account_change(account_id, AccountChange::Success, now)
    }

    /// Set an account's state explicitly — operator disable/enable, a 429's `retry-after`, a
    /// quota reset — keeping the existing failure history untouched.
    pub fn set_account_state(
        &self,
        account_id: &AccountId,
        state: AccountState,
        reset_at: Option<Millis>,
        now: Millis,
    ) -> Result<AccountHealth> {
        self.write_account_change(account_id, AccountChange::SetState { state, reset_at }, now)
    }

    /// The live health row for an account; `None` if unknown or tombstoned.
    pub fn account_health(&self, account_id: &AccountId) -> Result<Option<AccountHealth>> {
        self.fetch_account(account_id, false)
    }

    /// The health row for an account **including tombstones** — how a sync peer (and the test
    /// for the tombstone invariant) still sees a removed account.
    pub fn account_health_including_deleted(
        &self,
        account_id: &AccountId,
    ) -> Result<Option<AccountHealth>> {
        self.fetch_account(account_id, true)
    }

    /// All health rows, optionally including tombstones, ordered by account id.
    pub fn list_account_health(&self, include_deleted: bool) -> Result<Vec<AccountHealth>> {
        let guard = self.guard()?;
        let sql = if include_deleted {
            format!("SELECT {ACCOUNT_COLUMNS} FROM account_health ORDER BY account_id")
        } else {
            format!(
                "SELECT {ACCOUNT_COLUMNS} FROM account_health WHERE deleted_at IS NULL \
                 ORDER BY account_id"
            )
        };
        let mut stmt = guard.prepare(&sql)?;
        let rows = stmt.query_map([], row_to_raw_account)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?.into_typed()?);
        }
        Ok(out)
    }

    /// Tombstone an account's health row (never a hard delete — see the module docs).
    ///
    /// Returns `false` when there was nothing live to remove, which makes the operation
    /// idempotent under a retried control-plane request.
    pub fn delete_account_health(&self, account_id: &AccountId, now: Millis) -> Result<bool> {
        let mut guard = self.guard()?;
        let tx = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE account_health SET deleted_at = ?2, updated_at = ?2, \
             revision = revision + 1 WHERE account_id = ?1 AND deleted_at IS NULL",
            params![account_id.as_str(), now.as_millis()],
        )?;
        if changed == 0 {
            return Ok(false);
        }
        let raw: RawAccountHealth = tx.query_row(
            &format!("SELECT {ACCOUNT_COLUMNS} FROM account_health WHERE account_id = ?1"),
            params![account_id.as_str()],
            row_to_raw_account,
        )?;
        let revision = u64::try_from(raw.revision).unwrap_or(0);
        let health = raw.into_typed()?;
        let payload = serde_json::to_vec(&health)?;
        append_oplog(
            &tx,
            "account_health",
            account_id.as_str(),
            revision,
            "delete",
            now,
            &payload,
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Accounts whose cooldown has elapsed as of `now`, i.e. the scheduler may probe them again.
    ///
    /// Rows without a `reset_at` — permanent failures — never appear here, no matter how much
    /// time passes. That exclusion is the whole point of the transient/permanent split.
    pub fn due_for_reset(&self, now: Millis) -> Result<Vec<AccountHealth>> {
        let guard = self.guard()?;
        let sql = format!(
            "SELECT {ACCOUNT_COLUMNS} FROM account_health \
             WHERE deleted_at IS NULL AND reset_at IS NOT NULL AND reset_at <= ?1 \
             ORDER BY account_id"
        );
        let mut stmt = guard.prepare(&sql)?;
        let rows = stmt.query_map(params![now.as_millis()], row_to_raw_account)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?.into_typed()?);
        }
        Ok(out)
    }

    // -- oauth credentials -------------------------------------------------------------

    /// Insert or update an OAuth credential record (last-writer-wins per record, `plan.md` §18
    /// invariant 7 — the losing value survives in the oplog) and append its oplog entry.
    ///
    /// `now` stamps `updated_at`/`created_at`; the record's own `updated_at` is informational,
    /// carried through so a caller can echo back what it last read. Returns the row's revision
    /// after the write.
    pub fn put_oauth_credential(
        &self,
        record: &OAuthCredentialRecord,
        now: Millis,
    ) -> Result<Revision> {
        let id = record.id.as_str();
        // Boundary check, not a suggestion: see `reject_credential_material`.
        reject_credential_material("vault_ref", &record.vault_ref, id)?;
        reject_credential_material("label", &record.label, id)?;
        let scopes = serde_json::to_string(&record.scopes)?;

        let mut guard = self.guard()?;
        let tx = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existed = tx
            .query_row(
                "SELECT revision FROM oauth_credential WHERE id = ?1",
                params![id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .is_some();
        let revision: i64 = tx.query_row(
            "INSERT INTO oauth_credential (id, platform, vault_ref, label, scopes, expires_at, \
             single_use, consumed_at, created_at, updated_at, revision, deleted_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, 1, NULL) \
             ON CONFLICT(id) DO UPDATE SET \
               platform = excluded.platform, vault_ref = excluded.vault_ref, \
               label = excluded.label, scopes = excluded.scopes, \
               expires_at = excluded.expires_at, single_use = excluded.single_use, \
               consumed_at = excluded.consumed_at, updated_at = excluded.updated_at, \
               revision = oauth_credential.revision + 1, deleted_at = excluded.deleted_at \
             RETURNING revision",
            params![
                id,
                record.platform,
                record.vault_ref,
                record.label,
                scopes,
                record.expires_at.map(Millis::as_millis),
                i64::from(record.single_use),
                record.consumed_at.map(Millis::as_millis),
                now.as_millis()
            ],
            |row| row.get(0),
        )?;
        let raw: RawOAuthCredential = tx.query_row(
            &format!("SELECT {OAUTH_COLUMNS} FROM oauth_credential WHERE id = ?1"),
            params![id],
            row_to_raw_oauth,
        )?;
        let typed = raw.into_typed()?;
        let payload = serde_json::to_vec(&typed)?;
        let revision = u64::try_from(revision).unwrap_or(0);
        append_oplog(
            &tx,
            "oauth_credential",
            id,
            revision,
            if existed { "update" } else { "insert" },
            now,
            &payload,
        )?;
        tx.commit()?;
        Ok(revision)
    }

    /// The live credential record; `None` if unknown or tombstoned.
    pub fn get_oauth_credential(
        &self,
        id: &OAuthCredentialId,
    ) -> Result<Option<OAuthCredentialRecord>> {
        self.fetch_oauth(id, false)
    }

    /// The credential record including tombstones, for sync and forensics.
    pub fn get_oauth_credential_including_deleted(
        &self,
        id: &OAuthCredentialId,
    ) -> Result<Option<OAuthCredentialRecord>> {
        self.fetch_oauth(id, true)
    }

    /// All credential records, optionally including tombstones, ordered by id.
    pub fn list_oauth_credentials(
        &self,
        include_deleted: bool,
    ) -> Result<Vec<OAuthCredentialRecord>> {
        let guard = self.guard()?;
        let sql = if include_deleted {
            format!("SELECT {OAUTH_COLUMNS} FROM oauth_credential ORDER BY id")
        } else {
            format!(
                "SELECT {OAUTH_COLUMNS} FROM oauth_credential WHERE deleted_at IS NULL \
                 ORDER BY id"
            )
        };
        let mut stmt = guard.prepare(&sql)?;
        let rows = stmt.query_map([], row_to_raw_oauth)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?.into_typed()?);
        }
        Ok(out)
    }

    /// Spend a single-use credential exactly once.
    ///
    /// The guard lives in the `WHERE` clause (`single_use = 1 AND consumed_at IS NULL`), so two
    /// racing device-code polls cannot both win: SQLite serialises the two updates, the first
    /// sets `consumed_at`, the second matches zero rows. Returns `Ok(true)` when *this* call
    /// consumed it and `Ok(false)` when someone else already did — the race's losing caller must
    /// be able to tell "already spent" from "never existed".
    pub fn consume_oauth_credential(&self, id: &OAuthCredentialId, now: Millis) -> Result<bool> {
        let mut guard = self.guard()?;
        let tx = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE oauth_credential SET consumed_at = ?2, updated_at = ?2, \
             revision = revision + 1 WHERE id = ?1 AND deleted_at IS NULL AND single_use = 1 \
             AND consumed_at IS NULL",
            params![id.as_str(), now.as_millis()],
        )?;
        if changed == 1 {
            let raw: RawOAuthCredential = tx.query_row(
                &format!("SELECT {OAUTH_COLUMNS} FROM oauth_credential WHERE id = ?1"),
                params![id.as_str()],
                row_to_raw_oauth,
            )?;
            let revision = u64::try_from(raw.revision).unwrap_or(0);
            let typed = raw.into_typed()?;
            let payload = serde_json::to_vec(&typed)?;
            append_oplog(
                &tx,
                "oauth_credential",
                id.as_str(),
                revision,
                "update",
                now,
                &payload,
            )?;
            tx.commit()?;
            return Ok(true);
        }
        let row: Option<(i64, Option<i64>, Option<i64>)> = tx
            .query_row(
                "SELECT single_use, consumed_at, deleted_at FROM oauth_credential WHERE id = ?1",
                params![id.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        match row {
            None | Some((_, _, Some(_))) => Err(Error::NotFound(format!("oauth credential {id}"))),
            // A caller consuming a credential that was never marked single-use is a programming
            // error, not a race; saying so beats returning `false` and letting the caller treat
            // a bug as "somebody else won".
            Some((0, _, _)) => Err(Error::InvalidState(format!(
                "oauth credential {id} is not marked single-use"
            ))),
            // The race: someone already spent it. Benign.
            Some((_, None, None)) | Some((_, Some(_), None)) => Ok(false),
        }
    }

    /// Tombstone an OAuth credential (never a hard delete — see the module docs).
    ///
    /// Returns `false` when there was nothing live to remove.
    pub fn delete_oauth_credential(&self, id: &OAuthCredentialId, now: Millis) -> Result<bool> {
        let mut guard = self.guard()?;
        let tx = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE oauth_credential SET deleted_at = ?2, updated_at = ?2, \
             revision = revision + 1 WHERE id = ?1 AND deleted_at IS NULL",
            params![id.as_str(), now.as_millis()],
        )?;
        if changed == 0 {
            return Ok(false);
        }
        let raw: RawOAuthCredential = tx.query_row(
            &format!("SELECT {OAUTH_COLUMNS} FROM oauth_credential WHERE id = ?1"),
            params![id.as_str()],
            row_to_raw_oauth,
        )?;
        let revision = u64::try_from(raw.revision).unwrap_or(0);
        let typed = raw.into_typed()?;
        let payload = serde_json::to_vec(&typed)?;
        append_oplog(
            &tx,
            "oauth_credential",
            id.as_str(),
            revision,
            "delete",
            now,
            &payload,
        )?;
        tx.commit()?;
        Ok(true)
    }

    // -- oplog -------------------------------------------------------------------------

    /// How many oplog entries no sync pass has marked as sent yet — the panel's "pending
    /// changes" counter, and the **only** thing v1 ever reads from the log. v2 replays it; see
    /// [`MIGRATION_V1`] for why writing it now is cheaper than backfilling it later.
    ///
    /// In v1 nothing marks rows synced, so the counter equals total writes — correct for a v1
    /// panel ("this many changes would sync") and monotonic rather than jumping to zero the
    /// first time sync lands.
    pub fn pending_changes(&self) -> Result<u64> {
        let guard = self.guard()?;
        let count: i64 = guard.query_row(
            "SELECT COUNT(*) FROM oplog WHERE synced_at IS NULL",
            [],
            |row| row.get(0),
        )?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    // -- internals ---------------------------------------------------------------------

    fn guard(&self) -> Result<MutexGuard<'_, Connection>> {
        // A poisoned mutex means some thread panicked while holding the connection. SQLite is
        // still consistent (the transaction unwound), but the invariants this module maintains
        // (revision bumps, oplog pairing) were mid-flight; refusing to serve is safer than
        // guessing what half-applied state looked like.
        self.conn
            .lock()
            .map_err(|_| Error::Store("store lock poisoned by a panicked writer".into()))
    }

    fn fetch_account(
        &self,
        id: &AccountId,
        include_deleted: bool,
    ) -> Result<Option<AccountHealth>> {
        let guard = self.guard()?;
        let sql = if include_deleted {
            format!("SELECT {ACCOUNT_COLUMNS} FROM account_health WHERE account_id = ?1")
        } else {
            format!(
                "SELECT {ACCOUNT_COLUMNS} FROM account_health \
                 WHERE account_id = ?1 AND deleted_at IS NULL"
            )
        };
        let raw: Option<RawAccountHealth> = guard
            .query_row(&sql, params![id.as_str()], row_to_raw_account)
            .optional()?;
        raw.map(RawAccountHealth::into_typed).transpose()
    }

    fn fetch_oauth(
        &self,
        id: &OAuthCredentialId,
        include_deleted: bool,
    ) -> Result<Option<OAuthCredentialRecord>> {
        let guard = self.guard()?;
        let sql = if include_deleted {
            format!("SELECT {OAUTH_COLUMNS} FROM oauth_credential WHERE id = ?1")
        } else {
            format!(
                "SELECT {OAUTH_COLUMNS} FROM oauth_credential \
                 WHERE id = ?1 AND deleted_at IS NULL"
            )
        };
        let raw: Option<RawOAuthCredential> = guard
            .query_row(&sql, params![id.as_str()], row_to_raw_oauth)
            .optional()?;
        raw.map(RawOAuthCredential::into_typed).transpose()
    }

    /// One write path for every account-health change: apply the change, bump the revision, read
    /// the row back, and append the oplog entry — all in one transaction, so a committed change
    /// can never be missing from the log.
    ///
    /// Any write also clears `deleted_at`: observed activity proves the account exists again,
    /// which is a legitimate last-writer-wins event, and the oplog records both the delete and
    /// the resurrection instead of one silently eating the other.
    fn write_account_change(
        &self,
        account_id: &AccountId,
        change: AccountChange,
        now: Millis,
    ) -> Result<AccountHealth> {
        let mut guard = self.guard()?;
        let tx = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<RawAccountHealth> = tx
            .query_row(
                &format!("SELECT {ACCOUNT_COLUMNS} FROM account_health WHERE account_id = ?1"),
                params![account_id.as_str()],
                row_to_raw_account,
            )
            .optional()?;

        let previous_failures = existing
            .as_ref()
            .map_or(0, |r| r.consecutive_failures.max(0));
        let created_at = existing.as_ref().map_or(now.as_millis(), |r| r.created_at);
        let existed = existing.is_some();

        let (state, reset_at, failures, kind_raw, reason_raw) = match change {
            AccountChange::Failure {
                kind,
                reason,
                cooldown_millis,
            } => {
                let state = match kind {
                    FailureKind::Transient => AccountState::Backoff,
                    FailureKind::Permanent => AccountState::AuthFailed,
                };
                let reset_at = match kind {
                    FailureKind::Transient => Some(now.plus_millis(cooldown_millis)),
                    // Permanent failures get no reset instant on purpose: `due_for_reset` will
                    // never surface this row, no matter how much time passes. That is the "not
                    // retried forever" property, expressed as data instead of a retry counter
                    // someone will eventually forget to honour.
                    FailureKind::Permanent => None,
                };
                (
                    state,
                    reset_at,
                    previous_failures.saturating_add(1),
                    Some(kind.as_str().to_string()),
                    Some(reason),
                )
            }
            AccountChange::Success => (AccountState::Ready, None, 0, None, None),
            AccountChange::SetState { state, reset_at } => {
                let kind = existing.as_ref().and_then(|r| r.last_failure_kind.clone());
                let reason = existing
                    .as_ref()
                    .and_then(|r| r.last_failure_reason.clone());
                (state, reset_at, previous_failures, kind, reason)
            }
        };
        let state_str = state.as_str();
        let reset_millis = reset_at.map(Millis::as_millis);

        let revision: i64 = if existed {
            tx.query_row(
                "UPDATE account_health SET state = ?2, reset_at = ?3, consecutive_failures = ?4, \
                 last_failure_kind = ?5, last_failure_reason = ?6, updated_at = ?7, \
                 deleted_at = NULL, revision = revision + 1 \
                 WHERE account_id = ?1 RETURNING revision",
                params![
                    account_id.as_str(),
                    state_str,
                    reset_millis,
                    failures,
                    kind_raw,
                    reason_raw,
                    now.as_millis()
                ],
                |row| row.get(0),
            )?
        } else {
            tx.query_row(
                "INSERT INTO account_health (account_id, state, reset_at, consecutive_failures, \
                 last_failure_kind, last_failure_reason, created_at, updated_at, revision, \
                 deleted_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, 1, NULL) \
                 RETURNING revision",
                params![
                    account_id.as_str(),
                    state_str,
                    reset_millis,
                    failures,
                    kind_raw,
                    reason_raw,
                    created_at
                ],
                |row| row.get(0),
            )?
        };

        let raw: RawAccountHealth = tx.query_row(
            &format!("SELECT {ACCOUNT_COLUMNS} FROM account_health WHERE account_id = ?1"),
            params![account_id.as_str()],
            row_to_raw_account,
        )?;
        let health = raw.into_typed()?;
        let payload = serde_json::to_vec(&health)?;
        append_oplog(
            &tx,
            "account_health",
            account_id.as_str(),
            u64::try_from(revision).unwrap_or(0),
            if existed { "update" } else { "insert" },
            now,
            &payload,
        )?;
        tx.commit()?;
        Ok(health)
    }

    /// Insert a batch of usage rows in one transaction.
    ///
    /// `ON CONFLICT DO NOTHING` on the id: the writer may retry a batch after an error, and a
    /// duplicate id that re-counted its tokens would corrupt every aggregate built on this table
    /// in a way no later query could detect.
    fn insert_usage_batch(&self, batch: &[UsageRecord]) -> Result<u64> {
        if batch.is_empty() {
            return Ok(0);
        }
        let mut guard = self.guard()?;
        let tx = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut written = 0u64;
        for record in batch {
            let losers = serde_json::to_string(&record.loser_steps)?;
            let now = Millis::now().as_millis();
            let n = tx.execute(
                "INSERT INTO usage (id, account_id, model, prompt_tokens, completion_tokens, \
                 duration_ms, winner_step, loser_steps, occurred_at, created_at, updated_at, \
                 revision, deleted_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10, 1, \
                 NULL) ON CONFLICT(id) DO NOTHING",
                params![
                    record.id,
                    record.account_id.as_str(),
                    record.model,
                    record.prompt_tokens,
                    record.completion_tokens,
                    record.duration_ms,
                    record.winner_step,
                    losers,
                    record.occurred_at.as_millis(),
                    now,
                ],
            )?;
            written += u64::try_from(n).unwrap_or(0);
        }
        tx.commit()?;
        Ok(written)
    }

    /// Aggregate grouped by one key column.
    ///
    /// `key_col` is a compile-time constant of this module (`account_id` or `model`), never
    /// caller input, so the `format!` here is not an injection point.
    fn usage_grouped(&self, key_col: &str, from: Millis, to: Millis) -> Result<Vec<UsageGroup>> {
        let guard = self.guard()?;
        let sql = format!(
            "SELECT {key_col}, {AGGREGATE_COLUMNS} FROM usage WHERE {OCCURRED_RANGE} \
             GROUP BY {key_col} ORDER BY {key_col}"
        );
        let mut stmt = guard.prepare(&sql)?;
        let rows = stmt.query_map(params![from.as_millis(), to.as_millis()], |row| {
            let key: String = row.get(0)?;
            let totals = aggregate_from(row, 1)?;
            Ok(UsageGroup { key, totals })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    // ---------------------------------------------------------------------------------------------
    // The per-key usage ledger
    // ---------------------------------------------------------------------------------------------

    /// Append one request's token spend to the per-key ledger.
    ///
    /// The row is written once and never updated: the ledger is the history that rolling windows
    /// are computed from, so a corrected row would change what a window meant after the fact.
    /// Re-recording the same id is a no-op (`ON CONFLICT DO NOTHING`), which makes a retry safe.
    pub fn record_key_usage(&self, record: &KeyUsageRecord) -> Result<()> {
        let guard = self.guard()?;
        guard.execute(
            "INSERT INTO key_usage (id, client_key_id, prompt_tokens, completion_tokens, \
             occurred_at, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(id) DO NOTHING",
            params![
                record.id,
                record.client_key_id,
                record.prompt_tokens,
                record.completion_tokens,
                record.occurred_at.as_millis(),
                record.created_at.as_millis(),
            ],
        )?;
        Ok(())
    }

    /// The usage of one key since `since`, in milliseconds since the epoch.
    ///
    /// The window is half-open at the far end only: everything from `since` onwards counts, which
    /// is what "the last five hours" means at the moment it is asked.
    pub fn key_window_usage(&self, client_key_id: &str, since: Millis) -> Result<KeyWindowUsage> {
        let guard = self.guard()?;
        let row = guard.query_row(
            "SELECT COUNT(*), COALESCE(SUM(prompt_tokens), 0), COALESCE(SUM(completion_tokens), 0) \
             FROM key_usage WHERE client_key_id = ?1 AND occurred_at >= ?2",
            params![client_key_id, since.as_millis()],
            |row| {
                Ok(KeyWindowUsage {
                    requests: row.get::<_, i64>(0)? as u64,
                    prompt_tokens: row.get::<_, i64>(1)? as u64,
                    completion_tokens: row.get::<_, i64>(2)? as u64,
                })
            },
        )?;
        Ok(row)
    }

    /// The usage of many keys since `since`, in one query.
    ///
    /// The map is **dense**: every requested key is present, with zeroes for the keys that have
    /// used nothing. A panel that reads five keys therefore does not need to know which of them
    /// had traffic, and it does not need one query per key.
    pub fn key_window_usage_many(
        &self,
        since: Millis,
        client_key_ids: &[String],
    ) -> Result<std::collections::HashMap<String, KeyWindowUsage>> {
        let mut out = std::collections::HashMap::with_capacity(client_key_ids.len());
        for key in client_key_ids {
            out.insert(key.clone(), KeyWindowUsage::default());
        }
        if client_key_ids.is_empty() {
            return Ok(out);
        }
        let guard = self.guard()?;
        let placeholders = vec!["?"; client_key_ids.len()].join(", ");
        let sql = format!(
            "SELECT client_key_id, COUNT(*), COALESCE(SUM(prompt_tokens), 0), \
             COALESCE(SUM(completion_tokens), 0) FROM key_usage \
             WHERE client_key_id IN ({placeholders}) AND occurred_at >= ? \
             GROUP BY client_key_id"
        );
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = client_key_ids
            .iter()
            .map(|k| Box::new(k.clone()) as Box<dyn rusqlite::ToSql>)
            .collect();
        params.push(Box::new(since.as_millis()));
        let mut stmt = guard.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| {
            let key: String = row.get(0)?;
            Ok((
                key,
                KeyWindowUsage {
                    requests: row.get::<_, i64>(1)? as u64,
                    prompt_tokens: row.get::<_, i64>(2)? as u64,
                    completion_tokens: row.get::<_, i64>(3)? as u64,
                },
            ))
        })?;
        for row in rows {
            let (key, usage) = row?;
            out.insert(key, usage);
        }
        Ok(out)
    }

    /// Delete ledger rows that happened before `before`, returning how many were removed.
    ///
    /// The ledger grows with traffic, so it needs a retention policy. Pruning is by occurrence
    /// time, not write time: a row written late about an old request is still old.
    pub fn prune_key_usage(&self, before: Millis) -> Result<u64> {
        let guard = self.guard()?;
        let removed = guard.execute(
            "DELETE FROM key_usage WHERE occurred_at < ?1",
            params![before.as_millis()],
        )?;
        Ok(u64::try_from(removed).unwrap_or(0))
    }

    // ---------------------------------------------------------------------------------------------
    // The provider quota cache
    // ---------------------------------------------------------------------------------------------

    /// Store the last known quota state of one provider account.
    ///
    /// A second fetch for the same `(provider, account)` updates the row in place and keeps the
    /// original `created_at`, so "first seen" and "last updated" stay distinct.
    pub fn put_provider_quota_state(&self, row: &ProviderQuotaState) -> Result<()> {
        let guard = self.guard()?;
        guard.execute(
            "INSERT INTO provider_quota_state (provider_id, account_id, fetched_at, source, \
             payload, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT(provider_id, account_id) DO UPDATE SET \
             fetched_at = excluded.fetched_at, source = excluded.source, \
             payload = excluded.payload, updated_at = excluded.updated_at",
            params![
                row.provider_id,
                row.account_id.as_str(),
                row.fetched_at.as_millis(),
                row.source,
                row.payload,
                row.created_at.as_millis(),
                row.updated_at.as_millis(),
            ],
        )?;
        Ok(())
    }

    /// The cached quota state of every account of one provider.
    ///
    /// An empty vector means "nothing cached", which is different from "cached and empty" and is
    /// reported as such: the caller must not treat a missing cache as a zero quota.
    pub fn provider_quota_state(&self, provider_id: &str) -> Result<Vec<ProviderQuotaState>> {
        let guard = self.guard()?;
        let mut stmt = guard.prepare(
            "SELECT provider_id, account_id, fetched_at, source, payload, created_at, updated_at \
             FROM provider_quota_state WHERE provider_id = ?1 ORDER BY account_id",
        )?;
        let rows = stmt.query_map(params![provider_id], |row| {
            Ok(ProviderQuotaState {
                provider_id: row.get(0)?,
                account_id: AccountId::from(row.get::<_, String>(1)?),
                fetched_at: Millis::from_millis(row.get(2)?),
                source: row.get(3)?,
                payload: row.get(4)?,
                created_at: Millis::from_millis(row.get(5)?),
                updated_at: Millis::from_millis(row.get(6)?),
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Every cached quota row, for every provider.
    pub fn all_provider_quota_state(&self) -> Result<Vec<ProviderQuotaState>> {
        let guard = self.guard()?;
        let mut stmt = guard.prepare(
            "SELECT provider_id, account_id, fetched_at, source, payload, created_at, updated_at \
             FROM provider_quota_state ORDER BY provider_id, account_id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ProviderQuotaState {
                provider_id: row.get(0)?,
                account_id: AccountId::from(row.get::<_, String>(1)?),
                fetched_at: Millis::from_millis(row.get(2)?),
                source: row.get(3)?,
                payload: row.get(4)?,
                created_at: Millis::from_millis(row.get(5)?),
                updated_at: Millis::from_millis(row.get(6)?),
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Forget the cached quota of one provider account.
    ///
    /// Returns whether a row was actually removed, so a caller can tell "deleted" from "was never
    /// there".
    pub fn delete_provider_quota_state(&self, provider_id: &str, account_id: &str) -> Result<bool> {
        let guard = self.guard()?;
        let removed = guard.execute(
            "DELETE FROM provider_quota_state WHERE provider_id = ?1 AND account_id = ?2",
            params![provider_id, account_id],
        )?;
        Ok(removed > 0)
    }

    // ---------------------------------------------------------------------------------------------
    // The per-usage token side table
    // ---------------------------------------------------------------------------------------------

    /// Attach the provider-reported token breakdown to a usage row.
    ///
    /// The breakdown is optional in every field: a provider that reports cached tokens but not a
    /// generation window leaves the other column `NULL` rather than inventing a zero. A `NULL`
    /// means "not reported"; a zero means "reported as zero".
    pub fn record_usage_tokens(
        &self,
        usage_id: &str,
        cached_tokens: Option<u64>,
        cache_write_tokens: Option<u64>,
        generation_ms: Option<u64>,
    ) -> Result<()> {
        let guard = self.guard()?;
        guard.execute(
            "INSERT INTO usage_tokens (usage_id, cached_tokens, cache_write_tokens, generation_ms, \
             created_at) VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(usage_id) DO UPDATE SET cached_tokens = excluded.cached_tokens, \
             cache_write_tokens = excluded.cache_write_tokens, generation_ms = excluded.generation_ms",
            params![
                usage_id,
                cached_tokens,
                cache_write_tokens,
                generation_ms,
                Millis::now().as_millis(),
            ],
        )?;
        Ok(())
    }

    /// The token breakdown of one usage row, if the provider reported one.
    pub fn usage_tokens(&self, usage_id: &str) -> Result<Option<UsageTokenBreakdown>> {
        let guard = self.guard()?;
        let mut stmt = guard.prepare(
            "SELECT cached_tokens, cache_write_tokens, generation_ms FROM usage_tokens \
             WHERE usage_id = ?1",
        )?;
        let mut rows = stmt.query_map(params![usage_id], |row| {
            Ok(UsageTokenBreakdown {
                cached_tokens: row.get::<_, Option<i64>>(0)?.map(|v| v as u64),
                cache_write_tokens: row.get::<_, Option<i64>>(1)?.map(|v| v as u64),
                generation_ms: row.get::<_, Option<i64>>(2)?.map(|v| v as u64),
            })
        })?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }
}

// -------------------------------------------------------------------------------------------------
// Schema and connection configuration
// -------------------------------------------------------------------------------------------------

/// The aggregate columns and the range predicate, shared by every usage query so the totals and
/// the grouped views can never disagree about what "in range" means.
const AGGREGATE_COLUMNS: &str = "COUNT(*), COALESCE(SUM(prompt_tokens), 0), \
     COALESCE(SUM(completion_tokens), 0), COALESCE(SUM(duration_ms), 0), \
     COALESCE(MAX(duration_ms), 0)";

/// Half-open range predicate over `occurred_at`, matching [`Store::usage_totals`]'s contract.
const OCCURRED_RANGE: &str = "occurred_at >= ?1 AND occurred_at < ?2";

/// Read one aggregate row starting at column `offset` (five columns: counts, sums, max).
///
/// Every value is read as nullable and folded to zero: `COUNT(*)` and the `COALESCE`d sums are
/// never actually null, but reading one type for all five keeps the grouped and ungrouped paths
/// byte-identical in what they accept — a `NULL` from an unexpected query shape degrades to a
/// zero sum instead of a conversion error that takes the whole panel down.
fn aggregate_from(row: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<UsageAggregate> {
    fn cell(row: &rusqlite::Row<'_>, idx: usize) -> rusqlite::Result<u64> {
        let value: Option<i64> = row.get(idx)?;
        Ok(value.map_or(0, |v| u64::try_from(v).unwrap_or(0)))
    }

    Ok(UsageAggregate {
        requests: cell(row, offset)?,
        prompt_tokens: cell(row, offset + 1)?,
        completion_tokens: cell(row, offset + 2)?,
        total_duration_ms: cell(row, offset + 3)?,
        max_duration_ms: cell(row, offset + 4)?,
    })
}

/// Connection-level setup that must happen before any query: lock policy, durability, foreign
/// keys.
///
/// `file_backed` distinguishes a real database from `:memory:` — WAL is a property of a journal
/// file, and asking an in-memory database for `journal_mode = WAL` silently returns `memory`,
/// which would make the verification below fail for a reason that has nothing to do with
/// correctness.
fn configure(conn: &mut Connection, file_backed: bool) -> Result<()> {
    // First: every later statement can hit a lock held by another process, and without this
    // they would fail instantly with `SQLITE_BUSY` instead of waiting.
    conn.busy_timeout(BUSY_TIMEOUT)?;
    if file_backed {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // `NORMAL` synchronous with WAL: a power loss can lose the last commits but cannot
        // corrupt the file, while `FULL` would fsync every batch and put the disk back on the
        // latency path this store is designed to keep off. What is traded away is at most the
        // most recent usage counts and cooldowns — bounded, re-derivable, non-corrupting; the
        // alternative spends real milliseconds per batch to protect values that are worth
        // seconds.
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let mode: String = conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            // Fail loudly rather than run degraded: in rollback-journal mode a reader blocks the
            // writer (and vice versa), so the busy timeout alone cannot prevent the "random lock
            // errors under load" this configuration exists to avoid.
            return Err(Error::Store(format!(
                "journal_mode is {mode:?} after requesting WAL; refusing to open the store in a \
                 mode where readers and writers block each other"
            )));
        }
    }
    conn.pragma_update(None, "foreign_keys", "ON")?;
    let foreign_keys: i64 = conn.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
    if foreign_keys != 1 {
        return Err(Error::Store(
            "foreign_keys could not be enabled on this connection".into(),
        ));
    }
    Ok(())
}

/// Bring a connection up to [`SCHEMA_VERSION`], or refuse to touch it.
///
/// The forward-safety rule in one sentence: **this build never applies a migration it does not
/// have.** A database written by a newer build may contain rows whose columns this build does
/// not know, rows this build's SQL would misinterpret, and triggerless data the older migration
/// set cannot reproduce — so opening it, "helpfully" running nothing and then writing v1-shaped
/// rows would corrupt state the newer build still needs (the config loader's counterpart to this
/// is `MAX_KNOWN_FORMAT_VERSION` in `config.rs`: load-but-never-save; here there is nothing to
/// downgrade, so we refuse outright).
fn migrate(conn: &mut Connection) -> Result<()> {
    let current: u32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if (current as usize) > MIGRATIONS.len() {
        return Err(Error::Store(format!(
            "database schema version {current} is newer than this build supports ({SCHEMA_VERSION}); \
             refusing to open it rather than run a migration set this build does not have — \
             upgrade the binary, or move the file aside to start fresh"
        )));
    }
    if (current as usize) == MIGRATIONS.len() {
        return Ok(());
    }

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    // See `set_user_version`: zeroing first makes the subsequent bump unconditional on disk.
    set_user_version(&tx, 0)?;
    let mut applied = current;
    for migration in &MIGRATIONS[(current as usize)..] {
        // Each migration and its version bump share one transaction: a crash mid-migration rolls
        // everything back to the last consistent version, so the runner never restarts halfway
        // through a schema change with a version number that says otherwise.
        tx.execute_batch(migration)?;
        applied += 1;
        set_user_version(&tx, applied)?;
    }
    tx.commit()?;
    Ok(())
}

/// Set `PRAGMA user_version` so the value is guaranteed to reach disk with this transaction.
///
/// SQLite only persists a version bump when it detects a real change against the value it last
/// read on this connection; a bare re-write of the same value can be skipped as a no-op and never
/// dirty the header page — and then a crash after "tables created" re-runs the migration on the
/// next open. Writing `0` first makes every subsequent bump an unconditional, detectable change,
/// so the version always commits atomically with the migration it names.
fn set_user_version(conn: &Connection, version: u32) -> Result<()> {
    conn.pragma_update(None, "user_version", 0u32)?;
    conn.pragma_update(None, "user_version", version)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // -- helpers -----------------------------------------------------------------------

    fn account(name: &str) -> AccountId {
        AccountId::from_raw(format!("acct-{name}"))
    }

    fn t(ms: i64) -> Millis {
        Millis::from_millis(ms)
    }

    // -- open, migrations, forward safety ----------------------------------------------

    #[test]
    fn open_creates_parent_directories_restrictively() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("state").join("state.sqlite");
        let store = Store::open(&path).expect("open");
        assert!(path.exists(), "database file should have been created");
        assert_eq!(store.path(), Some(path.as_path()));
        assert_eq!(store.schema_version().expect("version"), SCHEMA_VERSION);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path.parent().expect("parent"))
                .expect("metadata")
                .permissions()
                .mode();
            // The state directory sits next to the vault: owner-only, not group/world readable.
            assert_eq!(mode & 0o777, 0o700, "state directory must be 0700");
        }
    }

    #[test]
    fn open_in_memory_has_no_path_but_a_full_schema() {
        let store = Store::open_in_memory().expect("open");
        assert!(store.path().is_none());
        assert_eq!(store.schema_version().expect("version"), SCHEMA_VERSION);
        // The tables must actually exist, not just the version number.
        store
            .record_account_success(&account("probe"), t(1))
            .expect("schema usable");
        assert!(store
            .account_health(&account("probe"))
            .expect("read")
            .is_some());
    }

    #[test]
    fn migration_is_idempotent_across_reopens() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("state.sqlite");
        {
            let store = Store::open(&path).expect("first open");
            store
                .record_account_success(&account("keeper"), t(10))
                .expect("write");
        }
        // Reopening re-runs the migration runner end to end; a non-idempotent migration would
        // fail here or wipe the row written above.
        let store = Store::open(&path).expect("second open");
        assert_eq!(store.schema_version().expect("version"), SCHEMA_VERSION);
        assert!(store
            .account_health(&account("keeper"))
            .expect("read")
            .is_some());
    }

    #[test]
    fn raw_migration_sql_can_be_applied_twice() {
        // The idempotency the runner relies on lives in the SQL itself (`IF NOT EXISTS`): prove
        // it directly, not only through the version guard.
        let conn = Connection::open_in_memory().expect("memory");
        conn.execute_batch(MIGRATION_V1).expect("first apply");
        conn.execute_batch(MIGRATION_V1)
            .expect("second apply must not fail");
    }

    #[test]
    fn open_refuses_a_database_from_a_newer_build() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("state.sqlite");
        drop(Store::open(&path).expect("first open"));

        // Simulate a future build having migrated this file past what we know.
        {
            let conn = Connection::open(&path).expect("reopen raw");
            conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
                .expect("bump version");
            conn.close().expect("close");
        }

        let err = Store::open(&path).expect_err("must refuse a newer schema");
        let message = err.to_string();
        assert!(
            message.contains("newer than this build"),
            "error should name the version problem, got: {message}"
        );
        // …and nothing was "helpfully" applied on the way to that error.
        let store_version = {
            let conn = Connection::open(&path).expect("reopen raw");
            let v: u32 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .expect("v");
            conn.close().expect("close");
            v
        };
        assert_eq!(store_version, SCHEMA_VERSION + 1, "refusal must not write");
    }

    // -- the per-key usage ledger -------------------------------------------------------

    #[test]
    fn a_v1_database_upgrades_to_the_current_schema_and_keeps_its_rows() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("state.sqlite");
        // A database written by the first build: v1 only.
        {
            let conn = Connection::open(&path).expect("open raw");
            conn.execute_batch(MIGRATION_V1).expect("apply v1");
            conn.pragma_update(None, "user_version", 1).expect("v1");
            conn.execute(
                "INSERT INTO account_health (account_id, state, reset_at, consecutive_failures, \
                 last_failure_kind, last_failure_reason, created_at, updated_at, revision, \
                 deleted_at) VALUES ('acct-a', 'ready', NULL, 0, NULL, NULL, 1, 1, 1, NULL)",
                [],
            )
            .expect("insert");
            conn.close().expect("close");
        }

        let store = Store::open(&path).expect("open");
        assert_eq!(store.schema_version().expect("version"), SCHEMA_VERSION);
        // The v1 row survived the upgrade…
        let health = store
            .account_health_including_deleted(&account("a"))
            .expect("query")
            .expect("row");
        assert_eq!(health.account_id, account("a"));
        // …and the new tables exist.
        let usage = store.key_window_usage("key-a", t(0)).expect("query");
        assert_eq!(usage.requests, 0);
    }

    #[test]
    fn the_ledger_counts_requests_and_tokens_over_a_rolling_window() {
        let store = Store::open_in_memory().expect("open");
        // Two requests inside the window, one before it.
        for (at, prompt, completion) in [(1_000i64, 10u64, 5u64), (2_000, 20, 7), (500, 99, 99)] {
            let record =
                KeyUsageRecord::new("key-a", t(at)).with_tokens(Some(prompt), Some(completion));
            store.record_key_usage(&record).expect("record");
        }

        let window = store.key_window_usage("key-a", t(1_000)).expect("query");
        assert_eq!(
            window.requests, 2,
            "the row before the window must not count"
        );
        assert_eq!(window.prompt_tokens, 30);
        assert_eq!(window.completion_tokens, 12);
        assert_eq!(window.total_tokens(), 42);

        // A later window rolls forward: what was in the window falls out of it.
        let later = store.key_window_usage("key-a", t(1_500)).expect("query");
        assert_eq!(later.requests, 1);
        assert_eq!(later.prompt_tokens, 20);
    }

    #[test]
    fn the_window_of_many_keys_is_dense_and_one_query() {
        let store = Store::open_in_memory().expect("open");
        let record = KeyUsageRecord::new("key-a", t(10)).with_tokens(Some(4), Some(6));
        store.record_key_usage(&record).expect("record");

        let keys = vec!["key-a".to_owned(), "key-b".to_owned()];
        let windows = store.key_window_usage_many(t(0), &keys).expect("query");
        assert_eq!(windows.len(), 2, "every requested key must be present");
        assert_eq!(windows["key-a"].requests, 1);
        assert_eq!(windows["key-a"].total_tokens(), 10);
        assert_eq!(
            windows["key-b"],
            KeyWindowUsage::default(),
            "a key with no traffic reports zeroes, not a missing entry"
        );
    }

    #[test]
    fn pruning_the_ledger_removes_only_rows_older_than_the_cutoff() {
        let store = Store::open_in_memory().expect("open");
        for at in [100i64, 200, 300] {
            let record = KeyUsageRecord::new("key-a", t(at));
            store.record_key_usage(&record).expect("record");
        }
        let removed = store.prune_key_usage(t(250)).expect("prune");
        assert_eq!(removed, 2);
        let window = store.key_window_usage("key-a", t(0)).expect("query");
        assert_eq!(window.requests, 1);
    }

    #[test]
    fn recording_the_same_ledger_row_twice_does_not_double_count() {
        let store = Store::open_in_memory().expect("open");
        let record = KeyUsageRecord::new("key-a", t(10)).with_tokens(Some(3), Some(4));
        store.record_key_usage(&record).expect("first");
        store.record_key_usage(&record).expect("second");
        let window = store.key_window_usage("key-a", t(0)).expect("query");
        assert_eq!(window.requests, 1);
        assert_eq!(window.total_tokens(), 7);
    }

    // -- the provider quota cache -------------------------------------------------------

    #[test]
    fn quota_state_is_stored_per_account_and_replaced_on_a_later_fetch() {
        let store = Store::open_in_memory().expect("open");
        let first = ProviderQuotaState::new("openai", account("a"), "provider", "{\"w\":1}");
        store.put_provider_quota_state(&first).expect("put");

        let second = ProviderQuotaState::new("openai", account("a"), "provider", "{\"w\":2}");
        store.put_provider_quota_state(&second).expect("put again");

        let rows = store.provider_quota_state("openai").expect("query");
        assert_eq!(rows.len(), 1, "the same account must not gain a second row");
        assert_eq!(rows[0].payload, "{\"w\":2}", "the newer fetch wins");
        assert_eq!(
            rows[0].created_at, first.created_at,
            "the creation stamp is kept across updates"
        );
        assert!(rows[0].updated_at >= first.updated_at);
    }

    #[test]
    fn quota_state_can_be_read_for_every_provider_and_deleted() {
        let store = Store::open_in_memory().expect("open");
        store
            .put_provider_quota_state(&ProviderQuotaState::new(
                "openai",
                account("a"),
                "provider",
                "{}",
            ))
            .expect("put");
        store
            .put_provider_quota_state(&ProviderQuotaState::new(
                "anthropic",
                account("b"),
                "provider",
                "{}",
            ))
            .expect("put");

        assert_eq!(store.all_provider_quota_state().expect("query").len(), 2);
        assert!(store
            .delete_provider_quota_state("openai", account("a").as_str())
            .expect("delete"));
        assert!(
            !store
                .delete_provider_quota_state("openai", account("a").as_str())
                .expect("delete again"),
            "deleting a row that is not there reports false"
        );
        assert_eq!(store.all_provider_quota_state().expect("query").len(), 1);
    }

    // -- the token side table -----------------------------------------------------------

    #[test]
    fn token_breakdowns_round_trip_and_distinguish_unreported_from_zero() {
        let store = Store::open_in_memory().expect("open");
        assert!(
            store.usage_tokens("usage-1").expect("query").is_none(),
            "an unknown usage row has no breakdown"
        );
        store
            .record_usage_tokens("usage-1", Some(12), Some(0), None)
            .expect("record");
        let breakdown = store.usage_tokens("usage-1").expect("query").expect("row");
        assert_eq!(breakdown.cached_tokens, Some(12));
        assert_eq!(
            breakdown.cache_write_tokens,
            Some(0),
            "a reported zero is not a missing value"
        );
        assert_eq!(breakdown.generation_ms, None, "an unreported window stays unknown");
    }

    // -- account health -----------------------------------------------------------------

    #[test]
    fn transient_failure_sets_backoff_with_a_reset_instant() {
        let store = Store::open_in_memory().expect("open");
        let now = t(1_000_000);
        let health = store
            .record_account_failure(
                &account("a"),
                FailureKind::Transient,
                "503 service unavailable",
                Duration::from_secs(60),
                now,
            )
            .expect("record");
        assert_eq!(health.state, AccountState::Backoff);
        assert_eq!(health.reset_at, Some(now.plus_millis(60_000)));
        assert_eq!(health.consecutive_failures, 1);
        assert_eq!(health.last_failure_kind, Some(FailureKind::Transient));
        assert_eq!(
            health.last_failure_reason.as_deref(),
            Some("503 service unavailable")
        );
        assert_eq!(health.revision, 1);
        // Not yet due, due exactly at the reset, due after it.
        assert!(!health.retry_due(now));
        assert!(health.retry_due(now.plus_millis(60_000)));
        assert!(health.retry_due(now.plus_millis(60_001)));
    }

    #[test]
    fn permanent_failure_has_no_reset_and_is_never_retried() {
        let store = Store::open_in_memory().expect("open");
        let now = t(2_000_000);
        let health = store
            .record_account_failure(
                &account("dead"),
                FailureKind::Permanent,
                "401 token revoked",
                Duration::from_secs(60),
                now,
            )
            .expect("record");
        assert_eq!(health.state, AccountState::AuthFailed);
        assert_eq!(
            health.reset_at, None,
            "a permanent failure must not arm a retry"
        );
        assert_eq!(health.consecutive_failures, 1);
        assert_eq!(health.last_failure_kind, Some(FailureKind::Permanent));

        // The heart of "not retried forever": no matter how far the clock advances, the account
        // never becomes due, and `due_for_reset` never surfaces it.
        let later = now.plus_millis(10 * 365 * 24 * 3_600_000);
        assert!(!health.retry_due(later));
        let due = store.due_for_reset(later).expect("query");
        assert!(
            due.iter().all(|h| h.account_id != account("dead")),
            "permanently failed accounts must never be auto-retried"
        );
    }

    #[test]
    fn failure_streak_accumulates_and_success_clears_it() {
        let store = Store::open_in_memory().expect("open");
        let mut now = t(1_000);
        for i in 1..=3 {
            let health = store
                .record_account_failure(
                    &account("flaky"),
                    FailureKind::Transient,
                    format!("attempt {i} timed out"),
                    Duration::from_secs(10),
                    now,
                )
                .expect("record");
            assert_eq!(health.consecutive_failures, i);
            now = now.plus_millis(11_000);
        }
        let ok = store
            .record_account_success(&account("flaky"), now)
            .expect("success");
        assert_eq!(ok.state, AccountState::Ready);
        assert_eq!(ok.consecutive_failures, 0);
        assert_eq!(ok.last_failure_kind, None);
        assert_eq!(ok.last_failure_reason, None);
        assert_eq!(ok.reset_at, None);
        assert_eq!(ok.revision, 4, "every write bumps the revision");
    }

    #[test]
    fn success_after_a_permanent_failure_rearms_the_account() {
        // The escape hatch for "permanent": a human fixes the grant, the next successful call
        // flips the row back to ready — no tombstone, no restart, no manual SQL.
        let store = Store::open_in_memory().expect("open");
        let now = t(5_000);
        store
            .record_account_failure(
                &account("a"),
                FailureKind::Permanent,
                "401",
                Duration::from_secs(1),
                now,
            )
            .expect("fail");
        let health = store
            .record_account_success(&account("a"), now.plus_millis(1))
            .expect("success");
        assert_eq!(health.state, AccountState::Ready);
        assert_eq!(health.reset_at, None, "a success arms no cooldown");
        assert_eq!(health.consecutive_failures, 0);
        assert_eq!(
            health.last_failure_kind, None,
            "the permanent classification is cleared"
        );
        assert!(
            !health.retry_due(now.plus_millis(1)),
            "ready is not 'due', it is usable"
        );
    }

    #[test]
    fn explicit_state_change_keeps_failure_history() {
        let store = Store::open_in_memory().expect("open");
        let now = t(9_000);
        store
            .record_account_failure(
                &account("a"),
                FailureKind::Transient,
                "429",
                Duration::from_secs(30),
                now,
            )
            .expect("fail");
        let health = store
            .set_account_state(
                &account("a"),
                AccountState::Disabled,
                None,
                now.plus_millis(1),
            )
            .expect("set state");
        assert_eq!(health.state, AccountState::Disabled);
        assert_eq!(
            health.consecutive_failures, 1,
            "operator actions do not clear the streak"
        );
        assert_eq!(health.last_failure_kind, Some(FailureKind::Transient));
        assert_eq!(health.last_failure_reason.as_deref(), Some("429"));
        assert_eq!(health.revision, 2);
    }

    #[test]
    fn account_health_roundtrips_and_unknown_accounts_are_none() {
        let store = Store::open_in_memory().expect("open");
        assert!(store
            .account_health(&account("ghost"))
            .expect("read")
            .is_none());
        store
            .record_account_success(&account("real"), t(42))
            .expect("write");
        let health = store
            .account_health(&account("real"))
            .expect("read")
            .expect("row");
        assert_eq!(health.account_id, account("real"));
        assert_eq!(health.state, AccountState::Ready);
        assert_eq!(health.created_at, t(42));
        assert_eq!(health.deleted_at, None);
        assert_eq!(health.revision, 1);
    }

    #[test]
    fn revisions_are_strictly_monotonic_across_every_write_kind() {
        let store = Store::open_in_memory().expect("open");
        let mut last = 0u64;
        let bump = |health: &AccountHealth, last: &mut u64| {
            assert!(
                health.revision > *last,
                "revision must strictly increase: {} then {}",
                health.revision,
                *last
            );
            *last = health.revision;
        };
        let now = t(1);
        let h = store
            .record_account_failure(
                &account("r"),
                FailureKind::Transient,
                "500",
                Duration::from_secs(1),
                now,
            )
            .expect("w1");
        bump(&h, &mut last);
        let h = store
            .record_account_success(&account("r"), now.plus_millis(1))
            .expect("w2");
        bump(&h, &mut last);
        let h = store
            .set_account_state(
                &account("r"),
                AccountState::RateLimited,
                Some(now.plus_millis(5)),
                now.plus_millis(2),
            )
            .expect("w3");
        bump(&h, &mut last);
        assert_eq!(last, 3);
        // The persisted row agrees with what the write path reported.
        let stored = store
            .account_health(&account("r"))
            .expect("read")
            .expect("row");
        assert_eq!(stored.revision, last);
    }

    #[test]
    fn deletion_leaves_a_tombstone_that_survives_queries() {
        let store = Store::open_in_memory().expect("open");
        let now = t(7_000);
        store
            .record_account_success(&account("gone"), now)
            .expect("create");
        assert!(store
            .delete_account_health(&account("gone"), now.plus_millis(1))
            .expect("delete"));

        // The live view treats it as absent — routing must never see a tombstone as usable.
        assert!(store
            .account_health(&account("gone"))
            .expect("live read")
            .is_none());
        assert!(store
            .list_account_health(false)
            .expect("live list")
            .is_empty());

        // But the row is still there, queryable, with its tombstone timestamp: this is what a
        // sync peer reads to learn "removed at T" rather than "never existed".
        let tomb = store
            .account_health_including_deleted(&account("gone"))
            .expect("tombstone read")
            .expect("row survives as a tombstone");
        assert_eq!(tomb.deleted_at, Some(now.plus_millis(1)));
        assert_eq!(tomb.revision, 2, "the delete itself bumps the revision");
        assert_eq!(store.list_account_health(true).expect("all list").len(), 1);
    }

    #[test]
    fn deletion_is_idempotent_and_resurrection_bumps_the_revision() {
        let store = Store::open_in_memory().expect("open");
        let now = t(8_000);
        store
            .record_account_success(&account("x"), now)
            .expect("create");
        assert!(store
            .delete_account_health(&account("x"), now.plus_millis(1))
            .expect("first delete"));
        assert!(!store
            .delete_account_health(&account("x"), now.plus_millis(2))
            .expect("second delete"));

        // A later write observes the account again: resurrect, and keep counting revisions so
        // the oplog order matches the real order of events.
        let health = store
            .record_account_success(&account("x"), now.plus_millis(3))
            .expect("resurrect");
        assert_eq!(health.deleted_at, None);
        assert_eq!(health.revision, 3);
        assert!(store
            .account_health(&account("x"))
            .expect("live read")
            .is_some());
    }

    #[test]
    fn due_for_reset_lists_only_elapsed_transient_cooldowns() {
        let store = Store::open_in_memory().expect("open");
        let now = t(10_000);
        store
            .record_account_failure(
                &account("soon"),
                FailureKind::Transient,
                "429",
                Duration::from_secs(60),
                now,
            )
            .expect("soon");
        store
            .record_account_failure(
                &account("later"),
                FailureKind::Transient,
                "429",
                Duration::from_secs(3_600),
                now,
            )
            .expect("later");
        store
            .record_account_failure(
                &account("never"),
                FailureKind::Permanent,
                "401",
                Duration::from_secs(60),
                now,
            )
            .expect("never");

        assert!(
            store.due_for_reset(now).expect("due").is_empty(),
            "nothing is due immediately"
        );
        let due = store.due_for_reset(now.plus_millis(60_000)).expect("due");
        let ids: Vec<String> = due.iter().map(|h| h.account_id.to_string()).collect();
        assert_eq!(ids, vec!["acct-soon".to_string()]);
        let due = store
            .due_for_reset(now.plus_millis(3_600_000))
            .expect("due");
        let mut ids: Vec<String> = due.iter().map(|h| h.account_id.to_string()).collect();
        ids.sort();
        assert_eq!(ids, vec!["acct-later".to_string(), "acct-soon".to_string()]);
        // The permanent failure is absent at every horizon, including the largest.
        assert!(due.iter().all(|h| h.account_id != account("never")));
    }

    // -- oplog -------------------------------------------------------------------------

    #[test]
    fn every_sync_relevant_write_appends_exactly_one_oplog_entry() {
        let store = Store::open_in_memory().expect("open");
        let now = t(100);
        assert_eq!(store.pending_changes().expect("count"), 0);
        store
            .record_account_success(&account("o"), now)
            .expect("w1");
        assert_eq!(store.pending_changes().expect("count"), 1);
        store
            .record_account_failure(
                &account("o"),
                FailureKind::Transient,
                "500",
                Duration::from_secs(1),
                now.plus_millis(1),
            )
            .expect("w2");
        store
            .delete_account_health(&account("o"), now.plus_millis(2))
            .expect("w3");
        assert_eq!(store.pending_changes().expect("count"), 3);

        // Usage is instance-local telemetry, not sync state: recording it must not grow the log.
        store
            .record_usage(&UsageRecord::new(account("o"), "gpt-x", 12))
            .expect("usage");
        assert_eq!(
            store.pending_changes().expect("count"),
            3,
            "usage rows are not oplogged"
        );
    }

    #[test]
    fn payload_hash_is_stable_and_content_sensitive() {
        // Two instances must agree on "the same write" without exchanging the value itself.
        let a = payload_hash(b"same bytes");
        let b = payload_hash(b"same bytes");
        let c = payload_hash(b"other bytes");
        assert_eq!(a, b, "hash must be deterministic");
        assert_ne!(a, c, "different content must hash differently");
        assert_eq!(a.len(), 64, "full SHA-256, hex-encoded");
        assert!(a.chars().all(|ch| ch.is_ascii_hexdigit()));
    }

    // -- usage -------------------------------------------------------------------------

    #[test]
    fn usage_roundtrips_every_field_including_hedge_participants() {
        let store = Store::open_in_memory().expect("open");
        let record = UsageRecord::new(account("u"), "gpt-x", 812)
            .with_tokens(Some(1_000), Some(500))
            .with_hedge(Some(2), vec![0, 1])
            .at(t(50_000));
        store.record_usage(&record).expect("write");

        let rows = store.usage_records(t(50_000), t(50_001), 10).expect("read");
        assert_eq!(rows.len(), 1);
        let got = &rows[0];
        assert_eq!(got.id, record.id);
        assert_eq!(got.account_id, account("u"));
        assert_eq!(got.model, "gpt-x");
        assert_eq!(got.prompt_tokens, Some(1_000));
        assert_eq!(got.completion_tokens, Some(500));
        assert_eq!(got.duration_ms, 812);
        assert_eq!(
            got.winner_step,
            Some(2),
            "the winning hedge participant must persist"
        );
        assert_eq!(
            got.loser_steps,
            vec![0, 1],
            "the losing participants must persist"
        );
        assert_eq!(got.occurred_at, t(50_000));
    }

    #[test]
    fn usage_without_reported_tokens_stays_unknown_not_zero() {
        // A streamed response may report no usage at all: the row must distinguish "no number
        // came in" from "zero tokens".
        let store = Store::open_in_memory().expect("open");
        store
            .record_usage(&UsageRecord::new(account("u"), "m", 10).at(t(1)))
            .expect("write");
        let rows = store.usage_records(t(0), t(2), 10).expect("read");
        assert_eq!(rows[0].prompt_tokens, None);
        assert_eq!(rows[0].completion_tokens, None);
        // Aggregates sum what was reported; the unknown contributes nothing to the sums but
        // still counts as a request.
        let totals = store.usage_totals(t(0), t(2)).expect("totals");
        assert_eq!(totals.requests, 1);
        assert_eq!(totals.prompt_tokens, 0);
    }

    #[test]
    fn aggregates_add_up_across_accounts_and_models() {
        let store = Store::open_in_memory().expect("open");
        let base = t(1_000_000);
        // Two accounts, two models, overlapping: the arithmetic has to hold per slice and whole.
        let rows = [
            (account("a"), "model-1", 10u64, 100u64, 50u64),
            (account("a"), "model-2", 20, 200, 60),
            (account("b"), "model-1", 30, 300, 70),
            (account("b"), "model-2", 40, 400, 80),
        ];
        for (i, (acct, model, prompt, completion, ms)) in rows.into_iter().enumerate() {
            let at = base.plus_millis(i64::try_from(i).unwrap_or(0) * 10);
            store
                .record_usage(
                    &UsageRecord::new(acct, model, ms)
                        .with_tokens(Some(prompt), Some(completion))
                        .at(at),
                )
                .expect("write");
        }

        let all = store
            .usage_totals(base, base.plus_millis(1_000))
            .expect("all");
        assert_eq!(all.requests, 4);
        // prompt below is 10+20+30+40 = 100; it is *completion* (100+200+300+400) that sums
        // to 1000. Asserting 1000 here transposed the two columns and read as a defect in
        // `usage_totals` itself, which sums correctly.
        assert_eq!(all.prompt_tokens, 100);
        assert_eq!(all.completion_tokens, 1_000);
        assert_eq!(all.total_duration_ms, 260);
        assert_eq!(all.max_duration_ms, 80);

        let by_account = store
            .usage_by_account(base, base.plus_millis(1_000))
            .expect("by account");
        assert_eq!(by_account.len(), 2);
        assert_eq!(by_account[0].key, "acct-a");
        assert_eq!(by_account[0].totals.requests, 2);
        assert_eq!(by_account[0].totals.prompt_tokens, 30);
        assert_eq!(by_account[0].totals.completion_tokens, 300);
        assert_eq!(by_account[1].key, "acct-b");
        assert_eq!(by_account[1].totals.prompt_tokens, 70);
        assert_eq!(by_account[1].totals.completion_tokens, 700);

        let by_model = store
            .usage_by_model(base, base.plus_millis(1_000))
            .expect("by model");
        assert_eq!(by_model.len(), 2);
        assert_eq!(by_model[0].key, "model-1");
        assert_eq!(by_model[0].totals.requests, 2);
        assert_eq!(by_model[0].totals.prompt_tokens, 40);
        assert_eq!(by_model[1].key, "model-2");
        assert_eq!(by_model[1].totals.prompt_tokens, 60);

        // The grouped views must partition the totals exactly — if either disagreed with the
        // whole, the panel would show numbers that do not add up.
        let grouped_sum: u64 = by_account.iter().map(|g| g.totals.requests).sum();
        assert_eq!(grouped_sum, all.requests);
        let grouped_tokens: u64 = by_model.iter().map(|g| g.totals.prompt_tokens).sum();
        assert_eq!(grouped_tokens, all.prompt_tokens);
    }

    #[test]
    fn usage_ranges_are_half_open() {
        let store = Store::open_in_memory().expect("open");
        // Records exactly on both boundaries: `[from, to)` must include the first and exclude
        // the second, so adjacent windows never double-count an instant.
        for at in [999_i64, 1_000, 1_999, 2_000] {
            store
                .record_usage(&UsageRecord::new(account("a"), "m", 1).at(t(at)))
                .expect("write");
        }
        let in_range = store.usage_totals(t(1_000), t(2_000)).expect("range");
        assert_eq!(
            in_range.requests, 2,
            "[from, to) includes from and excludes to"
        );
        // A closed window would report 3; verify that reading the neighbours agrees.
        assert_eq!(
            store.usage_totals(t(0), t(1_000)).expect("left").requests,
            1
        );
        assert_eq!(
            store
                .usage_totals(t(2_000), t(3_000))
                .expect("right")
                .requests,
            1
        );
    }

    #[test]
    fn usage_records_are_limited_and_newest_first() {
        let store = Store::open_in_memory().expect("open");
        for i in 0..5 {
            store
                .record_usage(&UsageRecord::new(account("a"), "m", 1).at(t(i * 10)))
                .expect("write");
        }
        let rows = store.usage_records(t(0), t(1_000), 3).expect("read");
        assert_eq!(rows.len(), 3);
        let times: Vec<i64> = rows.iter().map(|r| r.occurred_at.as_millis()).collect();
        assert_eq!(times, vec![40, 30, 20]);
    }

    #[test]
    fn duplicate_usage_ids_do_not_double_count() {
        // The writer may retry a batch after an error; a replayed id must not inflate quota.
        let store = Store::open_in_memory().expect("open");
        let record = UsageRecord::new(account("a"), "m", 5)
            .with_tokens(Some(10), Some(20))
            .at(t(1));
        store.record_usage(&record).expect("first");
        store.record_usage(&record).expect("replay");
        let totals = store.usage_totals(t(0), t(2)).expect("totals");
        assert_eq!(totals.requests, 1);
        assert_eq!(totals.prompt_tokens, 10);
    }

    // -- oauth credentials -------------------------------------------------------------

    #[test]
    fn oauth_credential_roundtrips_all_fields() {
        let store = Store::open_in_memory().expect("open");
        let now = t(60_000);
        let record = OAuthCredentialRecord::new("copilot", "provider/p/account/abc")
            .with_label("octo@example.com")
            .with_scopes(vec!["repo:read".into(), "org".into()])
            .with_expiry(t(90_000))
            .single_use();
        let revision = store.put_oauth_credential(&record, now).expect("put");
        assert_eq!(revision, 1);

        let got = store
            .get_oauth_credential(&record.id)
            .expect("read")
            .expect("row");
        assert_eq!(got.id, record.id);
        assert_eq!(got.platform, "copilot");
        assert_eq!(got.vault_ref, "provider/p/account/abc");
        assert_eq!(got.label, "octo@example.com");
        assert_eq!(got.scopes, vec!["repo:read".to_string(), "org".to_string()]);
        assert_eq!(got.expires_at, Some(t(90_000)));
        assert!(got.single_use);
        assert_eq!(got.consumed_at, None);
        assert_eq!(got.revision, 1);
        assert_eq!(got.created_at, now, "the store stamps its own clock");
        assert_eq!(got.deleted_at, None);
        assert!(store
            .get_oauth_credential(&OAuthCredentialId::generate())
            .expect("unknown")
            .is_none());
    }

    #[test]
    fn oauth_put_rejects_token_shaped_values_at_the_boundary() {
        // docs/21-security.md §3: the database stores references, never secrets. The realistic
        // mistake is pasting the token into the ref column "just for now".
        let store = Store::open_in_memory().expect("open");
        let token = format!("sk-{}", "A".repeat(48));
        let record = OAuthCredentialRecord::new("codex", token.clone());
        let err = store
            .put_oauth_credential(&record, t(1))
            .expect_err("token-shaped vault_ref must be refused");
        assert!(err.to_string().contains("looks like a token"), "got: {err}");
        assert!(
            store.list_oauth_credentials(true).expect("list").is_empty(),
            "nothing must land"
        );
        // Same guard on the human-editable label, where a paste is even more likely.
        let record = OAuthCredentialRecord::new("codex", "provider/p/account/x").with_label(token);
        assert!(store.put_oauth_credential(&record, t(1)).is_err());
    }

    #[test]
    fn oauth_put_bumps_revisions_and_oplog_on_every_write() {
        let store = Store::open_in_memory().expect("open");
        let now = t(10);
        let record = OAuthCredentialRecord::new("codex", "provider/p/account/x");
        assert_eq!(store.pending_changes().expect("count"), 0);
        let rev = store.put_oauth_credential(&record, now).expect("insert");
        assert_eq!(rev, 1);
        assert_eq!(store.pending_changes().expect("count"), 1);
        let rev = store
            .put_oauth_credential(&record, now.plus_millis(1))
            .expect("update");
        assert_eq!(rev, 2, "an update must bump, not overwrite, the revision");
        assert_eq!(store.pending_changes().expect("count"), 2);
        let got = store
            .get_oauth_credential(&record.id)
            .expect("read")
            .expect("row");
        assert_eq!(got.revision, 2);
    }

    #[test]
    fn oauth_delete_tombstones_rather_than_removing() {
        let store = Store::open_in_memory().expect("open");
        let now = t(20);
        let record = OAuthCredentialRecord::new("copilot", "provider/p/account/y");
        store.put_oauth_credential(&record, now).expect("put");

        assert!(store
            .delete_oauth_credential(&record.id, now.plus_millis(1))
            .expect("delete"));
        assert!(!store
            .delete_oauth_credential(&record.id, now.plus_millis(2))
            .expect("idempotent"));

        assert!(store
            .get_oauth_credential(&record.id)
            .expect("live read")
            .is_none());
        assert_eq!(
            store
                .list_oauth_credentials(false)
                .expect("live list")
                .len(),
            0
        );

        let tomb = store
            .get_oauth_credential_including_deleted(&record.id)
            .expect("tombstone read")
            .expect("row survives");
        assert_eq!(tomb.deleted_at, Some(now.plus_millis(1)));
        assert_eq!(
            tomb.revision, 2,
            "delete bumped the revision, the row was not dropped"
        );
        assert_eq!(
            store.list_oauth_credentials(true).expect("all list").len(),
            1
        );
        assert_eq!(
            store.pending_changes().expect("count"),
            2,
            "put + delete are both logged"
        );
    }

    #[test]
    fn single_use_credentials_can_be_spent_exactly_once() {
        let store = Store::open_in_memory().expect("open");
        let now = t(30);
        let record = OAuthCredentialRecord::new("codex", "provider/p/account/z").single_use();
        store.put_oauth_credential(&record, now).expect("put");

        // Two racing device-code polls: only the first may take the credential.
        assert!(store
            .consume_oauth_credential(&record.id, now.plus_millis(1))
            .expect("first"));
        assert!(!store
            .consume_oauth_credential(&record.id, now.plus_millis(2))
            .expect("second"));
        let got = store
            .get_oauth_credential(&record.id)
            .expect("read")
            .expect("row");
        assert_eq!(got.consumed_at, Some(now.plus_millis(1)));

        // An unknown credential and a non-single-use one are different failures from "already
        // spent", and callers need to tell them apart.
        assert!(matches!(
            store.consume_oauth_credential(&OAuthCredentialId::generate(), now),
            Err(Error::NotFound(_))
        ));
        let reusable = OAuthCredentialRecord::new("codex", "provider/p/account/r");
        store.put_oauth_credential(&reusable, now).expect("put");
        assert!(matches!(
            store.consume_oauth_credential(&reusable.id, now),
            Err(Error::InvalidState(_))
        ));
    }

    #[test]
    fn oauth_put_resurrects_a_tombstoned_record() {
        // Last-writer-wins in both directions: a credential re-added after deletion must come
        // back live, with the delete still visible in the oplog.
        let store = Store::open_in_memory().expect("open");
        let now = t(40);
        let record = OAuthCredentialRecord::new("codex", "provider/p/account/w");
        store.put_oauth_credential(&record, now).expect("put");
        store
            .delete_oauth_credential(&record.id, now.plus_millis(1))
            .expect("delete");
        let rev = store
            .put_oauth_credential(&record, now.plus_millis(2))
            .expect("re-put");
        assert_eq!(rev, 3);
        let got = store
            .get_oauth_credential(&record.id)
            .expect("read")
            .expect("live again");
        assert_eq!(got.deleted_at, None);
        assert_eq!(got.revision, 3);
        assert_eq!(store.pending_changes().expect("count"), 3);
    }

    // -- bounded usage channel ---------------------------------------------------------

    #[test]
    fn usage_channel_drops_under_saturation_and_counts_every_drop() {
        let store = Store::open_in_memory().expect("open");
        // No writer is draining, so the queue saturates at its capacity and the rest must be
        // dropped — visibly.
        let (channel, queue) = store.usage_queue(2);
        assert_eq!(channel.capacity(), 2);

        let mut accepted = 0u64;
        let mut refused = 0u64;
        for i in 0..5 {
            let ok = channel.try_record(UsageRecord::new(
                account("sat"),
                "m",
                u64::try_from(i).unwrap_or(0),
            ));
            if ok {
                accepted += 1;
            } else {
                refused += 1;
            }
        }
        assert_eq!(accepted, 2, "the queue holds exactly its capacity");
        assert_eq!(refused, 3, "overflow is dropped, not queued unboundedly");
        assert_eq!(channel.dropped(), 3, "every drop must be counted");
        assert_eq!(channel.accepted(), 2);
        assert_eq!(channel.pending(), 2);
        assert_eq!(queue.stats().dropped, 3, "both ends see the same counters");

        // Draining persists what survived; the drops stay dropped and still counted.
        let written = queue.drain_now().expect("drain");
        assert_eq!(written, 2);
        assert_eq!(queue.stats().written, 2);
        assert_eq!(
            queue.stats().dropped,
            3,
            "draining must not erase the drop history"
        );
        assert_eq!(
            queue.stats().pending(),
            0,
            "accepted = written + failed + pending"
        );
        assert_eq!(
            store
                .usage_totals(t(0), Millis::now().plus_millis(1))
                .expect("totals")
                .requests,
            2
        );
        assert!(queue.is_empty());
        assert_eq!(queue.len(), 0);
    }

    #[test]
    fn usage_channel_counts_drops_when_the_writer_is_gone() {
        // A shutdown race (writer dropped before the last request finishes) is the other drop
        // mode: still a drop, still counted, never a block.
        let store = Store::open_in_memory().expect("open");
        let (channel, queue) = store.usage_queue(4);
        drop(queue);
        assert!(!channel.try_record(UsageRecord::new(account("a"), "m", 1)));
        assert_eq!(
            channel.dropped(),
            1,
            "a disconnected writer must not eat records silently"
        );
        assert_eq!(channel.accepted(), 0);
    }

    #[test]
    fn usage_queue_capacity_is_clamped_to_a_usable_minimum() {
        let store = Store::open_in_memory().expect("open");
        // Capacity 0 would drop every record while reporting a perfectly healthy channel.
        let (channel, queue) = store.usage_queue(0);
        assert_eq!(channel.capacity(), 1);
        assert!(channel.try_record(UsageRecord::new(account("a"), "m", 1)));
        drop(channel);
        let written = queue.drain_now().expect("drain");
        assert_eq!(written, 1);
    }

    #[test]
    fn spawned_writer_drains_batches_and_joins_with_final_stats() {
        let store = Store::open_in_memory().expect("open");
        let (channel, queue) = store.usage_queue(16);
        let writer = queue.spawn().expect("spawn");

        for i in 0..4 {
            assert!(channel.try_record(
                UsageRecord::new(account("bg"), "m", 10)
                    .with_tokens(Some(u64::try_from(i).unwrap_or(0)), None)
                    .at(t(i * 1_000))
            ));
        }
        // Order matters for shutdown: drop every sender, then join — the thread exits once the
        // queue behind the last record has drained.
        drop(channel);
        let stats = writer.join().expect("join");
        assert_eq!(stats.accepted, 4);
        assert_eq!(stats.written, 4);
        assert_eq!(stats.failed, 0);
        assert_eq!(stats.dropped, 0);
        assert_eq!(stats.pending(), 0);
        assert_eq!(
            store
                .usage_totals(t(0), t(10_000))
                .expect("totals")
                .requests,
            4,
            "the background thread must have persisted everything before joining"
        );
    }

    // -- security posture ---------------------------------------------------------------

    #[test]
    fn raw_database_bytes_carry_no_credential_material() {
        // docs/21-security.md §3 makes this a testable claim about the *file*, not about the
        // types: a leaked state.sqlite (plus its WAL sidecars) must contain no token.
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("state.sqlite");
        let token = format!("ghp_{}", "B".repeat(40));
        {
            let store = Store::open(&path).expect("open");
            store
                .record_account_success(&account("acct"), t(1))
                .expect("health row");
            store
                .record_usage(
                    &UsageRecord::new(account("acct"), "gpt-x", 10)
                        .with_tokens(Some(100), Some(50)),
                )
                .expect("usage row");
            let record = OAuthCredentialRecord::new("copilot", "provider/p/account/1")
                .with_label("user@example.com");
            store
                .put_oauth_credential(&record, t(2))
                .expect("credential row");
            // Even a failure *reason* is checked: no path writes an unchecked string.
            let reason_err = store.record_account_failure(
                &account("acct"),
                FailureKind::Permanent,
                token.clone(),
                Duration::from_secs(1),
                t(3),
            );
            assert!(
                reason_err.is_err(),
                "token-shaped reasons must be refused too"
            );

            // A deliberate attempt to smuggle the token in through the front door.
            let smuggle = OAuthCredentialRecord::new("codex", token.clone());
            assert!(store.put_oauth_credential(&smuggle, t(4)).is_err());
        }

        let mut all_bytes = Vec::new();
        for entry in std::fs::read_dir(dir.path()).expect("read dir") {
            let entry = entry.expect("entry");
            if entry.path().is_file() {
                all_bytes.extend(std::fs::read(entry.path()).expect("read file"));
            }
        }
        let haystack = String::from_utf8_lossy(&all_bytes);
        assert!(
            !haystack.contains(&token),
            "a raw token must never reach the file"
        );
        assert!(
            !haystack.contains("sk-"),
            "no token prefix may appear in the file"
        );
        assert!(
            !haystack.contains("ghp_"),
            "no GitHub token prefix may appear in the file"
        );
        // The legitimate reference *does* belong in the database — references are the point.
        assert!(haystack.contains("provider/p/account/1"));
    }

    // -- plumbing -----------------------------------------------------------------------

    #[test]
    fn clones_share_one_connection() {
        let store = Store::open_in_memory().expect("open");
        let other = store.clone();
        other
            .record_account_success(&account("shared"), t(5))
            .expect("write via clone");
        assert!(store
            .account_health(&account("shared"))
            .expect("read via original")
            .is_some());
    }

    #[test]
    fn failure_kind_round_trips_through_its_stored_form() {
        for kind in [FailureKind::Transient, FailureKind::Permanent] {
            assert_eq!(FailureKind::parse(kind.as_str()), Some(kind));
            assert_eq!(kind.to_string(), kind.as_str());
        }
        assert_eq!(FailureKind::parse("sometimes"), None);
        assert_eq!(FailureKind::parse(""), None);
    }

    #[test]
    fn state_strings_round_trip_through_the_parser() {
        for state in [
            AccountState::Ready,
            AccountState::Disabled,
            AccountState::RateLimited,
            AccountState::Backoff,
            AccountState::AuthFailed,
            AccountState::QuotaExhausted,
        ] {
            assert_eq!(parse_state(state.as_str()).expect("parse"), state);
        }
        let err = parse_state("half-ready").expect_err("unknown states must error");
        assert!(err.to_string().contains("half-ready"));
    }

    #[test]
    fn debug_formatting_is_safe_on_every_type() {
        let store = Store::open_in_memory().expect("open");
        let (channel, queue) = store.usage_queue(1);
        assert!(format!("{store:?}").contains("Store"));
        assert!(format!("{channel:?}").contains("UsageChannel"));
        let rendered = format!("{queue:?}");
        assert!(
            rendered.contains("UsageQueue"),
            "the writer must be printable: {rendered}"
        );
        assert!(
            !rendered.contains("Store {"),
            "and must not recurse into the store, which owns rows: {rendered}"
        );
        assert_eq!(queue.len(), 0);
    }
}
