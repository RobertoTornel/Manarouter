//! Server-owned state for interactive OAuth sessions.
//!
//! A bounded registry owns browser listeners and device-code state. Token results remain server-side
//! until the conductor has durably attached the account.
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use super::flow::{Callback, Loopback};
use super::{DeviceAuthorization, DevicePoll, OAuthPlatform, Pkce, TokenSet};

const SESSION_TTL: Duration = Duration::from_secs(10 * 60);
const MAX_SESSIONS: usize = 64;

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
    BrowserInProgress,
    Device {
        authorization: DeviceAuthorization,
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
            return Err("platform requires device-code authorization".into());
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
                port: redirect_port,
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
            return Err("platform requires browser redirect authorization".into());
        }
        self.ensure_capacity().await?;
        let authorization = platform.device_start().await.map_err(safe_error)?;
        let expires = tokio::time::Instant::now() + authorization.expires_in.min(SESSION_TTL);
        let interval = authorization.interval.max(Duration::from_secs(1));
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

    async fn ensure_capacity(&self) -> Result<(), String> {
        let mut sessions = self.sessions.lock().await;
        sessions.retain(|_, pending| pending.expires > tokio::time::Instant::now());
        if sessions.len() >= MAX_SESSIONS {
            return Err("too many pending OAuth sessions; finish or restart one".into());
        }
        Ok(())
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

    pub async fn validate_binding(
        &self,
        id: &str,
        provider_id: &str,
        platform_id: &str,
    ) -> Result<(), String> {
        let sessions = self.sessions.lock().await;
        let pending = sessions
            .get(id)
            .ok_or_else(|| "OAuth session not found or expired".to_string())?;
        if pending.expires <= tokio::time::Instant::now() {
            return Err("OAuth session expired".into());
        }
        if pending.busy {
            return Err("OAuth session operation already in progress".into());
        }
        if pending.provider_id != provider_id || pending.platform.id() != platform_id {
            return Err("OAuth session provider binding does not match".into());
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
        // The state is not ceremony: it is what ties this code to *our* authorization request.
        // Without it, anyone able to reach the panel could redeem a code from their own flow.
        let state = parsed.state.ok_or_else(|| {
            "the pasted value has no state parameter, so it cannot be tied to this sign-in; paste \
             the whole URL from the browser's address bar"
                .to_string()
        })?;
        if state != slot.state {
            return Err(
                "that redirect belongs to a different sign-in attempt — its state parameter does \
                 not match this session; paste the URL from the attempt you just started"
                    .into(),
            );
        }
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
        let result = self.finish_browser_inner(id, provider_id).await;
        // One exit for every path — success, refusal, expiry — so a stale slot can never outlive
        // the attempt it belongs to.
        self.manual.lock().await.remove(id);
        result
    }

    async fn finish_browser_inner(
        &self,
        id: &str,
        provider_id: &str,
    ) -> Result<(TokenSet, super::AccountIdentity), String> {
        let mut pending = self.take(id, provider_id).await?;
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
        let PendingKind::Browser {
            state,
            pkce,
            listener,
        } = old_kind
        else {
            return Err("OAuth session is not a browser flow".into());
        };
        let slot = self.manual.lock().await.get(id).cloned();
        let callback: Callback = match slot {
            Some(slot) => {
                // `notified()` is armed *before* the select so a paste that lands during setup is
                // not lost: `Notify` holds the permit until somebody waits on it.
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
                        pasted.ok_or_else(|| "the pasted redirect could not be read".to_string())?
                    }
                }
            }
            // No slot means it was already cleaned up; still try the automatic path rather than
            // refusing a sign-in that may well succeed.
            None => listener
                .await_callback(&state, pending.platform.label(), SESSION_TTL)
                .await
                .map_err(safe_error)?,
        };
        let tokens = match pending
            .platform
            .exchange_code(&callback.code, pkce.as_ref(), callback.port)
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
                pending.interval += Duration::from_secs(5);
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

fn safe_error(error: impl std::fmt::Display) -> String {
    error.to_string().chars().take(240).collect()
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
    }

    impl FakeBrowser {
        fn leaked() -> &'static Self {
            Box::leak(Box::new(Self {
                exchanged: std::sync::Mutex::new(Vec::new()),
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
            true
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

    /// A bare code carries no state, so it cannot be tied to this attempt. Saying that plainly is
    /// better than exchanging it and letting the provider produce a confusing error.
    #[tokio::test]
    async fn a_bare_code_is_refused_with_an_explanation() {
        let platform = FakeBrowser::leaked();
        let sessions = OAuthSessions::new();
        let (id, _url) = sessions
            .start_browser(platform, "provider-1".into())
            .await
            .expect("a loopback flow starts");

        let error = sessions
            .submit_redirect(&id, "provider-1", "just-a-code")
            .await
            .expect_err("a code without state cannot be verified");
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
}
