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
        let listener = Loopback::bind(
            platform.redirect_ports(),
            platform.validates_redirect_port(),
        )
        .await
        .map_err(safe_error)?;
        let state = super::flow::new_state();
        let pkce = platform.requires_pkce().then(Pkce::generate);
        let url = platform
            .authorization_url(&state, pkce.as_ref(), listener.port())
            .map_err(safe_error)?;
        let id = self
            .insert(Pending {
                platform,
                kind: PendingKind::Browser {
                    state,
                    pkce,
                    listener,
                },
                expires: tokio::time::Instant::now() + SESSION_TTL,
                provider_id,
                last_polled: None,
                interval: Duration::from_secs(5),
                busy: false,
            })
            .await?;
        Ok((id, url))
    }

    pub async fn start_device(
        &self,
        platform: &'static dyn OAuthPlatform,
        provider_id: String,
    ) -> Result<(String, DeviceAuthorization), String> {
        if platform.begins_with_loopback() {
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

    pub async fn finish_browser(
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
            let tokens = tokens.clone();
            let platform = pending.platform;
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
        let callback: Callback = match listener
            .await_callback(&state, pending.platform.label(), SESSION_TTL)
            .await
        {
            Ok(callback) => callback,
            Err(error) => return Err(safe_error(error)),
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
        let outcome = match pending
            .platform
            .device_poll(&authorization.device_code)
            .await
        {
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
