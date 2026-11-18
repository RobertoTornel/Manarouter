//! Declared per-account quota accounting: record dials, settle tokens, enforce windows.
//!
//! The counting rules themselves live in [`mr_core::account_quota`], which knows nothing about
//! HTTP. This module is the relay side of that split:
//!
//! * [`record_dial`] appends one row the moment the relay chooses an account, so a request the
//!   upstream refuses still counts against a request budget.
//! * [`AccountSpendTicket`] settles the upstream's reported token counts when the response ends,
//!   then cools the account down for the moment a full window frees up.
//! * [`filter_spent_candidates`] keeps a full account out of a step's candidate list, so failover
//!   moves to a sibling rather than to a spent account, and the last-resort sweep never wakes one.
//!
//! Every failure in this module is a warning, never an error response. A quota ledger that
//! refuses a request because it could not count one would turn a storage hiccup into an outage,
//! which is the wrong trade for an advisory cap: the operator's windows are budgets, not a second
//! authentication layer.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use mr_core::account_quota::{exhaustion, Exhaustion as QuotaExhaustion, SpendEvent};
use mr_core::ids::{AccountId, Millis};
use mr_core::snapshot::AccountPlan;
use mr_core::store::{AccountSpendRecord, Store};

use crate::select::{CooldownReason, Health};
use crate::usage_tap::{GenerationUsage, UsageTap};

/// Append one dialled request to the spend ledger.
///
/// The row is written before the upstream is dialled: counting a request when it is answered
/// instead would leave a run of refused requests invisible to a request budget. Returns the
/// record, or `None` when the ledger could not be written — the request still goes out.
pub fn record_dial(
    store: &Store,
    provider_id: &str,
    account_id: &AccountId,
    now_ms: u64,
) -> Option<AccountSpendRecord> {
    let record = AccountSpendRecord::new(
        provider_id,
        account_id.clone(),
        Millis::from_millis(now_ms as i64),
    );
    match store.record_account_spend(&record) {
        Ok(()) => Some(record),
        Err(error) => {
            tracing::warn!(
                provider = provider_id,
                account = %account_id,
                error = %error,
                "could not record the account spend row; the request goes out uncounted"
            );
            None
        }
    }
}

/// The earliest `since` a reader needs, for the longest declared window on an account.
///
/// `None` when the account declares no window that can stop it. A read since an earlier instant
/// would pull events no window can see, which is a scan with no verdict at the end of it.
fn since_for(account: &AccountPlan, now_ms: u64) -> Option<u64> {
    let longest = account
        .quota
        .iter()
        .filter(|window| window.seconds > 0 && window.constrains())
        .map(|window| window.seconds)
        .max()?;
    let span_ms = u64::try_from(longest).ok()?.saturating_mul(1000);
    Some(now_ms.saturating_sub(span_ms))
}

/// Whether an account's declared windows are full, and when the last full one frees.
///
/// Reads the account's own events only, so the cost is proportional to one account's
/// recent traffic rather than to the whole ledger.
pub fn exhaustion_for(
    store: &Store,
    account: &AccountPlan,
    now_ms: u64,
) -> Option<QuotaExhaustion> {
    if account.quota.is_empty() {
        return None;
    }
    let since = since_for(account, now_ms)?;
    let events = match store.account_spend_since(account.id.as_str(), since) {
        Ok(events) => events,
        Err(error) => {
            tracing::warn!(
                account = %account.id,
                error = %error,
                "could not read the account spend ledger; the account is treated as usable"
            );
            return None;
        }
    };
    exhaustion(&account.quota, &events, now_ms)
}

/// Keep only the candidates whose declared windows are not full.
///
/// An account without declared windows is never filtered here — it has no cap to be over.
pub fn filter_spent_candidates<'a>(
    candidates: Vec<&'a AccountPlan>,
    store: &Store,
    now_ms: u64,
) -> Vec<&'a AccountPlan> {
    candidates
        .into_iter()
        .filter(|account| match exhaustion_for(store, account, now_ms) {
            Some(hit) => {
                tracing::info!(
                    account = %account.id,
                    window = hit.window_index,
                    reset_at_ms = hit.reset_at_ms,
                    "account skipped: a declared quota window is full"
                );
                false
            }
            None => true,
        })
        .collect()
}

/// Whether `events` leave any of `windows` full, for callers that already hold the events.
///
/// The selection path prefers [`exhaustion_for`] — it reads the ledger itself — but the pure form
/// is what a step-availability check with events already in hand uses, and it is what the tests
/// pin the arithmetic through.
pub fn exhaustion_from_events(
    windows: &[mr_core::model::QuotaWindow],
    events: &[SpendEvent],
    now_ms: u64,
) -> Option<QuotaExhaustion> {
    exhaustion(windows, events, now_ms)
}

/// An account's in-flight spend settlement.
///
/// Created when the relay dials an account, dropped when the response is over — normally, by a
/// client hang-up, or by an upstream that never answered. Settling in `Drop` is what makes the
/// accounting cover every way out of a request without a second code path.
pub struct AccountSpendTicket {
    store: Arc<Store>,
    health: Arc<Health>,
    provider_id: String,
    account_id: AccountId,
    record_id: String,
    windows: Vec<mr_core::model::QuotaWindow>,
    usage: Arc<Mutex<Option<GenerationUsage>>>,
    occurred_at_ms: u64,
    settled: AtomicBool,
}

impl AccountSpendTicket {
    /// Record the dial and return the ticket that will settle it.
    ///
    /// `None` when the spend row could not be written: there is then nothing to settle against,
    /// and wrapping the body to observe usage that will be discarded is cost with no reader.
    pub fn open(
        store: Arc<Store>,
        health: Arc<Health>,
        provider_id: &str,
        account: &AccountPlan,
        now_ms: u64,
    ) -> Option<Arc<Self>> {
        if account.quota.is_empty() {
            return None;
        }
        let record = record_dial(&store, provider_id, &account.id, now_ms)?;
        Some(Arc::new(Self {
            store,
            health,
            provider_id: provider_id.to_owned(),
            account_id: account.id.clone(),
            record_id: record.id,
            windows: account.quota.clone(),
            usage: Arc::new(Mutex::new(None)),
            occurred_at_ms: now_ms,
            settled: AtomicBool::new(false),
        }))
    }

    /// The slot the tap writes the observed usage into.
    pub fn usage_slot(&self) -> Arc<Mutex<Option<GenerationUsage>>> {
        Arc::clone(&self.usage)
    }

    /// Settle the dialled request now, idempotently.
    ///
    /// The token counts the upstream reported replace the unknown ones; a response that reported
    /// none leaves them unknown rather than inventing zeros. Then the account is re-measured
    /// including this request, and parked until a full window frees — but only until that
    /// instant, and only with reason [`CooldownReason::QuotaExhausted`], so the reset-aware view
    /// shows when this account comes back rather than treating it as a failure to be retried.
    pub fn settle(&self) {
        if self.settled.swap(true, Ordering::SeqCst) {
            return;
        }
        let observed = self
            .usage
            .lock()
            .ok()
            .and_then(|slot| *slot)
            .filter(|usage| !usage.is_empty());
        if let Some(usage) = observed {
            let usage = usage.usage;
            if let Err(error) = self.store.settle_account_spend(
                &self.record_id,
                usage.input_tokens,
                usage.output_tokens,
            ) {
                tracing::warn!(
                    provider = self.provider_id,
                    account = %self.account_id,
                    error = %error,
                    "could not settle the account spend row"
                );
            }
        }
        if self.windows.is_empty() {
            return;
        }
        // Re-measure from the ledger, this request included. The row was written at dial time,
        // so it is already in the events the read returns.
        let Some(since) = self
            .windows
            .iter()
            .filter(|window| window.seconds > 0 && window.constrains())
            .map(|window| window.seconds)
            .max()
            .map(|seconds| {
                self.occurred_at_ms
                    .saturating_sub(u64::try_from(seconds).unwrap_or(0).saturating_mul(1000))
            })
        else {
            return;
        };
        let events = match self
            .store
            .account_spend_since(self.account_id.as_str(), since)
        {
            Ok(events) => events,
            Err(error) => {
                tracing::warn!(
                    account = %self.account_id,
                    error = %error,
                    "could not re-measure the account after settling"
                );
                return;
            }
        };
        if let Some(hit) = exhaustion(&self.windows, &events, self.occurred_at_ms) {
            tracing::info!(
                account = %self.account_id,
                window = hit.window_index,
                reset_at_ms = hit.reset_at_ms,
                "account parked: a declared quota window is full"
            );
            self.health.cool_down(
                self.account_id.as_str(),
                hit.reset_at_ms,
                CooldownReason::QuotaExhausted,
            );
        }
    }
}

impl Drop for AccountSpendTicket {
    fn drop(&mut self) {
        self.settle();
    }
}

/// A response body that reports what the account spent.
///
/// Mirrors the per-key `TapBody`: the bytes pass through untouched, the tap only reads them, and a
/// body dropped mid-stream still publishes the partial counts it saw.
pub struct SpendBody<B> {
    inner: B,
    tap: UsageTap,
    ticket: Arc<AccountSpendTicket>,
    finished: bool,
}

impl<B> SpendBody<B> {
    /// Wrap a body so the usage it carries settles `ticket`.
    pub fn new(inner: B, ticket: Arc<AccountSpendTicket>) -> Self {
        Self {
            inner,
            tap: UsageTap::new(),
            ticket,
            finished: false,
        }
    }

    fn publish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        let tap = std::mem::replace(&mut self.tap, UsageTap::new());
        let observed = tap.finish();
        if observed.is_empty() {
            return;
        }
        if let Ok(mut slot) = self.ticket.usage.lock() {
            *slot = Some(observed);
        }
    }
}

impl<B> Drop for SpendBody<B> {
    fn drop(&mut self) {
        self.publish();
        // The ticket settles when *it* is dropped, after this body is gone: it must outlive the
        // last byte, not end with the last frame.
    }
}

impl<B> http_body::Body for SpendBody<B>
where
    B: http_body::Body<Data = bytes::Bytes> + Unpin,
{
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let this = &mut *self;
        match std::pin::Pin::new(&mut this.inner).poll_frame(cx) {
            std::task::Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    this.tap.observe_chunk(data);
                }
                std::task::Poll::Ready(Some(Ok(frame)))
            }
            std::task::Poll::Ready(None) => {
                this.publish();
                std::task::Poll::Ready(None)
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Arc<Store> {
        Arc::new(Store::open_in_memory().expect("open"))
    }

    fn account_with_windows(id: &str, seconds: i64, limit: u64) -> AccountPlan {
        AccountPlan {
            id: AccountId::from_raw(id.to_owned()),
            label: None,
            secret_ref: None,
            oauth_ref: None,
            priority: 0,
            weight: 0,
            max_concurrent: 0,
            models_allow: Vec::new(),
            models_deny: Vec::new(),
            proxy: None,
            placeholders: Default::default(),
            credit_cents: None,
            quota: vec![mr_core::model::QuotaWindow::requests_per(seconds, limit)],
        }
    }

    #[test]
    fn a_full_declared_window_filters_the_account_out() {
        let store = store();
        let account = account_with_windows("a", 60, 2);
        let health = Arc::new(Health::new());
        assert!(exhaustion_for(&store, &account, 10_000).is_none());
        let _ = record_dial(&store, "prov", &account.id, 9_000);
        let _ = record_dial(&store, "prov", &account.id, 9_500);
        let hit = exhaustion_for(&store, &account, 10_000).expect("full");
        assert_eq!(hit.reset_at_ms, 69_000);
        let other = account_with_windows("b", 60, 100);
        let kept = filter_spent_candidates(vec![&account, &other], &store, 10_000);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].id.as_str(), "b");
        assert!(health.cooling_count() == 0);
    }

    #[test]
    fn an_unsettled_dial_still_counts_against_a_request_budget() {
        // The ticket exists to settle tokens; the *request* is already in the ledger from the
        // dial. Dropping it without any observed usage must not erase that fact.
        let store = store();
        let account = account_with_windows("a", 60, 1);
        let health = Arc::new(Health::new());
        let ticket = AccountSpendTicket::open(
            Arc::clone(&store),
            Arc::clone(&health),
            "prov",
            &account,
            10_000,
        )
        .expect("ticket");
        // No body ever ran, so no usage was published. Dropping settles with unknown tokens.
        drop(ticket);
        let hit =
            exhaustion_for(&store, &account, 10_001).expect("one dial fills a 1-request window");
        assert_eq!(hit.reset_at_ms, 70_000);
        // The park is recorded so the reset-aware view can show when the account recovers.
        let view = health.view();
        assert!(view.contains_key("a"));
    }
}
