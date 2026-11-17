//! Server-owned state for interactive OAuth sessions.
//!
//! A bounded registry owns browser listeners and device-code state. Token results remain server-side
//! until the conductor has durably attached the account.
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use super::flow::{Callback, Loopback};
use super::{DeviceAuthorization, DevicePoll, OAuthPlatform, Pkce, ScreenLogin, TokenSet};

const SESSION_TTL: Duration = Duration::from_secs(10 * 60);
const MAX_SESSIONS: usize = 64;
/// How soon after the page opens the first completion poll runs.
///
/// A browser-completion login is short: the human is already signed in to the provider in that
/// browser, so the common case is one click and the page reports done within a second or two.
const SCREEN_POLL_START: Duration = Duration::from_secs(1);
/// Growth factor between polls while the human is still on the provider's page.
///
/// The cadence the page itself advertises: poll once a second, stretch by 1.2 each time, so an
/// abandoned tab costs a request every ten seconds instead of one every ten milliseconds.
const SCREEN_POLL_GROWTH: f32 = 1.2;
/// Ceiling for [`SCREEN_POLL_GROWTH`].
const SCREEN_POLL_MAX: Duration = Duration::from_secs(10);

/// Why a session id cannot be used from this request. See [`OAuthSessions::validate_binding`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingError {
    /// No such session, or it is past its TTL. Polling again cannot revive it.
    Gone,
    /// A request for this session is already in flight; the next attempt will find it free.
    Busy,
    /// The session exists, but another provider or platform started it.
    Mismatch,
}

#[derive(Clone)]
pub struct OAuthSessions {
    sessions: Arc<Mutex<HashMap<String, Pending>>>,
    /// Redirects pasted by hand, parked where a waiting browser flow can collect them.
    ///
    /// This is a second registry rather than a field on [`Pending`] because the waiting flow takes
    /// its session *out* of `sessions` for the duration of the wait (so one slow browser cannot
    /// hold the map lock). A paste arriving during that window would therefore find nothing to
    /// talk to; this map stays reachable for exactly as long as somebody may be waiting.
    manual: Arc<Mutex<HashMap<String, Arc<ManualSlot>>>>,
}

/// One waiting sign-in, as seen by a pasted redirect.
///
/// Holds the two things the comparison needs — the provider binding and the `state` this attempt
/// was started with — plus the bound port, because the token exchange rebuilds the same
/// `redirect_uri` the authorization request used.
struct ManualSlot {
    provider_id: String,
    state: String,
    /// Whether this platform binds the code to the attempt with a PKCE verifier of its own. When
    /// it does, a paste carrying only the code is still ours and is accepted; when it does not,
    /// the state is the only thing tying the code to this sign-in and is required.
    pkce_bound: bool,
    port: u16,
    callback: Mutex<Option<Callback>>,
    notify: tokio::sync::Notify,
    expires: tokio::time::Instant,
}

struct Pending {
    platform: &'static dyn OAuthPlatform,
    kind: PendingKind,
    expires: tokio::time::Instant,
    provider_id: String,
    last_polled: Option<tokio::time::Instant>,
    interval: Duration,
    busy: bool,
}

enum PendingKind {
    Browser {
        state: String,
        pkce: Option<Pkce>,
        listener: Loopback,
    },
    BrowserComplete {
        tokens: TokenSet,
        identity: Option<super::AccountIdentity>,
    },
    /// A login whose only return path is the operator copying `code#state` off the provider's page.
    ///
    /// No listener is bound — the provider sends the browser to a callback on *its* origin, which
    /// this router could never receive — so the wait is on [`ManualSlot`] alone. The exchange is
    /// otherwise the same one [`PendingKind::Browser`] performs, down to the PKCE verifier and the
    /// cached [`PendingKind::BrowserComplete`] result.
    Paste {
        state: String,
        pkce: Option<Pkce>,
    },
    BrowserInProgress,
    Device {
        authorization: DeviceAuthorization,
        result: Option<TokenSet>,
    },
    /// The human finishes on the provider's own page; this router only watches for it.
    ///
    /// `result` mirrors the device flow: once tokens exist they are kept, so a repeated poll answers
    /// with the same pair instead of spending another exchange, and a failure to read the identity
    /// afterwards does not throw the tokens away.
    Screen {
        login: ScreenLogin,
        result: Option<TokenSet>,
    },
}

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)] // Public result shape is stable; device flow is infrequent.
pub enum PollResult {
    Pending {
        retry_after_seconds: u64,
    },
    SlowDown {
        interval_seconds: u64,
    },
    Complete {
        tokens: TokenSet,
        identity: super::AccountIdentity,
    },
    Failed {
        message: String,
    },
}

impl OAuthSessions {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            manual: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn start_browser(
        &self,
        platform: &'static dyn OAuthPlatform,
        provider_id: String,
    ) -> Result<(String, String), String> {
        if !platform.begins_with_loopback() {
            return Err(if platform.begins_with_screen_login() {
                "platform is finished on the provider's own page, not by redirecting back here"
                    .to_string()
            } else {
                "platform requires device-code authorization".to_string()
            });
        }
        self.ensure_capacity().await?;
        // A new attempt for this provider supersedes an older one: the operator clicking Connect
        // again means the previous browser is lost. More urgently, it releases the redirect port —
        // Antigravity has exactly one registered port and no fallback, so an abandoned attempt
        // would otherwise keep it bound for the whole TTL and make the retry fail with a port
        // conflict that reads like a configuration error.
        self.discard_superseded(&provider_id, platform.id()).await;
        let listener = Loopback::bind(
            platform.redirect_ports(),
            platform.validates_redirect_port(),
        )
        .await
        .map_err(safe_error)?;
        let state = super::flow::new_state();
        let pkce = platform.requires_pkce().then(Pkce::generate);
        let redirect_port = listener.port();
        let url = platform
            .authorization_url(&state, pkce.as_ref(), redirect_port)
            .map_err(safe_error)?;
        let expires = tokio::time::Instant::now() + SESSION_TTL;
        let id = self
            .insert(Pending {
                platform,
                kind: PendingKind::Browser {
                    state: state.clone(),
                    pkce,
                    listener,
                },
                expires,
                provider_id: provider_id.clone(),
                last_polled: None,
                interval: Duration::from_secs(5),
                busy: false,
            })
            .await?;
        // The paste fallback is reachable from the moment the URL exists: an operator whose
        // browser cannot come back should not have to wait out the automatic path first.
        self.manual.lock().await.insert(
            id.clone(),
            Arc::new(ManualSlot {
                provider_id,
                state,
                pkce_bound: platform.requires_pkce(),
                port: redirect_port,
                callback: Mutex::new(None),
                notify: tokio::sync::Notify::new(),
                expires,
            }),
        );
        Ok((id, url))
    }

    /// Begin a login the human completes by pasting `code#state` back here.
    ///
    /// The provider's redirect target is a page on its own origin — Claude's is
    /// `https://console.anthropic.com/oauth/code/callback` — so no port can be bound and nothing
    /// can be received automatically. What makes the flow work is that such a page *displays* the
    /// code and state for the human to copy; [`submit_redirect`](Self::submit_redirect) parks the
    /// pasted value in the same [`ManualSlot`] a browser flow would use, and
    /// [`finish_browser`](Self::finish_browser) waits on that slot instead of a listener.
    ///
    /// `authorization_url` is built with port `0`, which is honest: the platform ignores the port
    /// and substitutes its own fixed redirect URI. A platform that does not declare
    /// [`paste_redirect_uri`](OAuthPlatform::paste_redirect_uri) is refused rather than started
    /// with a URL that would end somewhere this router cannot see.
    pub async fn start_paste(
        &self,
        platform: &'static dyn OAuthPlatform,
        provider_id: String,
    ) -> Result<(String, String), String> {
        if platform.paste_redirect_uri().is_none() {
            return Err("platform cannot be finished by pasting a code".into());
        }
        self.ensure_capacity().await?;
        self.discard_superseded(&provider_id, platform.id()).await;
        let state = super::flow::new_state();
        // A pasted code has no listener tying it to this attempt, so PKCE is what binds it. Every
        // paste platform declares it; asking here means a future one cannot silently lose it.
        let pkce = platform.requires_pkce().then(Pkce::generate);
        let url = platform
            .authorization_url(&state, pkce.as_ref(), 0)
            .map_err(safe_error)?;
        let expires = tokio::time::Instant::now() + SESSION_TTL;
        let id = self
            .insert(Pending {
                platform,
                kind: PendingKind::Paste {
                    state: state.clone(),
                    pkce,
                },
                expires,
                provider_id: provider_id.clone(),
                last_polled: None,
                interval: Duration::from_secs(5),
                busy: false,
            })
            .await?;
        self.manual.lock().await.insert(
            id.clone(),
            Arc::new(ManualSlot {
                provider_id,
                state,
                pkce_bound: platform.requires_pkce(),
                port: 0,
                callback: Mutex::new(None),
                notify: tokio::sync::Notify::new(),
                expires,
            }),
        );
        Ok((id, url))
    }

    /// Drop unfinished browser attempts for the same provider and platform, releasing their
    /// listeners.
    ///
    /// Only sessions sitting idle in the registry are dropped. A session whose `finish_browser` is
    /// already running has been taken *out* of the registry, so a live wait is never cancelled
    /// underneath itself.
    async fn discard_superseded(&self, provider_id: &str, platform_id: &str) {
        let stale: Vec<String> = {
            let sessions = self.sessions.lock().await;
            sessions
                .iter()
                .filter(|(_, pending)| {
                    pending.provider_id == provider_id && pending.platform.id() == platform_id
                })
                .map(|(id, _)| id.clone())
                .collect()
        };
        if stale.is_empty() {
            return;
        }
        // Removed under its own lock rather than nested with the `manual` lock, so no ordering
        // between the two maps can deadlock.
        {
            let mut sessions = self.sessions.lock().await;
            for id in &stale {
                sessions.remove(id);
            }
        }
        let mut manual = self.manual.lock().await;
        for id in &stale {
            manual.remove(id);
        }
    }

    pub async fn start_device(
        &self,
        platform: &'static dyn OAuthPlatform,
        provider_id: String,
    ) -> Result<(String, DeviceAuthorization), String> {
        if !platform.supports_device_flow() {
            return Err(if platform.begins_with_screen_login() {
                "platform is finished on the provider's own page, so there is no code to enter"
                    .to_string()
            } else {
                "platform requires browser redirect authorization".to_string()
            });
        }
        self.ensure_capacity().await?;
        let authorization = platform.device_start().await.map_err(safe_error)?;
        let expires = tokio::time::Instant::now() + authorization.expires_in.min(SESSION_TTL);
        let interval = authorization
            .interval
            .max(Duration::from_secs(1))
            .min(SESSION_TTL);
        let id = self
            .insert(Pending {
                platform,
                kind: PendingKind::Device {
                    authorization: authorization.clone(),
                    result: None,
                },
                expires,
                provider_id,
                last_polled: None,
                interval,
                busy: false,
            })
            .await?;
        Ok((id, authorization))
    }

    /// Begin a login the human finishes on the provider's own page.
    ///
    /// Returns the session id and the page to open. There is no listener and no paste fallback
    /// because nothing comes back to this router: the only exits are a poll that reports the tokens
    /// or the TTL. That is also why the supersession rule matters here — a second click on Connect
    /// abandons the first page, and leaving both sessions alive would let a stale page complete a
    /// login the operator has already replaced.
    pub async fn start_screen(
        &self,
        platform: &'static dyn OAuthPlatform,
        provider_id: String,
    ) -> Result<(String, String), String> {
        if !platform.begins_with_screen_login() {
            return Err("platform is not finished in the browser".into());
        }
        self.ensure_capacity().await?;
        self.discard_superseded(&provider_id, platform.id()).await;
        let login = platform.screen_login_start().await.map_err(safe_error)?;
        let url = login.url.clone();
        let expires = tokio::time::Instant::now() + SESSION_TTL;
        let id = self
            .insert(Pending {
                platform,
                kind: PendingKind::Screen {
                    login,
                    result: None,
                },
                expires,
                provider_id,
                last_polled: None,
                interval: SCREEN_POLL_START,
                busy: false,
            })
            .await?;
        Ok((id, url))
    }

    async fn ensure_capacity(&self) -> Result<(), String> {
        let now = tokio::time::Instant::now();
        let live = {
            let mut sessions = self.sessions.lock().await;
            sessions.retain(|_, pending| pending.expires > now);
            sessions.len()
        };
        // The paste registry is reaped here too. Nothing else ever walks it, so without this every
        // abandoned attempt leaves a slot behind for the life of the process — one more per TTL
        // window, even though the session registry itself is capped at MAX_SESSIONS. Taken under
        // its own lock, never nested with the session lock, so no ordering can deadlock.
        self.manual
            .lock()
            .await
            .retain(|_, slot| slot.expires > now);
        if live >= MAX_SESSIONS {
            return Err("too many pending OAuth sessions; finish or restart one".into());
        }
        Ok(())
    }

    /// Flip the in-flight flag the way a running poll does, so a test can observe the `Busy`
    /// binding without racing one.
    #[cfg(test)]
    async fn mark_busy(&self, id: &str) {
        if let Some(pending) = self.sessions.lock().await.get_mut(id) {
            pending.busy = true;
        }
    }

    async fn insert(&self, pending: Pending) -> Result<String, String> {
        let mut sessions = self.sessions.lock().await;
        sessions.retain(|_, p| p.expires > tokio::time::Instant::now());
        if sessions.len() >= MAX_SESSIONS {
            return Err("too many pending OAuth sessions; finish or restart one".into());
        }
        let id = super::flow::new_state();
        sessions.insert(id.clone(), pending);
        Ok(id)
    }

    /// Check that `id` is a live session this request is allowed to use.
    ///
    /// The three failures are not interchangeable to the caller: a session that is gone can never
    /// come back, one that is busy will work on the next attempt, and a mismatched binding will
    /// never work at all. `BindingError` keeps them apart so the HTTP layer can answer with a
    /// status the panel acts on instead of one it retries.
    pub async fn validate_binding(
        &self,
        id: &str,
        provider_id: &str,
        platform_id: &str,
    ) -> Result<(), BindingError> {
        let sessions = self.sessions.lock().await;
        let Some(pending) = sessions.get(id) else {
            return Err(BindingError::Gone);
        };
        if pending.expires <= tokio::time::Instant::now() {
            return Err(BindingError::Gone);
        }
        if pending.busy {
            return Err(BindingError::Busy);
        }
        if pending.provider_id != provider_id || pending.platform.id() != platform_id {
            return Err(BindingError::Mismatch);
        }
        Ok(())
    }

    /// Hand a pasted redirect URL to a sign-in that is already waiting for its browser callback.
    ///
    /// Returns as soon as the paste is accepted: the token exchange still happens in exactly one
    /// place, [`finish_browser`], which is already waiting. Keeping it that way is the point — the
    /// pasted path and the loopback path cannot drift apart, because there is only one path.
    pub async fn submit_redirect(
        &self,
        id: &str,
        provider_id: &str,
        pasted: &str,
    ) -> Result<(), String> {
        let parsed = super::flow::parse_pasted_redirect(pasted)?;
        let slot = {
            let map = self.manual.lock().await;
            map.get(id).cloned()
        };
        let slot = slot.ok_or_else(|| {
            "this sign-in is no longer waiting for a browser redirect (it may have finished or \
             expired); start it again"
                .to_string()
        })?;
        if slot.provider_id != provider_id {
            return Err("OAuth session provider binding does not match".into());
        }
        if tokio::time::Instant::now() >= slot.expires {
            return Err("this sign-in expired; start it again".into());
        }
        // The state ties this code to *our* authorization request. It is not ceremony: without it,
        // a code from a different flow can be bound to this session, and the router ends up
        // holding somebody else's account. But where the platform enforces PKCE the code is
        // already bound to this session's verifier — a foreign code simply fails at exchange — so
        // a paste carrying only the code is accepted rather than refused. Asking for the whole URL
        // there would be friction with nothing behind it, and the code alone is the easiest thing
        // an operator can select out of a provider's page.
        let state =
            match parsed.state {
                Some(state) if state == slot.state => state,
                Some(_) => return Err(
                    "that redirect belongs to a different sign-in attempt — its state parameter \
                     does not match this session; paste the URL from the attempt you just started"
                        .into(),
                ),
                None if slot.pkce_bound => slot.state.clone(),
                None => return Err(
                    "the pasted value has no state parameter, and this platform's flow does not \
                     bind the code to this sign-in on its own, so the two cannot be tied together; \
                     paste the whole URL from the browser's address bar"
                        .into(),
                ),
            };
        let mut parked = slot.callback.lock().await;
        if parked.is_some() {
            return Err("a redirect was already submitted for this sign-in".into());
        }
        *parked = Some(Callback {
            code: parsed.code,
            state,
            port: slot.port,
        });
        drop(parked);
        slot.notify.notify_one();
        Ok(())
    }

    pub async fn finish_browser(
        &self,
        id: &str,
        provider_id: &str,
    ) -> Result<(TokenSet, super::AccountIdentity), String> {
        // Take the session here rather than inside the helper: only the caller that actually
        // acquired this attempt may retire its paste slot. A concurrent duplicate call, or one
        // naming the wrong provider, finds nothing to take and must leave the live attempt's slot
        // alone — removing it would silently disable the paste fallback for a sign-in that
        // somebody else is still waiting on.
        let pending = self.take(id, provider_id).await?;
        let result = self.finish_browser_inner(id, pending).await;
        // One exit for every path of the attempt we own — success, refusal, expiry — so a stale
        // slot can never outlive it.
        self.manual.lock().await.remove(id);
        result
    }

    async fn finish_browser_inner(
        &self,
        id: &str,
        mut pending: Pending,
    ) -> Result<(TokenSet, super::AccountIdentity), String> {
        if pending.busy {
            self.reinsert(id.to_owned(), pending).await?;
            return Err("OAuth session operation already in progress".into());
        }
        pending.busy = true;
        if let PendingKind::BrowserComplete { tokens, identity } = &pending.kind {
            if let Some(identity) = identity {
                let result = (tokens.clone(), identity.clone());
                pending.busy = false;
                self.reinsert(id.to_owned(), pending).await?;
                return Ok(result);
            }
            let mut tokens = tokens.clone();
            let platform = pending.platform;
            if let Err(error) = platform.complete_login(&mut tokens).await {
                pending.kind = PendingKind::BrowserComplete {
                    tokens,
                    identity: None,
                };
                pending.busy = false;
                self.reinsert(id.to_owned(), pending).await?;
                return Err(safe_error(error));
            }
            let identity = match platform.account_identity(&tokens).await {
                Ok(identity) => identity,
                Err(error) => {
                    pending.busy = false;
                    self.reinsert(id.to_owned(), pending).await?;
                    return Err(safe_error(error));
                }
            };
            pending.kind = PendingKind::BrowserComplete {
                tokens: tokens.clone(),
                identity: Some(identity.clone()),
            };
            pending.busy = false;
            self.reinsert(id.to_owned(), pending).await?;
            return Ok((tokens, identity));
        }
        let old_kind = std::mem::replace(&mut pending.kind, PendingKind::BrowserInProgress);
        // Both returning flows end in the same exchange: the browser flow arrives at `callback` by
        // listener or paste, the paste-only flow by paste alone. Only the wait differs, so only the
        // wait is branched on.
        let (state, pkce, callback) = match old_kind {
            PendingKind::Browser {
                state,
                pkce,
                listener,
            } => {
                let slot = self.manual.lock().await.get(id).cloned();
                let callback: Callback = match slot {
                    Some(slot) => {
                        // `notified()` is armed *before* the select so a paste that lands during
                        // setup is not lost: `Notify` holds the permit until somebody waits on it.
                        let notified = slot.notify.notified();
                        tokio::select! {
                            result = listener.await_callback(&state, pending.platform.label(), SESSION_TTL) => {
                                match result {
                                    Ok(callback) => callback,
                                    Err(error) => return Err(safe_error(error)),
                                }
                            }
                            _ = notified => {
                                let pasted = slot.callback.lock().await.take();
                                pasted.ok_or_else(|| {
                                    "the pasted redirect could not be read".to_string()
                                })?
                            }
                        }
                    }
                    // No slot means it was already cleaned up; still try the automatic path rather
                    // than refusing a sign-in that may well succeed.
                    None => listener
                        .await_callback(&state, pending.platform.label(), SESSION_TTL)
                        .await
                        .map_err(safe_error)?,
                };
                (state, pkce, callback)
            }
            PendingKind::Paste { state, pkce } => {
                let slot = self.manual.lock().await.get(id).cloned().ok_or_else(|| {
                    "this sign-in is no longer waiting for a pasted code (it may have finished or \
                     expired); start it again"
                        .to_string()
                })?;
                // Armed before the check so a paste that lands in between is remembered.
                let notified = slot.notify.notified();
                let parked = slot.callback.lock().await.take();
                let callback = match parked {
                    Some(callback) => callback,
                    None => {
                        tokio::time::timeout(SESSION_TTL, notified)
                            .await
                            .map_err(|_| {
                                "the sign-in expired before a code was pasted".to_string()
                            })?;
                        let pasted = slot.callback.lock().await.take();
                        pasted.ok_or_else(|| "the pasted code could not be read".to_string())?
                    }
                };
                (state, pkce, callback)
            }
            _ => return Err("OAuth session is not waiting for a redirect".into()),
        };
        let tokens = match pending
            .platform
            .exchange_code_with_state(&callback.code, &state, pkce.as_ref(), callback.port)
            .await
        {
            Ok(tokens) => tokens,
            Err(error) => {
                return Err(safe_error(error));
            }
        };
        let mut tokens = tokens;
        if let Err(error) = pending.platform.complete_login(&mut tokens).await {
            pending.kind = PendingKind::BrowserComplete {
                tokens,
                identity: None,
            };
            self.reinsert(id.to_owned(), pending).await?;
            return Err(safe_error(error));
        }
        let identity = match pending.platform.account_identity(&tokens).await {
            Ok(identity) => identity,
            Err(error) => {
                pending.kind = PendingKind::BrowserComplete {
                    tokens,
                    identity: None,
                };
                self.reinsert(id.to_owned(), pending).await?;
                return Err(safe_error(error));
            }
        };
        pending.kind = PendingKind::BrowserComplete {
            tokens: tokens.clone(),
            identity: Some(identity.clone()),
        };
        pending.busy = false;
        self.reinsert(id.to_owned(), pending).await?;
        Ok((tokens, identity))
    }

    pub async fn poll_device(&self, id: &str, provider_id: &str) -> Result<PollResult, String> {
        // Taking the session out prevents a provider network wait from holding the global map lock.
        // It also makes simultaneous polls for this session fail closed instead of double-spending.
        let mut pending = self.take(id, provider_id).await?;
        if pending.busy {
            self.reinsert(id.to_owned(), pending).await?;
            return Err("OAuth session operation already in progress".into());
        }
        pending.busy = true;
        let PendingKind::Device {
            authorization,
            result,
        } = &mut pending.kind
        else {
            return Err("OAuth session is not a device flow".into());
        };
        if let Some(tokens) = result.as_ref() {
            let tokens = tokens.clone();
            let identity = match pending.platform.account_identity(&tokens).await {
                Ok(identity) => identity,
                Err(error) => {
                    self.reinsert(id.to_owned(), pending).await?;
                    return Err(safe_error(error));
                }
            };
            self.reinsert(id.to_owned(), pending).await?;
            return Ok(PollResult::Complete { tokens, identity });
        }
        let now = tokio::time::Instant::now();
        if let Some(last) = pending.last_polled {
            if now < last + pending.interval {
                let wait = (last + pending.interval - now).as_secs().max(1);
                self.reinsert(id.to_owned(), pending).await?;
                return Ok(PollResult::Pending {
                    retry_after_seconds: wait,
                });
            }
        }
        pending.last_polled = Some(now);
        let outcome = match pending.platform.device_poll(authorization).await {
            Ok(outcome) => outcome,
            Err(error) => {
                self.reinsert(id.to_owned(), pending).await?;
                return Err(safe_error(error));
            }
        };
        match outcome {
            DevicePoll::Pending => {
                let retry_after_seconds = pending.interval.as_secs();
                self.reinsert(id.to_owned(), pending).await?;
                Ok(PollResult::Pending {
                    retry_after_seconds,
                })
            }
            DevicePoll::SlowDown => {
                pending.interval = pending.interval.saturating_add(Duration::from_secs(5));
                pending.last_polled = Some(tokio::time::Instant::now());
                let interval_seconds = pending.interval.as_secs();
                self.reinsert(id.to_owned(), pending).await?;
                Ok(PollResult::SlowDown { interval_seconds })
            }
            DevicePoll::Success(tokens) => {
                let identity = match pending.platform.account_identity(&tokens).await {
                    Ok(identity) => identity,
                    Err(error) => {
                        if let PendingKind::Device { result, .. } = &mut pending.kind {
                            *result = Some(tokens);
                        }
                        self.reinsert(id.to_owned(), pending).await?;
                        return Err(safe_error(error));
                    }
                };
                if let PendingKind::Device { result, .. } = &mut pending.kind {
                    *result = Some(tokens.clone());
                }
                pending.busy = false;
                self.reinsert(id.to_owned(), pending).await?;
                Ok(PollResult::Complete { tokens, identity })
            }
            DevicePoll::ExpiredToken => Ok(PollResult::Failed {
                message: "device authorization expired; start again".into(),
            }),
            DevicePoll::AccessDenied => Ok(PollResult::Failed {
                message: "authorization was denied".into(),
            }),
            DevicePoll::Failed { message } => {
                self.reinsert(id.to_owned(), pending).await?;
                Ok(PollResult::Failed {
                    message: message.chars().take(240).collect(),
                })
            }
        }
    }

    /// Ask the provider whether the page has been finished, advancing the poll cadence.
    ///
    /// The cadence belongs to the session rather than to the platform: it describes how hard this
    /// router may knock on somebody else's door while a human is looking at it, which is the same
    /// question for every platform that uses this flow.
    pub async fn poll_screen(&self, id: &str, provider_id: &str) -> Result<PollResult, String> {
        // Taken out for the duration of the network wait, exactly as the device flow does: one
        // session cannot be polled twice at once, and the map lock is not held across the call.
        let mut pending = self.take(id, provider_id).await?;
        if pending.busy {
            self.reinsert(id.to_owned(), pending).await?;
            return Err("OAuth session operation already in progress".into());
        }
        pending.busy = true;
        let PendingKind::Screen { login, result } = &pending.kind else {
            self.reinsert(id.to_owned(), pending).await?;
            return Err("OAuth session is not a browser-completion login".into());
        };
        let login = login.clone();
        if let Some(tokens) = result.as_ref() {
            let tokens = tokens.clone();
            let identity = match pending.platform.account_identity(&tokens).await {
                Ok(identity) => identity,
                Err(error) => {
                    self.reinsert(id.to_owned(), pending).await?;
                    return Err(safe_error(error));
                }
            };
            self.reinsert(id.to_owned(), pending).await?;
            return Ok(PollResult::Complete { tokens, identity });
        }
        let now = tokio::time::Instant::now();
        if let Some(last) = pending.last_polled {
            if now < last + pending.interval {
                let wait = (last + pending.interval - now).as_secs().max(1);
                self.reinsert(id.to_owned(), pending).await?;
                return Ok(PollResult::Pending {
                    retry_after_seconds: wait,
                });
            }
        }
        pending.last_polled = Some(now);
        pending.interval = grow_screen_interval(pending.interval);
        let outcome = match pending.platform.screen_login_poll(&login).await {
            Ok(outcome) => outcome,
            Err(error) => {
                self.reinsert(id.to_owned(), pending).await?;
                return Err(safe_error(error));
            }
        };
        match outcome {
            DevicePoll::Pending => {
                let retry_after_seconds = pending.interval.as_secs();
                self.reinsert(id.to_owned(), pending).await?;
                Ok(PollResult::Pending {
                    retry_after_seconds,
                })
            }
            DevicePoll::SlowDown => {
                pending.interval =
                    grow_screen_interval(pending.interval.saturating_add(Duration::from_secs(5)));
                pending.last_polled = Some(tokio::time::Instant::now());
                let interval_seconds = pending.interval.as_secs();
                self.reinsert(id.to_owned(), pending).await?;
                Ok(PollResult::SlowDown { interval_seconds })
            }
            DevicePoll::Success(tokens) => {
                let identity = match pending.platform.account_identity(&tokens).await {
                    Ok(identity) => identity,
                    Err(error) => {
                        if let PendingKind::Screen { result, .. } = &mut pending.kind {
                            *result = Some(tokens);
                        }
                        self.reinsert(id.to_owned(), pending).await?;
                        return Err(safe_error(error));
                    }
                };
                if let PendingKind::Screen { result, .. } = &mut pending.kind {
                    *result = Some(tokens.clone());
                }
                pending.busy = false;
                self.reinsert(id.to_owned(), pending).await?;
                Ok(PollResult::Complete { tokens, identity })
            }
            DevicePoll::ExpiredToken => Ok(PollResult::Failed {
                message: "the login page expired; start again".into(),
            }),
            DevicePoll::AccessDenied => Ok(PollResult::Failed {
                message: "authorization was denied".into(),
            }),
            DevicePoll::Failed { message } => {
                self.reinsert(id.to_owned(), pending).await?;
                Ok(PollResult::Failed {
                    message: message.chars().take(240).collect(),
                })
            }
        }
    }

    pub async fn acknowledge(&self, id: &str, provider_id: &str) -> Result<(), String> {
        let mut sessions = self.sessions.lock().await;
        match sessions.get(id) {
            Some(pending) if pending.provider_id == provider_id && !pending.busy => {}
            _ => return Err("OAuth session not found or expired".into()),
        }
        sessions.remove(id);
        Ok(())
    }

    async fn take(&self, id: &str, provider_id: &str) -> Result<Pending, String> {
        let mut sessions = self.sessions.lock().await;
        let pending = sessions
            .get(id)
            .ok_or_else(|| "OAuth session not found or expired".to_string())?;
        if pending.expires <= tokio::time::Instant::now() {
            sessions.remove(id);
            return Err("OAuth session expired".into());
        }
        if pending.provider_id != provider_id {
            return Err("OAuth session belongs to a different provider".into());
        }
        if pending.busy {
            return Err("OAuth session operation already in progress".into());
        }
        Ok(sessions.remove(id).expect("session was just validated"))
    }

    async fn reinsert(&self, id: String, mut pending: Pending) -> Result<(), String> {
        pending.busy = false;
        let mut sessions = self.sessions.lock().await;
        if pending.expires <= tokio::time::Instant::now() {
            return Err("OAuth session expired".into());
        }
        if sessions.len() >= MAX_SESSIONS {
            return Err("too many pending OAuth sessions".into());
        }
        sessions.insert(id, pending);
        Ok(())
    }
}

impl Default for OAuthSessions {
    fn default() -> Self {
        Self::new()
    }
}

/// Stretch the completion-poll cadence toward its ceiling.
fn grow_screen_interval(interval: Duration) -> Duration {
    interval
        .mul_f32(SCREEN_POLL_GROWTH)
        .min(SCREEN_POLL_MAX)
        .max(SCREEN_POLL_START)
}

/// Bound what an upstream error can push into the log and the panel, keeping both of its ends.
///
/// Both ends matter, and a one-sided cut is what hid Google's verdict from the operator on
/// 2026-11-10: "Cloud Code would onboard this account only as `standard-tier`, ... refused:
/// free-tier (VALIDATION_REQUIRED): Your current ac" is where the message stopped. The head named
/// the failure and the tail named the reason code that explains it, and the 240-character ceiling
/// that was there cut the second off mid-word. A message at or under the ceiling now passes
/// through whole; a longer one keeps most of its head and the rest of its tail, so a provider
/// cannot flood the panel while its own verdict stays visible. Provider-supplied text has its own
/// narrower ceiling (`refresh::clip`).
fn safe_error(error: impl std::fmt::Display) -> String {
    const CAP: usize = 1000;
    const ELISION: &str = " ... ";
    let message = error.to_string();
    if message.chars().count() <= CAP {
        return message;
    }
    let budget = CAP - ELISION.chars().count();
    let head: String = message.chars().take(budget * 3 / 5).collect();
    let tail: String = message
        .chars()
        .rev()
        .take(budget * 2 / 5)
        .collect::<Vec<char>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}{ELISION}{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::{AccountIdentity, RedirectPorts};
    use async_trait::async_trait;

    /// A browser platform that records the code it exchanged, so a test can prove *which* redirect
    /// actually completed the sign-in rather than just that something did.
    struct FakeBrowser {
        exchanged: std::sync::Mutex<Vec<String>>,
        pkce: bool,
    }

    impl FakeBrowser {
        fn leaked() -> &'static Self {
            Self::leaked_with_pkce(true)
        }

        /// The same double, but with no PKCE binding — the shape of a platform whose code is only
        /// tied to the attempt by its state parameter.
        fn leaked_without_pkce() -> &'static Self {
            Self::leaked_with_pkce(false)
        }

        fn leaked_with_pkce(pkce: bool) -> &'static Self {
            Box::leak(Box::new(Self {
                exchanged: std::sync::Mutex::new(Vec::new()),
                pkce,
            }))
        }

        fn exchanged(&self) -> Vec<String> {
            self.exchanged.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl OAuthPlatform for FakeBrowser {
        fn id(&self) -> &'static str {
            "fake-browser"
        }
        fn label(&self) -> &'static str {
            "Fake browser"
        }
        fn description(&self) -> &'static str {
            "Test double with a loopback flow."
        }
        fn begins_with_loopback(&self) -> bool {
            true
        }
        fn redirect_ports(&self) -> RedirectPorts {
            RedirectPorts {
                host: "127.0.0.1",
                primary: 0,
                fallback: None,
                path: "/cb",
            }
        }
        fn requires_pkce(&self) -> bool {
            self.pkce
        }
        fn validates_redirect_port(&self) -> bool {
            false
        }
        fn authorization_url(
            &self,
            state: &str,
            _pkce: Option<&Pkce>,
            port: u16,
        ) -> crate::oauth::Result<String> {
            Ok(format!(
                "https://example.test/authorize?state={state}&port={port}"
            ))
        }
        async fn exchange_code(
            &self,
            code: &str,
            _pkce: Option<&Pkce>,
            _port: u16,
        ) -> crate::oauth::Result<TokenSet> {
            self.exchanged.lock().unwrap().push(code.to_owned());
            Ok(TokenSet::new("access-from-exchange"))
        }
        async fn refresh(&self, _refresh_token: &str) -> crate::oauth::Result<TokenSet> {
            Ok(TokenSet::new("refreshed"))
        }
        async fn account_identity(
            &self,
            _tokens: &TokenSet,
        ) -> crate::oauth::Result<AccountIdentity> {
            Ok(AccountIdentity {
                id: "acct-1".into(),
                label: "Account One".into(),
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
            &[]
        }
        fn refresh_lead(&self) -> Duration {
            Duration::from_secs(0)
        }
    }

    /// A platform whose redirect lands on the provider's own page: no port is ever bound, and the
    /// only way a code reaches this process is the human pasting it.
    struct FakePaste {
        exchanged: std::sync::Mutex<Vec<(String, String)>>,
    }

    impl FakePaste {
        fn new() -> &'static Self {
            Box::leak(Box::new(Self {
                exchanged: std::sync::Mutex::new(Vec::new()),
            }))
        }

        fn exchanged(&self) -> Vec<(String, String)> {
            self.exchanged.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl OAuthPlatform for FakePaste {
        fn id(&self) -> &'static str {
            "fake-paste"
        }
        fn label(&self) -> &'static str {
            "Fake paste"
        }
        fn description(&self) -> &'static str {
            "Test double whose redirect cannot come back."
        }
        fn begins_with_loopback(&self) -> bool {
            false
        }
        fn paste_redirect_uri(&self) -> Option<&'static str> {
            Some("https://example.test/oauth/code/callback")
        }
        fn redirect_ports(&self) -> RedirectPorts {
            RedirectPorts {
                host: "",
                primary: 0,
                fallback: None,
                path: "",
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
            pkce: Option<&Pkce>,
            _port: u16,
        ) -> crate::oauth::Result<String> {
            Ok(format!(
                "https://example.test/authorize?state={state}&challenge={}",
                pkce.map(|p| p.challenge().to_owned()).unwrap_or_default()
            ))
        }
        async fn exchange_code(
            &self,
            _code: &str,
            _pkce: Option<&Pkce>,
            _port: u16,
        ) -> crate::oauth::Result<TokenSet> {
            panic!("a paste platform exchanges with its state; the code-only method must not run")
        }
        async fn exchange_code_with_state(
            &self,
            code: &str,
            state: &str,
            _pkce: Option<&Pkce>,
            _port: u16,
        ) -> crate::oauth::Result<TokenSet> {
            self.exchanged
                .lock()
                .unwrap()
                .push((code.to_owned(), state.to_owned()));
            Ok(TokenSet::new("access-from-paste-exchange"))
        }
        async fn refresh(&self, _refresh_token: &str) -> crate::oauth::Result<TokenSet> {
            Ok(TokenSet::new("refreshed"))
        }
        async fn account_identity(
            &self,
            _tokens: &TokenSet,
        ) -> crate::oauth::Result<AccountIdentity> {
            Ok(AccountIdentity {
                id: "acct-paste".into(),
                label: "Pasted Account".into(),
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
            &[]
        }
        fn refresh_lead(&self) -> Duration {
            Duration::from_secs(0)
        }
    }

    #[tokio::test]
    async fn a_paste_platform_starts_without_binding_a_port_and_finishes_with_its_state() {
        let platform = FakePaste::new();
        let sessions = OAuthSessions::new();
        let (id, url) = sessions
            .start_paste(platform, "provider-1".into())
            .await
            .expect("a paste platform starts");
        assert!(
            state_of(&url).len() >= 32,
            "the URL carries a fresh state: {url}"
        );
        assert!(
            !url.contains("port="),
            "no port is bound for this flow: {url}"
        );

        let waiting = {
            let sessions = sessions.clone();
            let id = id.clone();
            tokio::spawn(async move { sessions.finish_browser(&id, "provider-1").await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Exactly what the provider's page shows: `code#state`, with no URL around it.
        let state = state_of(&url);
        sessions
            .submit_redirect(&id, "provider-1", &format!("pasted-code#{state}"))
            .await
            .expect("a code#state paste is accepted");
        let (tokens, identity) = tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("the waiting flow must finish")
            .expect("the task must not panic")
            .expect("the sign-in must succeed");
        assert_eq!(tokens.access_token, "access-from-paste-exchange");
        assert_eq!(identity.id, "acct-paste");
        assert_eq!(
            platform.exchanged(),
            vec![("pasted-code".to_owned(), state)],
            "the exchange must receive the code and the state that binds it to this attempt"
        );
    }

    #[tokio::test]
    async fn a_loopback_platform_cannot_start_a_paste_flow() {
        let sessions = OAuthSessions::new();
        let error = sessions
            .start_paste(FakeBrowser::leaked(), "provider-1".into())
            .await
            .expect_err("a platform with a listener has nothing to paste into");
        assert!(
            error.contains("cannot be finished by pasting a code"),
            "{error}"
        );
    }

    /// The `state` the flow generated, read back out of the authorize URL the panel would open.
    fn state_of(url: &str) -> String {
        url.split("state=")
            .nth(1)
            .and_then(|rest| rest.split('&').next())
            .expect("the authorize URL carries a state")
            .to_owned()
    }

    /// The whole point of the fallback, and the reason the manual slot is a separate registry: by
    /// the time the operator pastes, `finish_browser` has already taken the session out of the map
    /// and is blocked on the listener. The paste must still reach it.
    #[tokio::test]
    async fn a_pasted_redirect_completes_a_waiting_sign_in() {
        let platform = FakeBrowser::leaked();
        let sessions = OAuthSessions::new();
        let (id, url) = sessions
            .start_browser(platform, "provider-1".into())
            .await
            .expect("a loopback flow starts");
        let state = state_of(&url);

        let waiting = {
            let sessions = sessions.clone();
            let id = id.clone();
            tokio::spawn(async move { sessions.finish_browser(&id, "provider-1").await })
        };
        // Let the waiter take the session out of the registry, which is exactly the window the
        // pasted submission has to survive.
        tokio::time::sleep(Duration::from_millis(50)).await;

        sessions
            .submit_redirect(
                &id,
                "provider-1",
                &format!("http://127.0.0.1:1455/auth/callback?code=good-code&state={state}"),
            )
            .await
            .expect("a correct paste is accepted");

        let (tokens, identity) = tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("the waiting flow must finish")
            .expect("the task must not panic")
            .expect("the sign-in must succeed");
        assert_eq!(tokens.access_token, "access-from-exchange");
        assert_eq!(identity.id, "acct-1");
        assert_eq!(
            platform.exchanged(),
            vec!["good-code".to_string()],
            "the exchange must have used the pasted code, exactly once"
        );
    }

    /// A paste can arrive before anything is waiting — the panel may submit in the same tick. The
    /// arrival is parked, so the later wait finds it instead of blocking for ten minutes.
    #[tokio::test]
    async fn a_paste_that_arrives_before_the_wait_is_not_lost() {
        let platform = FakeBrowser::leaked();
        let sessions = OAuthSessions::new();
        let (id, url) = sessions
            .start_browser(platform, "provider-1".into())
            .await
            .expect("a loopback flow starts");
        let state = state_of(&url);

        sessions
            .submit_redirect(&id, "provider-1", &format!("?code=early&state={state}"))
            .await
            .expect("a correct paste is accepted");

        let (_, identity) = tokio::time::timeout(
            Duration::from_secs(5),
            sessions.finish_browser(&id, "provider-1"),
        )
        .await
        .expect("the parked redirect must be found immediately")
        .expect("the sign-in must succeed");
        assert_eq!(identity.id, "acct-1");
        assert_eq!(platform.exchanged(), vec!["early".to_string()]);
    }

    /// A code from somebody else's flow must not redeem here. Refusing it is what makes the paste
    /// box safe: without the state comparison, anyone who can reach the panel could inject one.
    #[tokio::test]
    async fn a_redirect_for_another_attempt_is_refused_and_the_flow_stays_usable() {
        let platform = FakeBrowser::leaked();
        let sessions = OAuthSessions::new();
        let (id, url) = sessions
            .start_browser(platform, "provider-1".into())
            .await
            .expect("a loopback flow starts");
        let state = state_of(&url);

        let error = sessions
            .submit_redirect(
                &id,
                "provider-1",
                "http://x/cb?code=evil&state=not-our-state",
            )
            .await
            .expect_err("a foreign state must be refused");
        assert!(error.contains("different sign-in attempt"), "{error}");

        // The refusal must not have consumed the session: the real redirect still works.
        sessions
            .submit_redirect(&id, "provider-1", &format!("?code=ours&state={state}"))
            .await
            .expect("the correct paste still completes the flow");
        let (_, identity) = tokio::time::timeout(
            Duration::from_secs(5),
            sessions.finish_browser(&id, "provider-1"),
        )
        .await
        .expect("the flow must finish")
        .expect("the sign-in must succeed");
        assert_eq!(identity.id, "acct-1");
    }

    /// When the platform enforces PKCE, the code is already bound to this attempt by the verifier:
    /// a foreign code cannot be redeemed with it. A bare code is then exactly as safe as the full
    /// URL, and refusing it would be friction with nothing behind it.
    #[tokio::test]
    async fn a_bare_code_is_accepted_when_pkce_binds_it() {
        let platform = FakeBrowser::leaked();
        let sessions = OAuthSessions::new();
        let (id, _url) = sessions
            .start_browser(platform, "provider-1".into())
            .await
            .expect("a loopback flow starts");

        sessions
            .submit_redirect(&id, "provider-1", "just-a-code")
            .await
            .expect("a PKCE-bound session can use a bare code");
    }

    /// Without PKCE, the state is the only thing tying the code to this sign-in, so a bare code is
    /// refused rather than exchanged and left for the provider to reject obscurely.
    #[tokio::test]
    async fn a_bare_code_is_refused_without_pkce() {
        let platform = FakeBrowser::leaked_without_pkce();
        let sessions = OAuthSessions::new();
        let (id, _url) = sessions
            .start_browser(platform, "provider-1".into())
            .await
            .expect("a loopback flow starts");

        let error = sessions
            .submit_redirect(&id, "provider-1", "just-a-code")
            .await
            .expect_err("a code without state cannot be verified without PKCE");
        assert!(error.contains("state parameter"), "{error}");
    }

    /// A fixed redirect port (Antigravity has exactly one, with no fallback) makes this the
    /// difference between "click Connect again" and "wait ten minutes or restart the panel": the
    /// abandoned attempt must release the port when the next one starts.
    #[tokio::test]
    async fn a_new_attempt_releases_the_port_held_by_an_abandoned_one() {
        // A fixed port is essential to the test: an ephemeral one would bind successfully either
        // way and prove nothing.
        struct FixedPort;
        #[async_trait]
        impl OAuthPlatform for FixedPort {
            fn id(&self) -> &'static str {
                "fixed-port"
            }
            fn label(&self) -> &'static str {
                "Fixed port"
            }
            fn description(&self) -> &'static str {
                "Test double that reuses one registered redirect port."
            }
            fn begins_with_loopback(&self) -> bool {
                true
            }
            fn redirect_ports(&self) -> RedirectPorts {
                RedirectPorts {
                    host: "127.0.0.1",
                    primary: 51121,
                    fallback: None,
                    path: "/oauth-callback",
                }
            }
            fn requires_pkce(&self) -> bool {
                false
            }
            fn validates_redirect_port(&self) -> bool {
                true
            }
            fn authorization_url(
                &self,
                state: &str,
                _pkce: Option<&Pkce>,
                _port: u16,
            ) -> crate::oauth::Result<String> {
                Ok(format!("https://example.test/authorize?state={state}"))
            }
            async fn exchange_code(
                &self,
                _code: &str,
                _pkce: Option<&Pkce>,
                _port: u16,
            ) -> crate::oauth::Result<TokenSet> {
                Ok(TokenSet::new("access"))
            }
            async fn refresh(&self, _refresh_token: &str) -> crate::oauth::Result<TokenSet> {
                Ok(TokenSet::new("refreshed"))
            }
            async fn account_identity(
                &self,
                _tokens: &TokenSet,
            ) -> crate::oauth::Result<AccountIdentity> {
                Ok(AccountIdentity {
                    id: "fixed".into(),
                    label: "Fixed".into(),
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
                &[]
            }
            fn refresh_lead(&self) -> Duration {
                Duration::from_secs(0)
            }
        }

        let platform: &'static FixedPort = Box::leak(Box::new(FixedPort));
        let sessions = OAuthSessions::new();
        let (first, _) = sessions
            .start_browser(platform, "provider-1".into())
            .await
            .expect("the first attempt binds the port");
        let (second, _) = sessions
            .start_browser(platform, "provider-1".into())
            .await
            .unwrap_or_else(|e| {
                panic!("the second attempt must not fail on the port the first one left bound: {e}")
            });
        assert_ne!(first, second);
    }

    /// A platform whose login finishes on its own page, with the poll outcomes scripted so a test
    /// can name the exact sequence the provider would produce.
    struct FakeScreen {
        outcome: std::sync::Mutex<std::collections::VecDeque<DevicePoll>>,
        polls: std::sync::atomic::AtomicUsize,
    }

    impl FakeScreen {
        fn scripted(outcome: Vec<DevicePoll>) -> &'static Self {
            Box::leak(Box::new(Self {
                outcome: std::sync::Mutex::new(outcome.into()),
                polls: std::sync::atomic::AtomicUsize::new(0),
            }))
        }

        fn polls(&self) -> usize {
            self.polls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl OAuthPlatform for FakeScreen {
        fn id(&self) -> &'static str {
            "fake-screen"
        }
        fn label(&self) -> &'static str {
            "Fake screen"
        }
        fn description(&self) -> &'static str {
            "Test double whose login finishes on its own page."
        }
        fn begins_with_loopback(&self) -> bool {
            false
        }
        fn begins_with_screen_login(&self) -> bool {
            true
        }
        fn redirect_ports(&self) -> RedirectPorts {
            RedirectPorts {
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
            _state: &str,
            _pkce: Option<&Pkce>,
            _port: u16,
        ) -> crate::oauth::Result<String> {
            Err(crate::oauth::oauth_error("no redirect flow"))
        }
        async fn exchange_code(
            &self,
            _code: &str,
            _pkce: Option<&Pkce>,
            _port: u16,
        ) -> crate::oauth::Result<TokenSet> {
            Err(crate::oauth::oauth_error("no redirect flow"))
        }
        async fn refresh(&self, _refresh_token: &str) -> crate::oauth::Result<TokenSet> {
            Ok(TokenSet::new("refreshed"))
        }
        async fn screen_login_start(&self) -> crate::oauth::Result<ScreenLogin> {
            Ok(ScreenLogin::new(
                "https://example.test/login?challenge=abc",
                "session-1",
                "ticket-1",
            ))
        }
        async fn screen_login_poll(
            &self,
            _login: &ScreenLogin,
        ) -> crate::oauth::Result<DevicePoll> {
            self.polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(self
                .outcome
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(DevicePoll::Pending))
        }
        async fn account_identity(
            &self,
            _tokens: &TokenSet,
        ) -> crate::oauth::Result<AccountIdentity> {
            Ok(AccountIdentity {
                id: "acct-screen".into(),
                label: "Screen Account".into(),
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
            &[]
        }
        fn refresh_lead(&self) -> Duration {
            Duration::from_secs(0)
        }
    }

    /// The three failures need three different answers over HTTP: a dead session is final, a busy
    /// one is worth retrying, and a mismatched binding will never work. Collapsing them into one
    /// message is what makes a panel poll a dead id until its own deadline.
    #[tokio::test]
    async fn a_binding_failure_says_whether_to_retry() {
        let sessions = OAuthSessions::new();
        let platform = FakeScreen::scripted(vec![]);
        let (id, _) = sessions
            .start_screen(platform, "p1".into())
            .await
            .expect("the screen flow starts");

        sessions
            .validate_binding(&id, "p1", platform.id())
            .await
            .expect("the session validates against the provider that started it");
        assert_eq!(
            sessions
                .validate_binding("no-such-session", "p1", platform.id())
                .await,
            Err(BindingError::Gone),
            "a missing session is gone, not mismatched"
        );
        assert_eq!(
            sessions.validate_binding(&id, "p2", platform.id()).await,
            Err(BindingError::Mismatch),
            "another provider's id is a mismatch"
        );
        assert_eq!(
            sessions.validate_binding(&id, "p1", "some-other").await,
            Err(BindingError::Mismatch),
            "another platform's id is a mismatch"
        );

        sessions.mark_busy(&id).await;
        assert_eq!(
            sessions.validate_binding(&id, "p1", platform.id()).await,
            Err(BindingError::Busy),
            "a poll already in flight is busy, not gone"
        );
    }

    /// The two flows a screen-login platform must refuse, and the refusal has to name the flow that
    /// would actually work — an operator who clicked the wrong button needs to be told which one is
    /// right, not that something is unsupported.
    #[tokio::test]
    async fn a_screen_login_opens_its_page_and_the_other_flows_refuse_it() {
        let sessions = OAuthSessions::new();
        let platform = FakeScreen::scripted(vec![]);
        let (id, url) = sessions
            .start_screen(platform, "p1".into())
            .await
            .expect("the screen flow starts");
        assert!(!id.is_empty());
        assert_eq!(url, "https://example.test/login?challenge=abc");

        let browser = sessions.start_browser(platform, "p1".into()).await;
        assert!(
            browser
                .as_ref()
                .is_err_and(|error| error.contains("own page")),
            "a browser start must name the page flow, got {browser:?}"
        );
        let device = sessions.start_device(platform, "p1".into()).await;
        assert!(
            device
                .as_ref()
                .is_err_and(|error| error.contains("own page")),
            "a device start must name the page flow, got {device:?}"
        );

        // And the reverse: a platform without the flow cannot be started as one.
        let wrong = sessions
            .start_screen(FakeBrowser::leaked(), "p1".into())
            .await;
        assert!(
            wrong.as_ref().is_err_and(|error| error.contains("browser")),
            "a non-screen platform must be refused, got {wrong:?}"
        );
    }

    /// The panel polls once, sees `complete`, and may poll again before it stops — the second answer
    /// must be the same login rather than an error, so a replayed poll cannot log the operator out.
    #[tokio::test]
    async fn a_completed_screen_login_replays_its_tokens_without_asking_again() {
        let sessions = OAuthSessions::new();
        let platform = FakeScreen::scripted(vec![DevicePoll::Success(TokenSet::new("access-1"))]);
        let (id, _) = sessions
            .start_screen(platform, "p1".into())
            .await
            .expect("start");

        let first = sessions.poll_screen(&id, "p1").await.expect("poll");
        match first {
            PollResult::Complete { tokens, identity } => {
                assert_eq!(tokens.access_token, "access-1");
                assert_eq!(identity.id, "acct-screen");
            }
            other => panic!("expected completion, got {other:?}"),
        }
        let second = sessions.poll_screen(&id, "p1").await.expect("replay");
        assert!(
            matches!(second, PollResult::Complete { .. }),
            "a replayed poll must replay the completion, got {second:?}"
        );
        assert_eq!(platform.polls(), 1, "the provider must be asked once");
    }

    /// A page that is still open is not an error, and one poll per request is the most the provider
    /// will accept: the second poll inside the interval is answered locally.
    #[tokio::test]
    async fn a_pending_screen_poll_waits_its_turn_instead_of_hammering_the_page() {
        let sessions = OAuthSessions::new();
        let platform = FakeScreen::scripted(vec![
            DevicePoll::Pending,
            DevicePoll::Success(TokenSet::new("access-2")),
        ]);
        let (id, _) = sessions
            .start_screen(platform, "p1".into())
            .await
            .expect("start");

        match sessions.poll_screen(&id, "p1").await.expect("first") {
            PollResult::Pending {
                retry_after_seconds,
            } => assert_eq!(retry_after_seconds, SCREEN_POLL_START.as_secs()),
            other => panic!("expected pending, got {other:?}"),
        }
        match sessions.poll_screen(&id, "p1").await.expect("second") {
            PollResult::Pending {
                retry_after_seconds,
            } => assert!(
                retry_after_seconds >= 1,
                "a refused poll still says when to retry"
            ),
            other => panic!("expected pending, got {other:?}"),
        }
        assert_eq!(platform.polls(), 1, "the interval is enforced locally");
    }

    /// A failure the provider reports is a message, not the end of the session: the operator may
    /// still finish the page, so the session stays until its TTL and the message is bounded.
    #[tokio::test]
    async fn a_failed_screen_poll_keeps_the_session_and_bounds_the_message() {
        let sessions = OAuthSessions::new();
        let long = "x".repeat(600);
        let platform = FakeScreen::scripted(vec![DevicePoll::Failed { message: long }]);
        let (id, _) = sessions
            .start_screen(platform, "p1".into())
            .await
            .expect("start");

        match sessions.poll_screen(&id, "p1").await.expect("first") {
            PollResult::Failed { message } => assert_eq!(message.chars().count(), 240),
            other => panic!("expected a bounded failure, got {other:?}"),
        }
        // Still there, and still rate-limited: the operator can finish the page and be let in.
        assert!(matches!(
            sessions.poll_screen(&id, "p1").await.expect("second"),
            PollResult::Pending { .. }
        ));
    }

    /// The cadence stretches and stops at its ceiling, and never falls below its floor — a poke at a
    /// stalled page costs one request every ten seconds at worst.
    #[test]
    fn the_screen_poll_cadence_stretches_toward_its_ceiling() {
        // A float multiply, so this is a window rather than an exact millisecond.
        let stretched = grow_screen_interval(SCREEN_POLL_START);
        assert!(
            stretched >= Duration::from_millis(1_190) && stretched < Duration::from_millis(1_210),
            "one second must stretch toward the ceiling, got {stretched:?}"
        );
        assert_eq!(
            grow_screen_interval(Duration::from_secs(9)),
            SCREEN_POLL_MAX
        );
        assert_eq!(grow_screen_interval(SCREEN_POLL_MAX), SCREEN_POLL_MAX);
        assert_eq!(
            grow_screen_interval(Duration::from_millis(1)),
            SCREEN_POLL_START
        );
    }

    #[tokio::test]
    async fn a_redirect_for_an_unknown_session_is_refused() {
        let sessions = OAuthSessions::new();
        let error = sessions
            .submit_redirect("no-such-session", "provider-1", "?code=c&state=s")
            .await
            .expect_err("an unknown session cannot accept a redirect");
        assert!(error.contains("no longer waiting"), "{error}");
    }

    #[tokio::test]
    async fn a_redirect_for_another_provider_is_refused() {
        let platform = FakeBrowser::leaked();
        let sessions = OAuthSessions::new();
        let (id, url) = sessions
            .start_browser(platform, "provider-1".into())
            .await
            .expect("a loopback flow starts");
        let state = state_of(&url);

        let error = sessions
            .submit_redirect(&id, "provider-2", &format!("?code=c&state={state}"))
            .await
            .expect_err("the provider binding must match");
        assert!(error.contains("binding does not match"), "{error}");
    }

    /// A provider message reaches the operator whole, and an oversized one keeps both ends.
    ///
    /// The 240-character ceiling that used to be here is why a `Cloud Code` sign-in failure was
    /// seen in the panel ending "... refused: free-tier (VALIDATION_REQUIRED): Your current ac":
    /// the tier and the code that explains it were in the part that was cut.
    #[test]
    fn safe_error_keeps_a_provider_message_whole_and_both_ends_of_a_flood() {
        let reason = format!(
            "invalid state: oauth: Cloud Code refused this account; refused: free-tier \
             (VALIDATION_REQUIRED): {}",
            "Google wants this account verified. ".repeat(8)
        );
        let kept = safe_error(&reason);
        assert_eq!(
            kept, reason,
            "a message under the ceiling is passed through whole"
        );
        assert!(kept.contains("VALIDATION_REQUIRED"), "{kept}");

        let flood = safe_error(format!("head {} tail-marker", "x".repeat(4000)));
        assert!(flood.chars().count() <= 1000, "{}", flood.chars().count());
        assert!(flood.starts_with("head "), "the head must survive: {flood}");
        assert!(
            flood.ends_with("tail-marker"),
            "the tail must survive: {flood}"
        );
    }
}
