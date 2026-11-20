//! The relay's memory of the combo decisions it has made.
//!
//! # Why this lives in the relay and not in the database
//!
//! A combo decision is made by the relay, from a compiled snapshot it already holds, and the panel
//! asks about it while it is happening. Writing each one to the control plane's database would put
//! a row write on the request path to answer a question about "right now", and it would make the
//! panel's live view depend on durability it does not need. This is a fixed ring of the last few
//! decisions, held for the life of the process: restarting the relay forgets them, which is the
//! honest scope of a fact about in-flight work.
//!
//! # Why the entry ends when the body does
//!
//! "In flight" means the client is still receiving the answer, and the only thing in this process
//! that knows when that stops being true is the response body itself. A timer would have to guess
//! how long a stream runs, and a guess is exactly the wrong thing to show an operator debugging a
//! slow route. So the entry is finished by a [`ActivityGuard`] that the client-facing body owns: the
//! guard is dropped when the body is dropped, which is when the relay has finished writing (or the
//! client stopped reading).
//!
//! # What is deliberately absent
//!
//! No labels, no account ids, no request ids, no bodies. The entry names the combo, the string the
//! client sent, and the `provider/model` the relay committed to — the three facts the question "what
//! is this combo doing" is made of. Nothing here can identify a person or carry a credential.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body as AxumBody;
use axum::response::Response;
use bytes::Bytes;
use http_body::Frame;
use pin_project_lite::pin_project;

/// How many decisions the panel can see.
///
/// Small on purpose: a sidebar shows the newest one, and an operator debugging a route looks at the
/// last handful. A larger ring would answer no question the panel asks and would keep old strings
/// alive for the life of the process.
pub const CAPACITY: usize = 24;

/// How a combo request ended, once it has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The relay committed to this step and the client-facing body ended.
    Ok,
    /// No step answered; the relay stopped trying and reported a failure.
    Failed,
}

impl Outcome {
    /// The wire spelling used by `GET /control/activity`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Failed => "failed",
        }
    }
}

/// One combo decision, as the panel reads it.
#[derive(Debug, Clone)]
pub struct Entry {
    /// Internal identity, so a guard can find the one entry it owns without keeping a reference
    /// into the deque. Never serialised: a client does not need it and it would be meaningless
    /// after a restart.
    id: u64,
    /// The combo the client's model name resolved to.
    pub combo: String,
    /// The model string the client actually sent, which may be an alias or a level-suffixed name.
    pub requested: String,
    /// The `provider/model` the relay committed to.
    pub step: String,
    /// The instant the routing decision was taken, in epoch milliseconds.
    pub started_at: u64,
    /// When the client-facing body ended, or `None` while it is still open.
    pub finished_at: Option<u64>,
    /// How it ended, or `None` while it is still open.
    pub outcome: Option<Outcome>,
}

impl Entry {
    /// The JSON object the control endpoint returns for this entry.
    ///
    /// Field names are the contract the panel and the conductor are written against; `finished_at`
    /// and `outcome` are explicitly `null` rather than absent, so a client can tell "still in
    /// flight" from "this build does not report it".
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "combo": self.combo,
            "requested": self.requested,
            "step": self.step,
            "started_at": self.started_at,
            "finished_at": self.finished_at,
            "outcome": self.outcome.map(Outcome::as_str),
        })
    }
}

/// The bounded ring of recent combo decisions.
///
/// One mutex, held for the few microseconds a push or a finish takes. That is the whole cost on the
/// request path: no allocation beyond the three strings an entry is made of, and no lock held
/// across an await.
#[derive(Debug, Default)]
pub struct ActivityLog {
    entries: Mutex<VecDeque<Entry>>,
    next_id: AtomicU64,
}

impl ActivityLog {
    /// An empty log.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a decision that is now in flight, returning the guard that ends it.
    ///
    /// The caller must attach the guard to the client-facing body. Dropping the guard without
    /// attaching it would mark the decision finished immediately, which would be a lie about a
    /// request that is still streaming — so the guard is returned rather than stored here.
    pub fn begin(
        self: &Arc<Self>,
        combo: &str,
        requested: &str,
        step: &str,
        started_at: u64,
    ) -> ActivityGuard {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.push(Entry {
            id,
            combo: combo.to_owned(),
            requested: requested.to_owned(),
            step: step.to_owned(),
            started_at,
            finished_at: None,
            outcome: None,
        });
        ActivityGuard {
            log: Arc::clone(self),
            id,
        }
    }

    /// Record a decision that is already over: no step answered.
    ///
    /// This is finished on arrival because there is nothing left to wait for. A failure response
    /// the client has not read yet is not "in flight" in any useful sense: the relay has stopped
    /// trying, and saying otherwise would make the panel show a failed route as busy.
    pub fn failed(&self, combo: &str, requested: &str, step: &str, started_at: u64) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.push(Entry {
            id,
            combo: combo.to_owned(),
            requested: requested.to_owned(),
            step: step.to_owned(),
            started_at,
            finished_at: Some(epoch_millis()),
            outcome: Some(Outcome::Failed),
        });
    }

    /// Mark one entry finished, if it is still open.
    ///
    /// Idempotent: a guard that is dropped after an explicit finish, or an entry evicted from the
    /// ring before its body ended, both find nothing to do. Eviction is why this takes an id and
    /// not an index.
    fn finish(&self, id: u64, outcome: Outcome) {
        let now = epoch_millis();
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = entries.iter_mut().find(|entry| entry.id == id) {
            if entry.finished_at.is_none() {
                entry.finished_at = Some(now);
                entry.outcome = Some(outcome);
            }
        }
    }

    /// The decisions, newest first.
    ///
    /// Cloned rather than returned as a guard: the caller serialises them, and holding the lock
    /// while a JSON document is built would make a slow panel request block the next combo
    /// decision.
    pub fn snapshot(&self) -> Vec<Entry> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    /// The number of decisions currently held.
    pub fn len(&self) -> usize {
        self.entries.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Whether no decision has been recorded.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Insert newest first, dropping the oldest when the ring is full.
    fn push(&self, entry: Entry) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.push_front(entry);
        while entries.len() > CAPACITY {
            entries.pop_back();
        }
    }
}

/// Owns the live half of one decision.
///
/// The entry finishes when this is dropped, which is arranged to happen when the client-facing body
/// is dropped. A poisoned lock is recovered rather than propagated: a panic while recording an
/// activity entry must not take down a request that is otherwise fine.
#[derive(Debug)]
pub struct ActivityGuard {
    log: Arc<ActivityLog>,
    id: u64,
}

impl ActivityGuard {
    /// Mark this decision failed immediately, without waiting for the body.
    ///
    /// Used when the request ended without an answer (a routed error, a refusal after the hold
    /// ceiling, a fatal transport failure). The drop handler is a no-op afterwards: `finish`
    /// only writes an outcome once, so a later `Ok` cannot overwrite the failure.
    pub(crate) fn finish_failed(self) {
        self.log.finish(self.id, Outcome::Failed);
    }
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        self.log.finish(self.id, Outcome::Ok);
    }
}

pin_project! {
    /// A client-facing body that keeps one activity decision open for as long as it exists.
    ///
    /// The guard is a field, not a spawn: it is released by the same drop that releases the body,
    /// in the same task, with no ordering question about whether the stream or the bookkeeping
    /// finished first.
    pub struct WatchedBody {
        #[pin]
        inner: AxumBody,
        // Held for its `Drop`. Never read.
        _guard: ActivityGuard,
    }
}

impl http_body::Body for WatchedBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        self.project().inner.poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

/// Attach a decision's lifetime to a response body.
///
/// Called only for a served combo answer. The response is otherwise untouched: headers, status and
/// frames pass through exactly as they would have.
pub fn watch(response: Response, guard: ActivityGuard) -> Response {
    let (parts, body) = response.into_parts();
    Response::from_parts(
        parts,
        AxumBody::new(WatchedBody {
            inner: body,
            _guard: guard,
        }),
    )
}

/// Wall-clock milliseconds, with a pre-1970 clock treated as zero.
///
/// The same convention as `mr_core::ids::now_millis` at its call sites: a clock that cannot be
/// trusted is clamped rather than allowed to produce a negative age the panel would render as
/// nonsense.
fn epoch_millis() -> u64 {
    mr_core::ids::now_millis().max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log() -> Arc<ActivityLog> {
        Arc::new(ActivityLog::new())
    }

    #[test]
    fn a_finished_request_is_marked_finished_when_the_body_drops() {
        // The whole point of the guard: liveness is bound to the client-facing body, so it cannot
        // be right for one and wrong for the other.
        let log = log();
        let guard = log.begin("coding", "coding", "openai/gpt-x", 1_000);
        let response = watch(Response::new(AxumBody::empty()), guard);

        let open = log.snapshot();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].combo, "coding");
        assert_eq!(open[0].step, "openai/gpt-x");
        assert_eq!(open[0].started_at, 1_000);
        assert!(open[0].finished_at.is_none(), "the body is still alive");
        assert!(open[0].outcome.is_none());

        drop(response);

        let closed = log.snapshot();
        assert!(closed[0].finished_at.is_some(), "the body was dropped");
        assert_eq!(closed[0].outcome, Some(Outcome::Ok));
    }

    #[test]
    fn a_failed_decision_is_finished_when_it_is_recorded() {
        let log = log();
        log.failed("coding", "coding-high", "openai/gpt-x", 2_000);
        let entries = log.snapshot();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].requested, "coding-high");
        assert_eq!(entries[0].outcome, Some(Outcome::Failed));
        // Finished when the relay gave up, not when the request started: a failure entry names a
        // real end, so the panel's age is the age of the failure.
        assert!(
            entries[0]
                .finished_at
                .is_some_and(|finished| finished >= entries[0].started_at),
            "the failure must be finished at or after it began"
        );
    }

    #[test]
    fn only_the_newest_decisions_are_kept_and_they_are_newest_first() {
        let log = log();
        for i in 0..(CAPACITY as u64 + 6) {
            log.failed("coding", "coding", &format!("p/m{i}"), i);
        }
        let entries = log.snapshot();
        assert_eq!(entries.len(), CAPACITY);
        // Newest first: the panel reads entries[0].
        assert_eq!(entries[0].step, format!("p/m{}", CAPACITY + 5));
        assert_eq!(entries[CAPACITY - 1].step, "p/m6");
    }

    #[test]
    fn evicting_an_open_entry_leaves_the_remaining_ones_consistent() {
        // A guard can outlive its entry when the ring wraps. Its drop must not panic or corrupt a
        // neighbour, and the open entry must simply be gone.
        let log = log();
        let guard = log.begin("coding", "coding", "p/old", 0);
        for i in 0..CAPACITY as u64 {
            log.failed("coding", "coding", &format!("p/m{i}"), i);
        }
        assert_eq!(log.len(), CAPACITY);
        drop(guard);
        assert_eq!(log.len(), CAPACITY);
        assert!(log.snapshot().iter().all(|e| e.finished_at.is_some()));
    }

    #[test]
    fn an_entry_serialises_with_nulls_while_in_flight() {
        // The panel distinguishes "still running" from "not reported" by the field being present
        // and null, so the shape is pinned here.
        let log = log();
        let guard = log.begin(
            "expensivesmart",
            "expensivesmart",
            "openai-codex/gpt-6-luna",
            7,
        );
        let json = log.snapshot()[0].to_json();
        assert_eq!(json["combo"], "expensivesmart");
        assert_eq!(json["requested"], "expensivesmart");
        assert_eq!(json["step"], "openai-codex/gpt-6-luna");
        assert_eq!(json["started_at"], 7);
        assert!(json["finished_at"].is_null());
        assert!(json["outcome"].is_null());
        assert!(json.get("id").is_none(), "internal identity is not exposed");
        drop(guard);
    }

    #[test]
    fn an_empty_log_reports_no_activity() {
        let log = log();
        assert!(log.is_empty());
        assert!(log.snapshot().is_empty());
    }
}
