//! Proactive and forced credential refresh: single-flight admission, failure classification,
//! and vault persistence ordering.
//!
//! The three rules this file exists to enforce:
//!
//! 1. **One refresh per credential, ever in flight at a time.** Refresh tokens are frequently
//!    *single-use*: if two callers refresh concurrently, the second succeeds only because the
//!    first's rotation has already landed — and the winner's response invalidates the loser's
//!    stored token, leaving one half of the pair dead. Depending on the provider that either
//!    bricks the account until re-login or silently oscillates between two identities. The
//!    single-flight slot below makes concurrent callers await the in-flight refresh's result
//!    instead of starting a second one — including when the caller forced it after a 401.
//! 2. **Persist before reporting success.** The new tokens are written to the vault (and
//!    flushed) *before* `ensure_fresh` returns them: a success the store never heard about
//!    would show a green panel and a token set that dies with the next restart.
//! 3. **Classify, don't guess.** A transient failure (5xx, 429, network) keeps the stored
//!    tokens and imposes a short cooldown; a permanent one (`invalid_grant`, `invalid_client`,
//!    HTTP 401 — the refresh token is dead or was reused) hands the account to
//!    [`AccountState::AuthFailed`] with a long cooldown and a "log in again" message.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex as StdMutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::dialect::AccountState;
use crate::error::{Error, Result};
use crate::ids::Millis;
use crate::secret::Vault;

use super::{OAuthPlatform, TokenSet};

/// How long a failed refresh stops further attempts before the cooldown expires.
///
/// Short enough that a provider hiccup recovers within a human's attention span, long enough
/// that a rate-limited endpoint is not hammered by the panel's refresh-on-every-request.
pub const TRANSIENT_COOLDOWN: Duration = Duration::from_secs(30);

/// Cooldown after a permanent failure: the credential cannot work until a human re-authenticates
/// (or the account is replaced), so retrying more often than this only generates traffic.
pub const PERMANENT_COOLDOWN: Duration = Duration::from_secs(3600);

/// Window in which a *forced* refresh accepts the previous rotation's result instead of
/// starting another one. A forced call exists to repair an observed 401 — but if a rotation
/// completed moments ago, that 401 came from the token that has just been replaced, and
/// rotating again would burn a single-use refresh token for nothing.
pub const FORCE_DEDUP_WINDOW: Duration = Duration::from_secs(10);

/// Which kind of refresh failure this is — the decision everything else hangs off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// The tokens themselves are probably still good; the *attempt* failed. Retry after a
    /// short cooldown with the stored credential untouched.
    Transient,
    /// The stored refresh token can never work again (expired, revoked, already used, or the
    /// client itself was rejected). The account needs a new login.
    Permanent,
}

impl FailureClass {
    /// `true` for [`FailureClass::Permanent`].
    pub const fn is_permanent(self) -> bool {
        matches!(self, Self::Permanent)
    }
}

impl fmt::Display for FailureClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Transient => "transient",
            Self::Permanent => "permanent",
        })
    }
}

/// A refresh that did not happen, with enough structure for the panel to word it correctly.
///
/// This is deliberately *not* a plain `Error`: the caller (the conductor) has to choose between
/// "backing off, still signed in" and "this account needs a new login", and re-parsing an error
/// string for that decision would be a bug waiting to be reintroduced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshFailure {
    /// Retry now-and-later, or give up until a human intervenes.
    pub class: FailureClass,
    /// Which platform's refresh endpoint failed.
    pub platform: &'static str,
    /// Credential-free explanation. Permanent messages end with the panel's wording: the
    /// operator must be told to log in again.
    pub message: String,
}

impl RefreshFailure {
    /// The account state this failure asks the conductor to apply.
    ///
    /// Transient → `Backoff` (short cooldown, still signed in); permanent → `AuthFailed`
    /// ("log in again" in the panel).
    pub const fn account_state(&self) -> AccountState {
        match self.class {
            FailureClass::Transient => AccountState::Backoff,
            FailureClass::Permanent => AccountState::AuthFailed,
        }
    }
}

impl fmt::Display for RefreshFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "oauth {} refresh failed ({}): {}",
            self.platform, self.class, self.message
        )
    }
}

impl std::error::Error for RefreshFailure {}

/// Build a classified failure as the crate's `Error`.
///
/// # Why the class travels in the message
///
/// `crate::error::Error` is a fixed enum this module may not extend, and the platform trait's
/// `refresh` returns it. The classification therefore rides along as a machine-readable marker
/// at the head of an `InvalidState` message — `oauth[permanent] …` / `oauth[transient] …` —
/// decoded by [`classify`] at the single point where it matters. The marker doubles as the
/// first thing an operator reads, and permanent messages end with the required
/// "log in again".
pub(crate) fn oauth_failure(
    class: FailureClass,
    platform: &str,
    detail: impl fmt::Display,
) -> Error {
    let text = match class {
        FailureClass::Permanent => format!("oauth[permanent] {platform}: {detail}; log in again"),
        FailureClass::Transient => format!("oauth[transient] {platform}: {detail}"),
    };
    Error::InvalidState(text)
}

/// Decode the class a platform stamped onto an [`Error`].
///
/// Everything unmarked is transient — transport failures, JSON decode failures and flow errors
/// all leave the stored tokens intact, and treating them as "the token is dead" would sign an
/// account out over a provider 500.
pub(crate) fn classify(error: &Error) -> FailureClass {
    match error {
        Error::InvalidState(message) if message.starts_with("oauth[permanent]") => {
            FailureClass::Permanent
        }
        _ => FailureClass::Transient,
    }
}

/// Classify a token-endpoint failure from its HTTP status and its OAuth error code, if the
/// body carried one.
///
/// The code wins over the status because providers answer with 200 + `{"error": …}` as often as
/// with a real 4xx, and the *code* is what says whether the credential can ever work again:
///
/// * `invalid_grant` / `invalid_client` (and the device flow's `access_denied`) — permanent:
///   the refresh token expired, was revoked, was already used, or the client itself was
///   rejected. Re-login is the only remedy. (Codex's manager draws the same line: invalid
///   grant on 400, and any 401, are permanent; everything else is retryable.)
/// * HTTP 401 without a code — permanent, same reason: the presented credential was refused.
/// * 408 / 429 / 5xx / network / anything unknown — transient: the tokens are untouched.
/// * HTTP 400 with an *unknown* code — transient, deliberately: Codex treats only
///   `invalid_grant` on 400 as fatal, and signing an account out over an unrecognised code
///   would be worse than retrying it for 30 seconds.
pub fn classify_token_endpoint(status: u16, oauth_error_code: Option<&str>) -> FailureClass {
    match oauth_error_code {
        Some("invalid_grant") | Some("invalid_client") | Some("unauthorized_client")
        | Some("access_denied")
        // RFC 6749 §5.2 and GitHub's documented spellings of the same permanent conditions:
        // a wrong grant type or wrong client credentials cannot start working on their own,
        // and a dead device code is dead. Retrying them would burn single-use state.
        | Some("unsupported_grant_type")
        | Some("incorrect_client_credentials")
        | Some("incorrect_device_code") => return FailureClass::Permanent,
        // Codes that exist to say "ask again later" — the flow's own vocabulary.
        Some("authorization_pending") | Some("slow_down") | Some("temporarily_unavailable")
        | Some("server_error") | Some("rate_limit_exceeded") => {
            return FailureClass::Transient
        }
        // Unknown code: fall through to the status.
        Some(_) | None => {}
    }
    if status == 401 {
        return FailureClass::Permanent;
    }
    // 408, 429, 5xx, plain 400 without an identifiable fatal code, 0 (never got a status).
    FailureClass::Transient
}

/// Pull `error` out of a JSON token-endpoint body.
///
/// Tolerant on purpose: bodies that are not JSON, or that carry no `error` field, yield `None`
/// and the caller classifies on status alone. The value is a protocol enum, never echo
/// material, so it is safe to put into a message later.
pub fn oauth_error_code(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    value.get("error")?.as_str().map(str::to_owned)
}

/// Truncate a provider-supplied message so one hostile `error_description` cannot flood the
/// panel or the log (same 200-char ceiling the callback pages use).
fn clip(text: &str) -> &str {
    if text.len() <= 200 {
        return text;
    }
    let mut end = 200;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Inspect a token-endpoint reply: `None` means success, `Some` carries the classified
/// failure. Shared by every platform that speaks OAuth's JSON error shape.
///
/// * A JSON `error` field decides even on HTTP 200 — providers answer both ways —
/// * the OAuth error code, not the bare status, decides whether the credential is dead
///   (`classify_token_endpoint`),
/// * the whole message is clipped, so a hostile `error_description` cannot flood the panel
///   by splitting itself across fields.
pub(crate) fn endpoint_failure(status: u16, body: &str) -> Option<(FailureClass, String)> {
    let parsed: Option<serde_json::Value> = serde_json::from_str(body).ok();
    let code = parsed
        .as_ref()
        .and_then(|value| value.get("error"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let description = parsed
        .as_ref()
        .and_then(|value| value.get("error_description"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let success = (200..300).contains(&status);
    if success && code.is_none() {
        return None;
    }
    let class = classify_token_endpoint(status, code.as_deref());
    let detail = match (code, description) {
        (Some(code), Some(description)) => format!("{code}: {}", clip(&description)),
        (Some(code), None) => code,
        (None, _) => format!("HTTP {status}"),
    };
    Some((class, clip(&detail).to_string()))
}

/// Whether `tokens` need a proactive refresh at `now`, given a platform's refresh lead.
///
/// `expires_at - now < lead` — strictly, so a token that expires *exactly* at the lead boundary
/// is not yet worth a round trip. Already-expired tokens report a remaining time of zero,
/// which is always `< lead`, so they are always due.
pub fn is_due(tokens: &TokenSet, now: Millis, lead: Duration) -> bool {
    let lead_ms = i64::try_from(lead.as_millis()).unwrap_or(i64::MAX);
    tokens.expires_at.remaining_ms_until(now) < lead_ms
}

/// Durable storage for OAuth credentials.
///
/// The token material is durable in the vault — encryption at rest, a 0600 key, atomic flush — while
/// the non-secret metadata lives in the `oauth_credential` table of [`crate::store`]. Implementations
/// must make `save` atomic from the caller's view: either the new tokens are durable or the old ones
/// are still there, never a torn state.
pub trait CredentialStore: Send + Sync {
    /// Load the credential under `key`, if one exists.
    fn load(&self, key: &str) -> Result<Option<TokenSet>>;
    /// Persist `tokens` under `key` and flush to disk before returning.
    fn save(&self, key: &str, tokens: &TokenSet) -> Result<()>;
}

/// The [`Vault`]-backed store. The vault needs `&mut` for writes, so it sits behind a mutex;
/// the single-flight admission in [`Refresher`] serialises writes per credential anyway.
pub struct VaultStore {
    vault: StdMutex<Vault>,
}

impl VaultStore {
    /// Wrap an already-open vault.
    pub fn new(vault: Vault) -> Self {
        Self {
            vault: StdMutex::new(vault),
        }
    }

    /// Open the default vault location.
    pub fn open_default() -> Result<Self> {
        Ok(Self::new(Vault::open_default()?))
    }
}

impl CredentialStore for VaultStore {
    fn load(&self, key: &str) -> Result<Option<TokenSet>> {
        // A poisoned mutex means *some* holder panicked while holding it; the vault data
        // itself is unaffected (writes are atomic temp+rename), so recovering is safe — and
        // panicking here would abort the process anyway (`panic = "abort"`).
        let mut vault = lock_std(&self.vault);
        vault.reload_from_disk()?;
        match vault.get(key)? {
            None => Ok(None),
            Some(raw) => {
                let tokens: TokenSet = serde_json::from_str(&raw)?;
                Ok(Some(tokens))
            }
        }
    }

    fn save(&self, key: &str, tokens: &TokenSet) -> Result<()> {
        let mut vault = lock_std(&self.vault);
        let encoded = serde_json::to_string(tokens)?;
        vault.put(key, &encoded)?;
        // Flush *now*: a success reported on an unflushed vault dies with the process, and
        // for a single-use refresh token that restart would strand the account.
        vault.flush()?;
        Ok(())
    }
}

/// `Mutex::lock` without `unwrap`: poisoning only means another thread panicked mid-critical-
/// section, and with `panic = "abort"` there are no other threads left to worry about — the
/// data behind it is what matters.
fn lock_std<T>(mutex: &StdMutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
struct Cooldown {
    until: Millis,
    class: FailureClass,
    message: String,
}

/// Refreshes credentials on behalf of the relay: proactive (before expiry), forced (after a
/// 401), single-flight, classified, persisted-before-success.
///
/// One `Refresher` serves one platform — the platform is fixed at construction so that
/// [`ensure_fresh`](Refresher::ensure_fresh) can keep the brief's `(credential_id, force)`
/// signature while still knowing whose endpoint to call.
pub struct Refresher {
    platform: &'static dyn OAuthPlatform,
    store: Arc<dyn CredentialStore>,
    now: fn() -> Millis,
    /// One `tokio::sync::Mutex` per credential: waiters queue here instead of issuing a
    /// second refresh.
    slots: StdMutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// How many refreshes have completed for a credential. A caller records the generation
    /// *before* queueing; if it moved by the time the slot is acquired, someone else already
    /// produced fresh tokens and this caller returns those — the mechanism that collapses N
    /// concurrent forced calls into one network refresh.
    generations: StdMutex<HashMap<String, u64>>,
    /// When the last successful refresh landed, for [`FORCE_DEDUP_WINDOW`]'s check.
    last_refreshed: StdMutex<HashMap<String, Millis>>,
    cooldowns: StdMutex<HashMap<String, Cooldown>>,
}

impl Refresher {
    /// Build a refresher using the system clock.
    pub fn new(platform: &'static dyn OAuthPlatform, store: Arc<dyn CredentialStore>) -> Self {
        Self::with_clock(platform, store, Millis::now)
    }

    /// Build a refresher with an injected clock — the refresh-lead and cooldown arithmetic is
    /// tested against fixed instants instead of wall time.
    pub fn with_clock(
        platform: &'static dyn OAuthPlatform,
        store: Arc<dyn CredentialStore>,
        now: fn() -> Millis,
    ) -> Self {
        Self {
            platform,
            store,
            now,
            slots: StdMutex::new(HashMap::new()),
            generations: StdMutex::new(HashMap::new()),
            last_refreshed: StdMutex::new(HashMap::new()),
            cooldowns: StdMutex::new(HashMap::new()),
        }
    }

    /// The platform this refresher serves.
    pub fn platform(&self) -> &'static dyn OAuthPlatform {
        self.platform
    }

    /// Current generation for a credential (0 when never refreshed).
    fn generation(&self, credential: &str) -> u64 {
        *lock_std(&self.generations).get(credential).unwrap_or(&0)
    }

    fn bump_generation(&self, credential: &str) {
        let mut generations = lock_std(&self.generations);
        *generations.entry(credential.to_owned()).or_insert(0) += 1;
    }

    fn slot_for(&self, credential: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut slots = lock_std(&self.slots);
        slots
            .entry(credential.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    fn cooldown(&self, credential: &str) -> Option<Cooldown> {
        lock_std(&self.cooldowns).get(credential).cloned()
    }

    /// Did a successful refresh land for this credential within the dedup window?
    fn recently_refreshed(&self, credential: &str, now: Millis) -> bool {
        let last = lock_std(&self.last_refreshed).get(credential).copied();
        match last {
            None => false,
            Some(at) => now.is_before(
                at.plus_millis(i64::try_from(FORCE_DEDUP_WINDOW.as_millis()).unwrap_or(i64::MAX)),
            ),
        }
    }

    fn remember_failure(&self, credential: &str, failure: &RefreshFailure, now: Millis) {
        let wait = match failure.class {
            FailureClass::Transient => TRANSIENT_COOLDOWN,
            FailureClass::Permanent => PERMANENT_COOLDOWN,
        };
        lock_std(&self.cooldowns).insert(
            credential.to_owned(),
            Cooldown {
                until: now.plus_millis(i64::try_from(wait.as_millis()).unwrap_or(i64::MAX)),
                class: failure.class,
                message: failure.message.clone(),
            },
        );
    }

    /// Make sure `credential`'s access token is usable at the next request, refreshing it when
    /// it is due, and always when `force` is set.
    ///
    /// * **Proactive:** refresh when `expires_at - now < platform.refresh_lead()`.
    /// * **Forced (401 repair):** refresh even when the token looks fresh — an observed 401
    ///   outranks our local clock — and bypass cooldowns, because the upstream's refusal is
    ///   better evidence than our classification. Still single-flight: a forced call queued
    ///   behind someone else's just-completed refresh returns *that* result rather than
    ///   starting a second rotation.
    /// * **Classified:** returns [`RefreshFailure`] rather than a bare error so the caller can
    ///   distinguish "back off" from "log in again" without parsing strings.
    pub async fn ensure_fresh(
        &self,
        credential: &str,
        force: bool,
    ) -> std::result::Result<TokenSet, RefreshFailure> {
        let generation_seen = self.generation(credential);
        let slot = self.slot_for(credential);
        let _in_flight = slot.lock().await;

        let failure = |message: String| RefreshFailure {
            class: FailureClass::Transient,
            platform: self.platform.id(),
            message,
        };

        let tokens = self
            .store
            .load(credential)
            .map_err(|e| failure(format!("stored credential could not be read: {e}")))?
            .ok_or_else(|| failure(format!("no credential is stored under {credential}")))?;

        // Someone else refreshed while we waited on the slot: their persisted result is the
        // answer, whatever we were about to do. This is what makes N concurrent callers
        // (force or not) collapse into one network refresh.
        if self.generation(credential) > generation_seen {
            return Ok(tokens);
        }

        let now = (self.now)();
        let due = is_due(&tokens, now, self.platform.refresh_lead());
        if !force && !due {
            return Ok(tokens);
        }
        // Forced and not due, but a rotation just landed: the 401 that forced this call came
        // from the token that rotation already replaced — accepting its result rather than
        // burning another single-use refresh token.
        if force && !due && self.recently_refreshed(credential, now) {
            return Ok(tokens);
        }

        // Cooldowns shape *non-forced* attempts; `force` carries fresh evidence (an observed
        // 401) that outranks our own classification — except for a permanent failure on a
        // credential that is still due: that refresh token is provably dead, and re-presenting
        // it on every 401 would burn attempts until a human logs in again. (Tokens that
        // suddenly look *fresh* next to a stale cooldown mean a re-login landed; the fall-
        // through below lets a forced rotation proceed instead of serving a cached verdict.)
        if let Some(cooldown) = self.cooldown(credential) {
            if now.is_before(cooldown.until) {
                match cooldown.class {
                    FailureClass::Permanent if due => {
                        return Err(RefreshFailure {
                            class: FailureClass::Permanent,
                            platform: self.platform.id(),
                            message: cooldown.message,
                        });
                    }
                    FailureClass::Transient if !force => {
                        // Tokens are intact and the endpoint is (recently) unhappy: hand back
                        // what we have and let the cooldown run its course.
                        return Ok(tokens);
                    }
                    _ => {}
                }
            }
        }

        let Some(refresh_token) = tokens.refresh_token.clone() else {
            // Nothing to refresh with: this credential can only ever be replaced by a login.
            let refresh_failure = RefreshFailure {
                class: FailureClass::Permanent,
                platform: self.platform.id(),
                message: format!("credential {credential} has no refresh token; log in again"),
            };
            self.remember_failure(credential, &refresh_failure, now);
            return Err(refresh_failure);
        };

        let refreshed = match self
            .platform
            .refresh_with_context(&refresh_token, &tokens)
            .await
        {
            Ok(fresh) => fresh,

            Err(error) => {
                let refresh_failure = RefreshFailure {
                    class: classify(&error),
                    platform: self.platform.id(),
                    message: error.to_string(),
                };
                self.remember_failure(credential, &refresh_failure, (self.now)());
                return Err(refresh_failure);
            }
        };

        // Ordering trap: persist BEFORE reporting success. A single-use refresh token has just
        // been spent — if the write fails and we returned these tokens anyway, the panel would
        // show green while the stored credential (the only one that survives a restart) holds
        // a token that no longer works. We report the save failure instead, classified
        // transient: for a provider with reusable refresh tokens the next attempt recovers,
        // and for a single-use one the next attempt lands on `invalid_grant` and the account
        // moves to `AuthFailed` — both honest, neither a silent lie.
        if let Err(error) = self.store.save(credential, &refreshed) {
            let refresh_failure = RefreshFailure {
                class: FailureClass::Transient,
                platform: self.platform.id(),
                message: format!(
                    "refresh succeeded but the credential could not be saved ({error}); \
                     the stored token set may now be stale — log in again if this repeats"
                ),
            };
            self.remember_failure(credential, &refresh_failure, (self.now)());
            return Err(refresh_failure);
        }

        self.bump_generation(credential);
        lock_std(&self.last_refreshed).insert(credential.to_owned(), (self.now)());
        lock_std(&self.cooldowns).remove(credential);
        Ok(refreshed)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::sync::Arc;

    use async_trait::async_trait;

    use super::*;
    use crate::oauth::{AccountIdentity, Pkce};

    const T0: i64 = 1_700_000_000_000;
    const CREDENTIAL: &str = "cred-test-1";

    thread_local! {
        static FAKE_NOW: Cell<i64> = const { Cell::new(T0) };
    }

    fn set_now(ms: i64) {
        FAKE_NOW.with(|cell| cell.set(ms));
    }

    fn fake_now() -> Millis {
        FAKE_NOW.with(|cell| Millis(cell.get()))
    }

    fn stored_tokens(expires_at: Millis) -> TokenSet {
        let mut tokens = TokenSet::new("old-access");
        tokens.refresh_token = Some("old-refresh".into());
        tokens.expires_at = expires_at;
        tokens
    }

    // ----- doubles ---------------------------------------------------------

    #[derive(Default)]
    struct FakeStore {
        data: StdMutex<HashMap<String, TokenSet>>,
        events: StdMutex<Vec<String>>,
        fail_save: AtomicBool,
        saves: AtomicUsize,
    }

    impl FakeStore {
        fn with_tokens(tokens: TokenSet) -> Arc<Self> {
            let store = Arc::new(Self::default());
            store
                .data
                .lock()
                .unwrap()
                .insert(CREDENTIAL.to_owned(), tokens);
            store
        }

        fn events(&self) -> Vec<String> {
            self.events.lock().unwrap().clone()
        }

        fn get(&self) -> Option<TokenSet> {
            self.data.lock().unwrap().get(CREDENTIAL).cloned()
        }
    }

    impl CredentialStore for FakeStore {
        fn load(&self, key: &str) -> Result<Option<TokenSet>> {
            self.events.lock().unwrap().push(format!("load:{key}"));
            Ok(self.data.lock().unwrap().get(key).cloned())
        }

        fn save(&self, key: &str, tokens: &TokenSet) -> Result<()> {
            self.events.lock().unwrap().push(format!("save:{key}"));
            self.saves.fetch_add(1, Ordering::SeqCst);
            if self.fail_save.load(Ordering::SeqCst) {
                // Fail *before* writing: the old credential must survive intact.
                return Err(Error::Io(std::io::Error::other("disk full (test)")));
            }
            self.data
                .lock()
                .unwrap()
                .insert(key.to_owned(), tokens.clone());
            Ok(())
        }
    }

    enum Next {
        Tokens(TokenSet),
        Fail {
            class: FailureClass,
            message: String,
        },
    }

    struct FakePlatform {
        refreshes: AtomicUsize,
        delay_ms: AtomicU64,
        lead_secs: AtomicU64,
        script: StdMutex<VecDeque<Next>>,
    }

    impl FakePlatform {
        fn leaked(lead: Duration, script: Vec<Next>) -> &'static Self {
            Box::leak(Box::new(Self {
                refreshes: AtomicUsize::new(0),
                delay_ms: AtomicU64::new(0),
                lead_secs: AtomicU64::new(lead.as_secs()),
                script: StdMutex::new(script.into()),
            }))
        }

        fn refreshes(&self) -> usize {
            self.refreshes.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl OAuthPlatform for FakePlatform {
        fn id(&self) -> &'static str {
            "fake-refresh"
        }
        fn label(&self) -> &'static str {
            "Fake refresh platform"
        }
        fn description(&self) -> &'static str {
            "Test double with a scripted refresh endpoint."
        }
        fn begins_with_loopback(&self) -> bool {
            true
        }
        fn redirect_ports(&self) -> crate::oauth::RedirectPorts {
            crate::oauth::RedirectPorts {
                host: "127.0.0.1",
                primary: 0,
                fallback: None,
                path: "/cb",
            }
        }
        fn requires_pkce(&self) -> bool {
            true
        }
        fn validates_redirect_port(&self) -> bool {
            false
        }
        fn authorization_url(
            &self,
            state: &str,
            _pkce: Option<&Pkce>,
            _port: u16,
        ) -> Result<String> {
            Ok(format!("https://example.test/authorize?state={state}"))
        }
        async fn exchange_code(
            &self,
            _code: &str,
            _pkce: Option<&Pkce>,
            _port: u16,
        ) -> Result<TokenSet> {
            Ok(TokenSet::new("exchanged"))
        }
        async fn refresh(&self, refresh_token: &str) -> Result<TokenSet> {
            assert!(
                !refresh_token.is_empty(),
                "refresh must present the stored token"
            );
            self.refreshes.fetch_add(1, Ordering::SeqCst);
            let delay = self.delay_ms.load(Ordering::SeqCst);
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            match self.script.lock().unwrap().pop_front() {
                Some(Next::Tokens(tokens)) => Ok(tokens),
                Some(Next::Fail { class, message }) => {
                    Err(oauth_failure(class, self.id(), message))
                }
                None => Err(oauth_failure(
                    FailureClass::Transient,
                    self.id(),
                    "script exhausted",
                )),
            }
        }
        async fn account_identity(&self, _tokens: &TokenSet) -> Result<AccountIdentity> {
            Ok(AccountIdentity {
                id: "fake".into(),
                label: "Fake".into(),
                email: None,
                plan: None,
            })
        }
        fn apply_credential(
            &self,
            _tokens: &TokenSet,
        ) -> Vec<(http::HeaderName, http::HeaderValue)> {
            Vec::new()
        }
        fn fingerprint_headers(&self) -> &'static [(&'static str, &'static str)] {
            &[("x-fake", "1")]
        }
        fn refresh_lead(&self) -> Duration {
            Duration::from_secs(self.lead_secs.load(Ordering::SeqCst))
        }
    }

    fn fresh_tokens() -> TokenSet {
        stored_tokens(Millis(T0).plus_secs(7200))
    }

    fn due_tokens() -> TokenSet {
        // Expires in 4 minutes: inside the fake's 5-minute lead, so refresh is due.
        stored_tokens(Millis(T0).plus_secs(240))
    }

    fn new_tokens() -> TokenSet {
        let mut tokens = TokenSet::new("new-access");
        tokens.refresh_token = Some("new-refresh".into());
        tokens.expires_at = Millis(T0).plus_secs(3600);
        tokens
    }

    // ----- refresh-lead arithmetic (injected clock) ------------------------

    #[test]
    fn due_arithmetic_is_strict_at_the_lead_boundary() {
        let lead = Duration::from_secs(300);
        let now = Millis(T0);
        // Inside the window → due; outside → not.
        assert!(is_due(&stored_tokens(now.plus_secs(299)), now, lead));
        assert!(!is_due(&stored_tokens(now.plus_secs(301)), now, lead));
        // Exactly at the lead: not yet worth a round trip (strict `<`).
        assert!(!is_due(&stored_tokens(now.plus_secs(300)), now, lead));
        // Expired, and the "unknown" zero expiry, are always due.
        assert!(is_due(&stored_tokens(now.plus_secs(-1)), now, lead));
        assert!(is_due(&stored_tokens(Millis(0)), now, lead));
    }

    #[tokio::test]
    async fn fresh_tokens_pass_through_without_a_network_call() {
        set_now(T0);
        let store = FakeStore::with_tokens(fresh_tokens());
        let platform = FakePlatform::leaked(Duration::from_secs(300), vec![]);
        let refresher = Refresher::with_clock(platform, store.clone(), fake_now);

        let tokens = refresher
            .ensure_fresh(CREDENTIAL, false)
            .await
            .expect("fresh");
        assert_eq!(tokens.access_token, "old-access");
        assert_eq!(platform.refreshes(), 0, "no refresh for a fresh token");
        assert_eq!(store.saves.load(Ordering::SeqCst), 0, "nothing to persist");
    }

    #[tokio::test]
    async fn due_tokens_are_refreshed_and_persisted_before_success() {
        set_now(T0);
        let store = FakeStore::with_tokens(due_tokens());
        let platform =
            FakePlatform::leaked(Duration::from_secs(300), vec![Next::Tokens(new_tokens())]);
        let refresher = Refresher::with_clock(platform, store.clone(), fake_now);

        let tokens = refresher
            .ensure_fresh(CREDENTIAL, false)
            .await
            .expect("refresh");
        assert_eq!(tokens.access_token, "new-access");
        assert_eq!(platform.refreshes(), 1);
        // The store saw the new tokens before `ensure_fresh` returned: success and durable
        // state agree (the save-failure test below proves the other side of the ordering).
        assert_eq!(
            store.events(),
            vec![
                "load:cred-test-1".to_string(),
                "save:cred-test-1".to_string()
            ]
        );
        assert_eq!(
            store.get().map(|t| t.access_token),
            Some("new-access".into())
        );
    }

    #[tokio::test]
    async fn save_failure_is_reported_and_the_old_credential_survives() {
        set_now(T0);
        let store = FakeStore::with_tokens(due_tokens());
        store.fail_save.store(true, Ordering::SeqCst);
        let platform =
            FakePlatform::leaked(Duration::from_secs(300), vec![Next::Tokens(new_tokens())]);
        let refresher = Refresher::with_clock(platform, store.clone(), fake_now);

        let failure = refresher
            .ensure_fresh(CREDENTIAL, false)
            .await
            .expect_err("unsaved success must not be reported as success");
        assert_eq!(failure.class, FailureClass::Transient);
        assert!(
            failure.message.contains("could not be saved"),
            "{}",
            failure.message
        );
        // Ordering trap: the old credential is still what is stored — no torn state.
        let stored = store.get().expect("credential still present");
        assert_eq!(stored.access_token, "old-access");
        assert_eq!(stored.refresh_token.as_deref(), Some("old-refresh"));
        assert_eq!(platform.refreshes(), 1);
    }

    // ----- classification and cooldowns ------------------------------------

    #[tokio::test]
    async fn permanent_failure_marks_auth_failed_and_its_cooldown_is_reused() {
        set_now(T0);
        let store = FakeStore::with_tokens(due_tokens());
        let platform = FakePlatform::leaked(
            Duration::from_secs(300),
            vec![Next::Fail {
                class: FailureClass::Permanent,
                message: "invalid_grant: refresh token has been used".into(),
            }],
        );
        let refresher = Refresher::with_clock(platform, store.clone(), fake_now);

        let failure = refresher
            .ensure_fresh(CREDENTIAL, false)
            .await
            .expect_err("permanent failure");
        assert_eq!(failure.class, FailureClass::Permanent);
        assert_eq!(failure.account_state(), AccountState::AuthFailed);
        assert!(
            failure.message.contains("log in again"),
            "panel wording travels with the failure: {}",
            failure.message
        );

        // Within the long cooldown: same verdict, no second attempt — not even forced, since
        // re-presenting a provably dead refresh token cannot help.
        for force in [false, true] {
            let again = refresher
                .ensure_fresh(CREDENTIAL, force)
                .await
                .expect_err("still permanent");
            assert_eq!(again.class, FailureClass::Permanent);
        }
        assert_eq!(
            platform.refreshes(),
            1,
            "cooldown prevented a second refresh"
        );
        // Tokens untouched throughout.
        assert_eq!(
            store.get().map(|t| t.access_token),
            Some("old-access".into())
        );
    }

    #[tokio::test]
    async fn transient_failure_keeps_tokens_short_cooldown_then_recovers() {
        set_now(T0);
        let store = FakeStore::with_tokens(due_tokens());
        let platform = FakePlatform::leaked(
            Duration::from_secs(300),
            vec![
                Next::Fail {
                    class: FailureClass::Transient,
                    message: "502 from token endpoint".into(),
                },
                Next::Tokens(new_tokens()),
            ],
        );
        let refresher = Refresher::with_clock(platform, store.clone(), fake_now);

        let failure = refresher
            .ensure_fresh(CREDENTIAL, false)
            .await
            .expect_err("transient failure");
        assert_eq!(failure.class, FailureClass::Transient);
        assert_eq!(failure.account_state(), AccountState::Backoff);

        // Inside the cooldown: the stored tokens are handed back untouched — still signed in.
        let tokens = refresher
            .ensure_fresh(CREDENTIAL, false)
            .await
            .expect("intact");
        assert_eq!(tokens.access_token, "old-access");
        assert_eq!(platform.refreshes(), 1, "cooldown suppressed the retry");

        // After the cooldown the next attempt goes through and succeeds.
        set_now(T0 + TRANSIENT_COOLDOWN.as_millis() as i64 + 1);
        let tokens = refresher
            .ensure_fresh(CREDENTIAL, false)
            .await
            .expect("recovered");
        assert_eq!(tokens.access_token, "new-access");
        assert_eq!(platform.refreshes(), 2);
        assert_eq!(
            store.get().map(|t| t.access_token),
            Some("new-access".into())
        );
    }

    #[tokio::test]
    async fn forced_refresh_bypasses_freshness_and_the_transient_cooldown() {
        set_now(T0);
        let store = FakeStore::with_tokens(fresh_tokens());
        let platform = FakePlatform::leaked(
            Duration::from_secs(300),
            vec![
                Next::Tokens(new_tokens()),
                Next::Fail {
                    class: FailureClass::Transient,
                    message: "flaky".into(),
                },
                Next::Tokens(new_tokens()),
            ],
        );
        let refresher = Refresher::with_clock(platform, store.clone(), fake_now);

        // A 401 outranks the local clock: force refreshes even though the token looks fresh.
        let tokens = refresher
            .ensure_fresh(CREDENTIAL, true)
            .await
            .expect("forced");
        assert_eq!(tokens.access_token, "new-access");
        assert_eq!(platform.refreshes(), 1);

        // Force again → transient failure → cooldown recorded …
        let mut stale = due_tokens();
        stale.refresh_token = Some("new-refresh".into());
        store
            .data
            .lock()
            .unwrap()
            .insert(CREDENTIAL.to_owned(), stale);
        let failure = refresher
            .ensure_fresh(CREDENTIAL, true)
            .await
            .expect_err("second forced attempt fails");
        assert_eq!(failure.class, FailureClass::Transient);

        // … yet a *forced* call still goes out: the caller's evidence outweighs our guess.
        set_now(T0);
        let _ = refresher
            .ensure_fresh(CREDENTIAL, true)
            .await
            .expect("third");
        assert_eq!(
            platform.refreshes(),
            3,
            "force ignored the transient cooldown"
        );
    }

    #[tokio::test]
    async fn a_credential_without_a_refresh_token_is_a_permanent_login_request() {
        set_now(T0);
        let mut tokens = due_tokens();
        tokens.refresh_token = None;
        let store = FakeStore::with_tokens(tokens);
        let platform = FakePlatform::leaked(Duration::from_secs(300), vec![]);
        let refresher = Refresher::with_clock(platform, store.clone(), fake_now);

        let failure = refresher
            .ensure_fresh(CREDENTIAL, false)
            .await
            .expect_err("nothing to refresh with");
        assert_eq!(failure.class, FailureClass::Permanent);
        assert_eq!(failure.account_state(), AccountState::AuthFailed);
        assert!(
            failure.message.contains("log in again"),
            "{}",
            failure.message
        );
        assert_eq!(platform.refreshes(), 0, "never called the endpoint");
        // Recalled on the next attempt rather than re-decided.
        let again = refresher
            .ensure_fresh(CREDENTIAL, false)
            .await
            .expect_err("cached");
        assert_eq!(again.message, failure.message);
        assert_eq!(platform.refreshes(), 0);
    }

    #[tokio::test]
    async fn a_missing_credential_says_so() {
        set_now(T0);
        let store = FakeStore::default();
        let platform = FakePlatform::leaked(Duration::from_secs(300), vec![]);
        let refresher = Refresher::with_clock(platform, Arc::new(store), fake_now);
        let failure = refresher
            .ensure_fresh(CREDENTIAL, false)
            .await
            .expect_err("nothing stored");
        assert!(
            failure.message.contains("no credential"),
            "{}",
            failure.message
        );
        assert_eq!(platform.refreshes(), 0);
    }

    // ----- single-flight ---------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_forced_callers_collapse_into_one_refresh() {
        set_now(T0);
        let store = FakeStore::with_tokens(due_tokens());
        let platform = FakePlatform::leaked(
            Duration::from_secs(300),
            vec![Next::Tokens({
                let mut t = new_tokens();
                t.access_token = "rotated".into();
                t
            })],
        );
        platform.delay_ms.store(80, Ordering::SeqCst);
        let refresher = Arc::new(Refresher::with_clock(platform, store.clone(), fake_now));

        let mut handles = Vec::new();
        for _ in 0..8 {
            let refresher = Arc::clone(&refresher);
            handles.push(tokio::spawn(async move {
                refresher.ensure_fresh(CREDENTIAL, true).await
            }));
        }
        let mut results = Vec::new();
        for handle in handles {
            results.push(handle.await.expect("join").expect("shared success"));
        }

        assert_eq!(
            platform.refreshes(),
            1,
            "eight concurrent forced callers must share one rotation — a second one would \
             invalidate the first's single-use refresh token"
        );
        assert_eq!(store.saves.load(Ordering::SeqCst), 1);
        for tokens in &results {
            assert_eq!(
                tokens.access_token, "rotated",
                "everyone sees the same result"
            );
        }
        assert_eq!(
            store.get().map(|t| t.access_token),
            Some("rotated".into()),
            "the shared result is the persisted one"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_proactive_callers_also_share_the_refresh() {
        set_now(T0);
        let store = FakeStore::with_tokens(due_tokens());
        let platform =
            FakePlatform::leaked(Duration::from_secs(300), vec![Next::Tokens(new_tokens())]);
        platform.delay_ms.store(50, Ordering::SeqCst);
        let refresher = Arc::new(Refresher::with_clock(platform, store.clone(), fake_now));

        let mut handles = Vec::new();
        for _ in 0..8 {
            let refresher = Arc::clone(&refresher);
            handles.push(tokio::spawn(async move {
                refresher.ensure_fresh(CREDENTIAL, false).await
            }));
        }
        for handle in handles {
            let tokens = handle.await.expect("join").expect("shared success");
            assert_eq!(tokens.access_token, "new-access");
        }
        assert_eq!(platform.refreshes(), 1);
        assert_eq!(store.saves.load(Ordering::SeqCst), 1);
    }

    // ----- classification helpers -----------------------------------------

    #[test]
    fn token_endpoint_classification_matrix() {
        use FailureClass::{Permanent, Transient};
        // The permanent set: the credential can never work again (or the client was rejected).
        assert_eq!(
            classify_token_endpoint(400, Some("invalid_grant")),
            Permanent
        );
        assert_eq!(
            classify_token_endpoint(400, Some("invalid_client")),
            Permanent
        );
        assert_eq!(classify_token_endpoint(401, None), Permanent);
        assert_eq!(
            classify_token_endpoint(403, Some("unauthorized_client")),
            Permanent
        );
        // Providers sometimes answer 200 with an error object — the code still decides.
        assert_eq!(
            classify_token_endpoint(200, Some("invalid_grant")),
            Permanent
        );
        // The transient set: endpoint trouble, credential untouched.
        assert_eq!(classify_token_endpoint(500, None), Transient);
        assert_eq!(classify_token_endpoint(503, None), Transient);
        assert_eq!(classify_token_endpoint(429, None), Transient);
        assert_eq!(classify_token_endpoint(408, None), Transient);
        assert_eq!(classify_token_endpoint(0, None), Transient);
        // Unknown code on a 400 stays retryable — signing an account out over an
        // unrecognised code would be worse than a 30-second retry.
        assert_eq!(classify_token_endpoint(400, Some("weird_code")), Transient);
        assert_eq!(classify_token_endpoint(400, None), Transient);
        // The flow's own vocabulary for "not yet"/"slower".
        assert_eq!(
            classify_token_endpoint(400, Some("authorization_pending")),
            Transient
        );
        assert_eq!(classify_token_endpoint(400, Some("slow_down")), Transient);
    }

    #[test]
    fn oauth_error_code_extraction_tolerates_junk_bodies() {
        assert_eq!(
            oauth_error_code(r#"{"error":"invalid_grant","error_description":"nope"}"#).as_deref(),
            Some("invalid_grant")
        );
        assert_eq!(oauth_error_code("<html>gateway</html>"), None);
        assert_eq!(oauth_error_code(r#"{"access_token":"fine"}"#), None);
        assert_eq!(oauth_error_code(""), None);
        assert_eq!(
            oauth_error_code(r#"{"error":42}"#),
            None,
            "non-string error"
        );
    }

    #[test]
    fn failure_marker_round_trips_through_the_crate_error() {
        let permanent = oauth_failure(FailureClass::Permanent, "openai-codex", "invalid_grant");
        assert_eq!(classify(&permanent), FailureClass::Permanent);
        assert!(
            permanent.to_string().contains("log in again"),
            "the panel's wording is part of the message: {permanent}"
        );

        let transient = oauth_failure(FailureClass::Transient, "openai-codex", "502");
        assert_eq!(classify(&transient), FailureClass::Transient);

        // Unmarked failures default to transient: a 500 or a JSON hiccup must never be read
        // as "this credential is dead".
        assert_eq!(
            classify(&Error::Io(std::io::Error::other("reset"))),
            FailureClass::Transient
        );
        assert_eq!(
            classify(&Error::Json(
                serde_json::from_str::<serde_json::Value>("x").unwrap_err()
            )),
            FailureClass::Transient
        );
        assert_eq!(
            classify(&crate::oauth::oauth_error("callback state mismatch")),
            FailureClass::Transient
        );
    }

    // ----- the real vault ---------------------------------------------------

    #[test]
    fn vault_store_round_trips_tokens_encrypted_at_rest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut extra = serde_json::Map::new();
        extra.insert("sku".to_string(), serde_json::json!("pro"));
        let tokens = TokenSet {
            access_token: "SECRET-ACCESS-TOKEN-VALUE".into(),
            refresh_token: Some("SECRET-REFRESH-TOKEN-VALUE".into()),
            expires_at: Millis(T0),
            scopes: vec!["openid".into()],
            token_type: "Bearer".into(),
            extra,
        };

        {
            let store = VaultStore::new(Vault::open(dir.path()).expect("open"));
            store.save(CREDENTIAL, &tokens).expect("save");
            let loaded = store.load(CREDENTIAL).expect("load").expect("present");
            assert_eq!(loaded, tokens, "unknown extra keys survive persistence");
        } // vault dropped — everything must already be on disk

        // Reopen: `save` flushes before returning, so a restart cannot lose the credential.
        let store = VaultStore::new(Vault::open(dir.path()).expect("reopen"));
        let loaded = store.load(CREDENTIAL).expect("load").expect("present");
        assert_eq!(loaded.access_token, "SECRET-ACCESS-TOKEN-VALUE");

        // At rest the token is ciphertext — grep the vault file and find nothing.
        let raw = std::fs::read(dir.path().join("vault.enc")).expect("read vault");
        assert!(
            !String::from_utf8_lossy(&raw).contains("SECRET-ACCESS-TOKEN-VALUE"),
            "tokens must never be stored in the clear"
        );
        assert_eq!(store.load("absent-key").expect("absent"), None);
    }
}
