//! Per-key quota enforcement: admission, reservation and settlement.
//!
//! A client key can carry caps ([`mr_core::model::KeyLimits`]) on the requests it makes and on the
//! tokens it spends, per rolling window, plus a flat, non-rolling request budget
//! ([`mr_core::model::ClientKey::limit_requests`]). This module turns those caps into two
//! decisions the request path can act on:
//!
//! * **admission** — before any upstream side effect, may this key make this request?
//! * **settlement** — after the response has ended, what did it actually cost?
//!
//! # Why a reservation, and not just a counter
//!
//! The caps are on *requests* and on *tokens*, and a token count is only known once the model has
//! answered. A plain "count what has been spent, then compare" check is wrong twice over: it lets
//! two concurrent requests both pass a check that only one of them may pass, and it cannot reserve
//! a token count that does not exist yet.
//!
//! So every admitted request holds a **reservation** until it settles. The reservation is what the
//! next admission sees, which makes the check atomic against concurrent requests: the ledger read
//! and the reservation write happen under one lock, so no two admissions can interleave between
//! them.
//!
//! # What is reserved, and what is not
//!
//! A reservation counts **one request** and an **estimate of the prompt's tokens** — the request
//! body divided by [`CHARS_PER_TOKEN`], which is the usual English approximation. The
//! *completion* is not reserved, because its size is unknown until the model has produced it and
//! guessing it would be inventing a number the operator never asked for. The consequence is
//! stated plainly rather than hidden: **a token cap can be overshot by the request that crosses
//! it**, and the request after it is refused. What the reservation does guarantee is that two
//! concurrent requests cannot both slip past a cap that only one of them may pass.
//!
//! # The cost, and who pays it
//!
//! Admission reads the ledger, which is one small SQLite read, and settlement writes one row. Both
//! happen **only for keys that actually carry caps** — a key with no limits is not metered at all
//! and pays nothing. That is the whole reason a limit is opt-in: the overwhelming majority of keys
//! have none, and a gateway that wrote to its database on every request would spend its latency
//! budget on accounting.
//!
//! # What the ledger is
//!
//! The settled usage is written to `key_usage` in the store — the same append-only ledger the
//! quota panel already reads. One row per admitted request, written when the request settles, with
//! the token counts the upstream reported (or none, when the upstream reported none).
//!
//! A request that is admitted and then fails still writes its row: it was admitted, and it is the
//! number of *admissions* the request caps count. A request that is **refused** writes nothing.
//!
//! # What is not counted
//!
//! * A request the relay never admits (no key, a wrong key) is not metered.
//! * A request that is in flight when the relay process dies leaves no row, and its reservation
//!   dies with the process. Both are undercounts of one request, never overcounts.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::Frame;
use mr_core::ids::Millis;
use mr_core::model::KeyLimits;
use mr_core::store::{KeyUsageRecord, Store};
use mr_core::usage::TokenUsage;

use crate::usage_tap::UsageTap;

/// The rolling five-hour window, in milliseconds.
const SESSION_5H_MS: i64 = 5 * 60 * 60 * 1000;
/// The rolling seven-day window, in milliseconds.
const WEEKLY_1W_MS: i64 = 7 * 24 * 60 * 60 * 1000;
/// The rolling thirty-day window, in milliseconds.
const MONTHLY_30D_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// Characters per token, used to estimate a prompt's size.
///
/// Four is the usual approximation for English text. It is an estimate and is treated as one: it
/// sizes a reservation, and it is replaced by the upstream's own count when the request settles.
pub const CHARS_PER_TOKEN: u64 = 4;

/// One request's place in the in-flight ledger.
#[derive(Debug, Clone)]
struct Reservation {
    /// The request's id, so the reservation can be released when it settles.
    request_id: String,
    /// The prompt tokens reserved for it, estimated from the request body.
    reserved_prompt_tokens: u64,
}

/// The reservations that are currently in flight, per client key.
#[derive(Debug, Default)]
struct InFlight {
    /// Client key id → its in-flight reservations.
    by_key: HashMap<String, Vec<Reservation>>,
}

impl InFlight {
    /// Drop a reservation, whatever key it belongs to.
    ///
    /// The lookup is by request id because the settlement path knows the request, not necessarily
    /// the key it was admitted under — the two are the same in practice, and searching by request
    /// id keeps the release correct even if that ever changes.
    fn release(&mut self, request_id: &str) {
        for reservations in self.by_key.values_mut() {
            if let Some(at) = reservations.iter().position(|r| r.request_id == request_id) {
                reservations.remove(at);
                return;
            }
        }
    }
}

/// Why a request was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DenialReason {
    /// The key has used up its request budget.
    Requests,
    /// The key has used up its token budget.
    Tokens,
    /// The ledger could not be read, so no honest decision was possible.
    ///
    /// This is deliberately **not** the same as "allowed". A cap that cannot be checked is not a cap
    /// that has been satisfied, and a gateway that quietly stops enforcing a budget the moment its
    /// database hiccups is not enforcing it.
    LedgerUnavailable,
}

/// A refused request, with what the client should be told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denial {
    /// Why the request was refused.
    pub reason: DenialReason,
    /// A message for the client, in the dialect it asked in.
    pub message: String,
    /// When the client may try again, if that is known.
    pub retry_after_ms: Option<u64>,
}

/// The outcome of an admission check.
#[derive(Debug)]
pub enum Admission {
    /// The key carries no caps, so the request is not metered at all.
    Unmetered,
    /// The request may proceed. The ticket settles it when it ends.
    Allowed(Arc<QuotaTicket>),
    /// The request may not proceed.
    Denied(Denial),
}

/// The ledger of settled usage, plus the reservations that are in flight.
#[derive(Debug)]
pub struct QuotaManager {
    /// Where settled usage is written and read from.
    store: Arc<Store>,
    /// The in-flight reservations.
    ///
    /// A plain mutex rather than a lock-free structure: the critical section is a SQLite read and a
    /// vector push, and a lock is easier to reason about than a lock-free one when the thing being
    /// protected is a budget.
    in_flight: Mutex<InFlight>,
}

impl QuotaManager {
    /// A manager over a store.
    pub fn new(store: Arc<Store>) -> Self {
        Self {
            store,
            in_flight: Mutex::new(InFlight::default()),
        }
    }

    /// Whether a key with these limits is metered at all.
    ///
    /// A key with no caps and no flat budget is not tracked: it costs nothing on the request path,
    /// which is what keeps the feature free for the keys that do not ask for it.
    pub fn is_metered(limits: &KeyLimits, flat_requests: u64) -> bool {
        flat_requests > 0 || !limits.is_empty()
    }

    /// Decide whether a request may proceed, and reserve what it will cost.
    ///
    /// The whole decision happens under one lock: the ledger is read, the caps are compared and the
    /// reservation is written before any other admission can look at the same key. That is what
    /// makes the check atomic against concurrent requests.
    pub fn admit(
        self: &Arc<Self>,
        key_id: &str,
        limits: &KeyLimits,
        flat_requests: u64,
        request_id: &str,
        prompt_estimate: u64,
        now: Millis,
    ) -> Admission {
        if !Self::is_metered(limits, flat_requests) {
            return Admission::Unmetered;
        }

        let mut in_flight = match self.in_flight.lock() {
            Ok(guard) => guard,
            // A poisoned lock means a previous holder panicked. The ledger is still readable and the
            // reservations are still there, so the budget can still be enforced; refusing every
            // request because of an unrelated panic would be a worse failure than carrying on.
            Err(poisoned) => poisoned.into_inner(),
        };

        let reservations = in_flight.by_key.entry(key_id.to_owned()).or_default();
        let reserved_requests = reservations.len() as u64;
        let reserved_tokens: u64 = reservations.iter().map(|r| r.reserved_prompt_tokens).sum();

        // The flat budget counts every request the key has ever been admitted for, so it reads the
        // whole ledger rather than a window.
        if flat_requests > 0 {
            let total = match self.store.key_window_usage(key_id, Millis(0)) {
                Ok(total) => total,
                Err(e) => return ledger_unavailable(e),
            };
            if total.requests + reserved_requests >= flat_requests {
                return Admission::Denied(Denial {
                    reason: DenialReason::Requests,
                    message: format!(
                        "this key has used its request budget of {flat_requests} request(s)"
                    ),
                    retry_after_ms: None,
                });
            }
        }

        for (limit, span_ms, name) in [
            (limits.session_5h, SESSION_5H_MS, "five-hour"),
            (limits.weekly_1w, WEEKLY_1W_MS, "weekly"),
            (limits.monthly_30d, MONTHLY_30D_MS, "monthly"),
        ] {
            let Some(limit) = limit else { continue };
            if limit.is_inert() {
                continue;
            }
            let since = Millis(now.as_millis().saturating_sub(span_ms));
            let used = match self.store.key_window_usage(key_id, since) {
                Ok(used) => used,
                Err(e) => return ledger_unavailable(e),
            };
            let requests = used.requests + reserved_requests;
            let tokens = used.prompt_tokens + used.completion_tokens + reserved_tokens;
            if limit.requests > 0 && requests >= limit.requests {
                return Admission::Denied(Denial {
                    reason: DenialReason::Requests,
                    message: format!(
                        "this key has used its {name} budget of {} request(s)",
                        limit.requests
                    ),
                    retry_after_ms: Some(span_ms as u64),
                });
            }
            if limit.tokens > 0 && tokens >= limit.tokens {
                return Admission::Denied(Denial {
                    reason: DenialReason::Tokens,
                    message: format!(
                        "this key has used its {name} budget of {} token(s)",
                        limit.tokens
                    ),
                    retry_after_ms: Some(span_ms as u64),
                });
            }
        }

        reservations.push(Reservation {
            request_id: request_id.to_owned(),
            reserved_prompt_tokens: prompt_estimate,
        });

        Admission::Allowed(Arc::new(QuotaTicket {
            manager: Arc::clone(self),
            key_id: key_id.to_owned(),
            request_id: request_id.to_owned(),
            occurred_at: now,
            usage: Arc::new(Mutex::new(None)),
            settled: AtomicBool::new(false),
        }))
    }

    /// Write a request's actual spend and release its reservation.
    fn settle(
        &self,
        key_id: &str,
        request_id: &str,
        usage: Option<TokenUsage>,
        occurred_at: Millis,
    ) {
        // The row is written **before** the reservation is released. The other order would open a
        // window in which the request is neither in flight nor in the ledger, and a request that
        // fell into that window would be counted by neither.
        let mut record = KeyUsageRecord::new(key_id, occurred_at);
        if let Some(usage) = usage {
            record.prompt_tokens = usage.input_tokens;
            record.completion_tokens = usage.output_tokens;
        }
        if let Err(e) = self.store.record_key_usage(&record) {
            // The usage is lost, and the count is one short. There is nothing better to do here:
            // the request has already been answered, and refusing to answer it now would be
            // worse than losing one row.
            tracing::warn!(
                key_id,
                request_id,
                error = %e,
                "could not write the key usage row; the ledger is one request short"
            );
        }
        let mut in_flight = match self.in_flight.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        in_flight.release(request_id);
    }
}

/// The refusal used when the ledger cannot be read.
fn ledger_unavailable(error: impl std::fmt::Display) -> Admission {
    tracing::warn!(error = %error, "the key usage ledger could not be read");
    Admission::Denied(Denial {
        reason: DenialReason::LedgerUnavailable,
        message: "the quota ledger is unavailable, so this request cannot be admitted".to_owned(),
        retry_after_ms: Some(1_000),
    })
}

/// A request's reservation, released when it settles.
///
/// The ticket settles itself when it is dropped, which is what makes the accounting work without a
/// second code path: every way out of a request — a normal answer, an upstream error, a client
/// that hung up — drops the ticket.
#[derive(Debug)]
pub struct QuotaTicket {
    /// The manager that holds the store and the reservations.
    manager: Arc<QuotaManager>,
    /// The key the request was admitted for.
    key_id: String,
    /// The request's id.
    request_id: String,
    /// When the request was admitted, in milliseconds since the epoch.
    occurred_at: Millis,
    /// The usage observed on the wire, filled in by the tap as the response arrives.
    usage: Arc<Mutex<Option<TokenUsage>>>,
    /// Whether the ticket has already settled.
    settled: AtomicBool,
}

impl QuotaTicket {
    /// The slot the tap writes the observed usage into.
    pub fn usage_slot(&self) -> Arc<Mutex<Option<TokenUsage>>> {
        Arc::clone(&self.usage)
    }
}

impl Drop for QuotaTicket {
    fn drop(&mut self) {
        if self.settled.swap(true, Ordering::SeqCst) {
            return;
        }
        let usage = self.usage.lock().ok().and_then(|slot| *slot);
        self.manager
            .settle(&self.key_id, &self.request_id, usage, self.occurred_at);
    }
}

/// A response body that reports what it carried.
///
/// The tap observes the bytes the upstream sent and never changes them: the client receives the
/// upstream's own bytes, and the tap only reads them. That is what makes it safe to put on the
/// path — the alternative, parsing the body for its usage field, would mean reading a body the
/// relay is supposed to be shuttling rather than interpreting.
pub struct TapBody<B> {
    /// The body being observed.
    inner: B,
    /// The tap, fed with the bytes as they pass.
    tap: UsageTap,
    /// The ticket the usage is published to.
    ///
    /// The body holds the ticket, not just its slot, because the settlement has to wait for the
    /// body: a stream that is still being sent has not finished spending the request's budget.
    ticket: Arc<QuotaTicket>,
    /// Whether the tap has been finished, so the usage is published exactly once.
    finished: bool,
}

impl<B> TapBody<B> {
    /// Wrap a body so that the usage it carries is published to its ticket.
    pub fn new(inner: B, ticket: Arc<QuotaTicket>) -> Self {
        Self {
            inner,
            tap: UsageTap::new(),
            ticket,
            finished: false,
        }
    }

    /// Publish whatever the tap has seen.
    fn publish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        // `finish` consumes the tap and drains the partial tail it was holding, which is where a
        // final fragment — often the usage report — ends up.
        let tap = std::mem::replace(&mut self.tap, UsageTap::new());
        let usage = tap.finish().usage;
        if usage.is_empty() {
            return;
        }
        if let Ok(mut slot) = self.ticket.usage.lock() {
            *slot = Some(usage);
        }
    }
}

impl<B> Drop for TapBody<B> {
    fn drop(&mut self) {
        // A body that was dropped before it ended — a client that hung up — still publishes what
        // it saw, so a partial stream is accounted for as the partial stream it was.
        self.publish();
    }
}

impl<B> http_body::Body for TapBody<B>
where
    B: http_body::Body<Data = Bytes> + Unpin,
{
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = &mut *self;
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    this.tap.observe_chunk(data);
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(None) => {
                this.publish();
                Poll::Ready(None)
            }
            other => other,
        }
    }
}

/// Estimate a prompt's token count from the size of the request body.
///
/// Four characters per token is the usual approximation for English text. It sizes a reservation,
/// and the upstream's own count replaces it when the request settles.
pub fn estimate_prompt_tokens(body_bytes: usize) -> u64 {
    (body_bytes as u64) / CHARS_PER_TOKEN
}

#[cfg(test)]
mod tests {
    use super::*;
    use mr_core::model::KeyLimit;

    fn manager() -> (Arc<QuotaManager>, Arc<Store>) {
        let store = Arc::new(Store::open_in_memory().unwrap());
        (Arc::new(QuotaManager::new(Arc::clone(&store))), store)
    }

    /// A window with only a request cap.
    fn request_caps(requests: u64) -> KeyLimits {
        KeyLimits {
            session_5h: Some(KeyLimit {
                tokens: 0,
                requests,
            }),
            ..Default::default()
        }
    }

    /// A window with only a token cap.
    fn token_caps(tokens: u64) -> KeyLimits {
        KeyLimits {
            session_5h: Some(KeyLimit {
                tokens,
                requests: 0,
            }),
            ..Default::default()
        }
    }

    /// Admit a request and settle it with the given token counts.
    /// Admit a request and fill in the usage its ticket will report.
    ///
    /// The ticket settles when the last handle to it is dropped, which is the returned
    /// `Admission`: a test that uses the result as a plain statement has dropped it by the time
    /// the next line runs, and the ledger therefore already holds the request.
    fn admit_and_settle(
        manager: &Arc<QuotaManager>,
        key: &str,
        limits: &KeyLimits,
        flat: u64,
        prompt: u64,
        completion: u64,
    ) -> Admission {
        let request_id = format!("req-{prompt}-{completion}");
        let admission = manager.admit(key, limits, flat, &request_id, prompt, Millis::now());
        if let Admission::Allowed(ticket) = &admission {
            let usage = TokenUsage {
                input_tokens: Some(prompt),
                output_tokens: Some(completion),
                ..Default::default()
            };
            if let Ok(mut slot) = ticket.usage_slot().lock() {
                *slot = Some(usage);
            }
        }
        admission
    }

    #[test]
    fn a_key_with_no_caps_is_not_metered_at_all() {
        let (manager, _store) = manager();
        let limits = KeyLimits::default();
        assert!(!QuotaManager::is_metered(&limits, 0));
        assert!(matches!(
            manager.admit("k", &limits, 0, "req-1", 100, Millis::now()),
            Admission::Unmetered
        ));
        // Nothing was reserved, so nothing was written either.
        assert_eq!(
            manager
                .store
                .key_window_usage("k", Millis(0))
                .unwrap()
                .requests,
            0
        );
    }

    #[test]
    fn a_request_cap_refuses_the_request_that_would_exceed_it() {
        let (manager, _store) = manager();
        let limits = request_caps(2);
        assert!(matches!(
            admit_and_settle(&manager, "k", &limits, 0, 10, 5),
            Admission::Allowed(_)
        ));
        assert!(matches!(
            admit_and_settle(&manager, "k", &limits, 0, 10, 5),
            Admission::Allowed(_)
        ));
        let third = admit_and_settle(&manager, "k", &limits, 0, 10, 5);
        match third {
            Admission::Denied(denial) => {
                assert_eq!(denial.reason, DenialReason::Requests);
                assert!(denial.message.contains("five-hour"));
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_refused_request_is_not_counted() {
        let (manager, store) = manager();
        let limits = request_caps(1);
        admit_and_settle(&manager, "k", &limits, 0, 10, 5);
        assert!(matches!(
            admit_and_settle(&manager, "k", &limits, 0, 10, 5),
            Admission::Denied(_)
        ));
        // One request was admitted and one was refused, and only the admitted one is in the ledger.
        assert_eq!(store.key_window_usage("k", Millis(0)).unwrap().requests, 1);
    }

    #[test]
    fn a_token_cap_refuses_the_request_after_the_budget_is_spent() {
        // The cap is checked against what has already been spent, so the request that *crosses*
        // the cap is served and the one after it is refused. That overshoot is the honest
        // consequence of not knowing the completion size before the model has answered.
        let (manager, _store) = manager();
        let limits = token_caps(100);
        assert!(matches!(
            admit_and_settle(&manager, "k", &limits, 0, 60, 30),
            Admission::Allowed(_)
        ));
        // 90 tokens spent, 10 left: this request is admitted and takes the total over the cap.
        assert!(matches!(
            admit_and_settle(&manager, "k", &limits, 0, 10, 5),
            Admission::Allowed(_)
        ));
        match admit_and_settle(&manager, "k", &limits, 0, 10, 5) {
            Admission::Denied(denial) => {
                assert_eq!(denial.reason, DenialReason::Tokens);
                assert!(denial.message.contains("token"));
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn the_flat_budget_counts_every_request_the_key_has_ever_made() {
        let (manager, _store) = manager();
        let limits = KeyLimits::default();
        assert!(matches!(
            admit_and_settle(&manager, "k", &limits, 2, 10, 5),
            Admission::Allowed(_)
        ));
        assert!(matches!(
            admit_and_settle(&manager, "k", &limits, 2, 10, 5),
            Admission::Allowed(_)
        ));
        match admit_and_settle(&manager, "k", &limits, 2, 10, 5) {
            Admission::Denied(denial) => {
                assert_eq!(denial.reason, DenialReason::Requests);
                assert!(denial.message.contains("request budget"));
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn two_admissions_cannot_both_pass_a_cap_of_one() {
        // This is the case a plain counter gets wrong: both requests are in flight at the same
        // time, so neither has been written to the ledger yet, and only the reservation can tell
        // the second one that the budget is already spent.
        let (manager, _store) = manager();
        let limits = request_caps(1);
        let first = manager.admit("k", &limits, 0, "req-1", 10, Millis::now());
        assert!(matches!(first, Admission::Allowed(_)));
        let second = manager.admit("k", &limits, 0, "req-2", 10, Millis::now());
        match second {
            Admission::Denied(denial) => assert_eq!(denial.reason, DenialReason::Requests),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_settled_request_is_written_to_the_ledger_with_its_tokens() {
        let (manager, store) = manager();
        let limits = request_caps(10);
        admit_and_settle(&manager, "k", &limits, 0, 120, 30);
        let used = store.key_window_usage("k", Millis(0)).unwrap();
        assert_eq!(used.requests, 1);
        assert_eq!(used.prompt_tokens, 120);
        assert_eq!(used.completion_tokens, 30);
    }

    #[test]
    fn an_in_flight_request_reserves_its_prompt_tokens() {
        // The completion is not reserved — it is unknown — but the prompt is, so the reservations
        // of two requests in flight are seen against the cap together.
        let (manager, _store) = manager();
        let limits = token_caps(100);
        let first = manager.admit("k", &limits, 0, "req-1", 90, Millis::now());
        assert!(matches!(first, Admission::Allowed(_)));
        // 90 tokens are reserved, 10 are left: this one is admitted and reserves the rest.
        let second = manager.admit("k", &limits, 0, "req-2", 10, Millis::now());
        assert!(matches!(second, Admission::Allowed(_)));
        let third = manager.admit("k", &limits, 0, "req-3", 1, Millis::now());
        match third {
            Admission::Denied(denial) => assert_eq!(denial.reason, DenialReason::Tokens),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_settled_request_releases_its_reservation() {
        let (manager, _store) = manager();
        let limits = request_caps(1);
        let first = manager.admit("k", &limits, 0, "req-1", 10, Millis::now());
        let Admission::Allowed(ticket) = first else {
            panic!("expected the first request to be admitted");
        };
        drop(ticket);
        // The request settled, so it is in the ledger and the reservation is gone: the cap of one
        // is spent, and the next request is refused.
        let second = manager.admit("k", &limits, 0, "req-2", 10, Millis::now());
        assert!(matches!(second, Admission::Denied(_)));
    }

    #[test]
    fn a_body_of_nothing_is_a_body_of_no_tokens() {
        // The estimate is only ever a size, and a body of nothing is a body of no tokens.
        assert_eq!(estimate_prompt_tokens(0), 0);
        assert_eq!(estimate_prompt_tokens(40), 10);
    }
}
