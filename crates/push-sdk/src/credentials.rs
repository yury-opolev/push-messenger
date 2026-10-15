//! The signed-in device's credentials (spec §5.1, §5.3): kept in the app's secret store,
//! refreshed before they expire, and saved before they are used.
//!
//! Refreshes are serialised, so a refresh token is never presented twice by this client:
//! auth-svc rotates it on every refresh and treats a second use after its 30 s grace as
//! theft, revoking the device's tokens. A caller that saw an expired access token asks
//! for a refresh naming it; if another caller refreshed meanwhile, the newer token is
//! returned without a second refresh.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use uuid::Uuid;

use crate::SdkError;
use crate::auth::{AuthApi, Tokens};
use crate::secrets::{REGISTRATION_KEY, SESSION_KEY, SecretStore, Session};

/// An access token with less than this left is refreshed before it is used: the gateway
/// warns 60 s before expiry, and a token used for a connection should outlast that.
pub(crate) const MIN_ACCESS_LIFETIME_S: i64 = 90;

pub(crate) struct Credentials {
    inner: Arc<Inner>,
}

struct Inner {
    secrets: Arc<dyn SecretStore>,
    auth: AuthApi,
    refreshing: tokio::sync::Mutex<()>,
}

impl Credentials {
    pub fn new(secrets: Arc<dyn SecretStore>, auth: AuthApi) -> Self {
        Self {
            inner: Arc::new(Inner {
                secrets,
                auth,
                refreshing: tokio::sync::Mutex::new(()),
            }),
        }
    }

    /// The stored session, if a device is signed in.
    pub fn session(&self) -> Result<Option<Session>, SdkError> {
        self.inner.session()
    }

    /// Keeps the tokens of a fresh sign-in.
    pub fn sign_in(&self, tokens: Tokens) -> Result<Session, SdkError> {
        self.inner.sign_in(tokens)
    }

    /// Forgets the session. Waits for a refresh in flight, so that it cannot save the
    /// session again afterwards.
    pub async fn sign_out(&self) -> Result<(), SdkError> {
        let _refreshing = self.inner.refreshing.lock().await;
        self.inner.forget_session()
    }

    /// A session whose access token is good for at least [`MIN_ACCESS_LIFETIME_S`],
    /// refreshed first if needed. [`SdkError::NotSignedIn`] when no device is signed in
    /// or auth-svc refuses the refresh token (the session is forgotten then).
    ///
    /// The work runs in its own task: a caller that goes away mid-refresh cannot lose
    /// the tokens auth-svc has already rotated.
    pub async fn fresh(&self) -> Result<Session, SdkError> {
        let inner = self.inner.clone();
        Self::finish(tokio::spawn(async move { inner.fresh().await })).await
    }

    /// A session with an access token other than `stale`, which the server refused or
    /// is about to expire: refreshed now unless another caller already did. Runs in
    /// its own task like [`Credentials::fresh`].
    pub async fn refresh(&self, stale: &str) -> Result<Session, SdkError> {
        let inner = self.inner.clone();
        let stale = stale.to_owned();
        Self::finish(tokio::spawn(async move { inner.refresh(&stale).await })).await
    }

    async fn finish(
        task: tokio::task::JoinHandle<Result<Session, SdkError>>,
    ) -> Result<Session, SdkError> {
        task.await
            .unwrap_or_else(|_| Err(SdkError::Network("the refresh was interrupted".to_owned())))
    }

    /// The `registration_id` of the device registration in progress, created and kept
    /// before the first attempt so that every retry, even after a restart, names it.
    pub fn registration_id(&self) -> Result<Uuid, SdkError> {
        self.inner.registration_id()
    }

    /// The registration was answered: the next one is a new device.
    pub fn registration_done(&self) -> Result<(), SdkError> {
        self.inner.registration_done()
    }
}

impl Inner {
    fn session(&self) -> Result<Option<Session>, SdkError> {
        Session::load(self.secrets.as_ref()).map_err(|error| SdkError::Secrets(error.0))
    }

    fn sign_in(&self, tokens: Tokens) -> Result<Session, SdkError> {
        let session = Session {
            user_id: tokens.user_id,
            device_id: tokens.device_id,
            access_token: tokens.access_token,
            access_expires_at: unix_now() + tokens.expires_in_s,
            refresh_token: tokens.refresh_token,
        };
        self.save(&session)?;
        Ok(session)
    }

    fn forget_session(&self) -> Result<(), SdkError> {
        self.secrets
            .delete(SESSION_KEY)
            .map_err(|error| SdkError::Secrets(error.0))
    }

    async fn fresh(&self) -> Result<Session, SdkError> {
        let _refreshing = self.refreshing.lock().await;
        let session = self.session()?.ok_or(SdkError::NotSignedIn)?;
        if has_lifetime(&session) {
            return Ok(session);
        }
        self.refresh_locked(session).await
    }

    async fn refresh(&self, stale: &str) -> Result<Session, SdkError> {
        let _refreshing = self.refreshing.lock().await;
        let session = self.session()?.ok_or(SdkError::NotSignedIn)?;
        if session.access_token != stale && has_lifetime(&session) {
            return Ok(session);
        }
        self.refresh_locked(session).await
    }

    async fn refresh_locked(&self, session: Session) -> Result<Session, SdkError> {
        match self.auth.refresh(&session.refresh_token).await {
            Ok(tokens)
                if tokens.user_id == session.user_id && tokens.device_id == session.device_id =>
            {
                // Saved before it is used: a token the store lost could not be refreshed.
                self.sign_in(tokens)
            }
            Ok(_) => Err(SdkError::Network(
                "auth-svc refreshed another device's session".to_owned(),
            )),
            Err(error) if is_refused(&error) => {
                tracing::info!(
                    code = error.code(),
                    "the refresh token was refused: signed out"
                );
                self.forget_session()?;
                Err(SdkError::NotSignedIn)
            }
            Err(error) => Err(error),
        }
    }

    fn save(&self, session: &Session) -> Result<(), SdkError> {
        session
            .save(self.secrets.as_ref())
            .map_err(|error| SdkError::Secrets(error.0))
    }

    fn registration_id(&self) -> Result<Uuid, SdkError> {
        let stored = self
            .secrets
            .load(REGISTRATION_KEY)
            .map_err(|error| SdkError::Secrets(error.0))?
            .and_then(|bytes| Uuid::from_slice(&bytes).ok());
        if let Some(id) = stored {
            return Ok(id);
        }
        let id = Uuid::now_v7();
        self.secrets
            .save(REGISTRATION_KEY, id.as_bytes())
            .map_err(|error| SdkError::Secrets(error.0))?;
        Ok(id)
    }

    fn registration_done(&self) -> Result<(), SdkError> {
        self.secrets
            .delete(REGISTRATION_KEY)
            .map_err(|error| SdkError::Secrets(error.0))
    }
}

fn has_lifetime(session: &Session) -> bool {
    session.access_expires_at - unix_now() >= MIN_ACCESS_LIFETIME_S
}

/// auth-svc refused the credentials for good: they were revoked, reused or expired.
pub(crate) fn is_refused(error: &SdkError) -> bool {
    matches!(error, SdkError::Server { code, .. }
        if matches!(code.as_str(), "unauthorized" | "token_expired" | "http_401"))
}

pub(crate) fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Router;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::post;
    use push_domain::{DeviceId, UserId};
    use serde_json::json;

    use super::*;
    use crate::MemorySecretStore;

    struct Fixture {
        credentials: Arc<Credentials>,
        secrets: Arc<MemorySecretStore>,
        calls: Arc<AtomicUsize>,
        session: Session,
    }

    /// auth-svc answering refreshes with new tokens for the same device (or refusing
    /// them), and a stored session whose access token expires in `left` seconds.
    async fn fixture(left: i64, refuse: bool) -> Fixture {
        fixture_slow(left, refuse, 50).await
    }

    async fn fixture_slow(left: i64, refuse: bool, delay_ms: u64) -> Fixture {
        let session = Session {
            user_id: UserId::new(),
            device_id: DeviceId::new(),
            access_token: "access-0".to_owned(),
            access_expires_at: unix_now() + left,
            refresh_token: "refresh-0".to_owned(),
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let (counter, user, device) = (calls.clone(), session.user_id, session.device_id);
        let router = Router::new().route(
            "/v1/auth/refresh",
            post(move || {
                let counter = counter.clone();
                async move {
                    let call = counter.fetch_add(1, Ordering::SeqCst) + 1;
                    // Slow enough for a concurrent caller to queue behind it.
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                    if refuse {
                        let error = json!({"error": {"code": "unauthorized", "message": "sign in again", "retryable": false}});
                        return (StatusCode::UNAUTHORIZED, axum::Json(error)).into_response();
                    }
                    axum::Json(json!({
                        "user_id": user,
                        "device_id": device,
                        "access_token": format!("access-{call}"),
                        "expires_in_s": 900,
                        "refresh_token": format!("refresh-{call}"),
                    }))
                    .into_response()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let secrets = Arc::new(MemorySecretStore::new());
        session.save(secrets.as_ref()).unwrap();
        let credentials = Arc::new(Credentials::new(
            secrets.clone(),
            AuthApi::new(&url, None).unwrap(),
        ));
        Fixture {
            credentials,
            secrets,
            calls,
            session,
        }
    }

    #[tokio::test]
    async fn a_fresh_token_is_used_as_it_is() {
        let fixture = fixture(600, false).await;
        let session = fixture.credentials.fresh().await.unwrap();
        assert_eq!(session, fixture.session);
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn an_expiring_token_is_refreshed_and_saved_before_it_is_used() {
        let fixture = fixture(30, false).await;
        let session = fixture.credentials.fresh().await.unwrap();
        assert_eq!(session.access_token, "access-1");
        assert_eq!(session.refresh_token, "refresh-1");
        assert!(session.access_expires_at >= unix_now() + 890);
        let stored = Session::load(fixture.secrets.as_ref()).unwrap();
        assert_eq!(stored, Some(session));
    }

    #[tokio::test]
    async fn concurrent_refreshes_of_one_token_refresh_once() {
        let fixture = fixture(600, false).await;
        let (first, second) = tokio::join!(
            fixture.credentials.refresh("access-0"),
            fixture.credentials.refresh("access-0"),
        );
        assert_eq!(first.unwrap().access_token, "access-1");
        assert_eq!(second.unwrap().access_token, "access-1");
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_refresh_finishes_when_its_caller_goes_away() {
        let fixture = fixture_slow(30, false, 300).await;
        let cut_off = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            fixture.credentials.fresh(),
        )
        .await;
        assert!(cut_off.is_err());
        let mut stored = None;
        for _ in 0..100 {
            stored = Session::load(fixture.secrets.as_ref()).unwrap();
            if stored
                .as_ref()
                .is_some_and(|s| s.access_token == "access-1")
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let stored = stored.unwrap();
        assert_eq!(stored.access_token, "access-1");
        assert_eq!(stored.refresh_token, "refresh-1");
        assert_eq!(fixture.credentials.fresh().await.unwrap(), stored);
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_newer_token_that_is_about_to_expire_is_refreshed_again() {
        let fixture = fixture(30, false).await;
        let session = fixture.credentials.refresh("access-other").await.unwrap();
        assert_eq!(session.access_token, "access-1");
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_newer_token_with_time_left_is_returned_without_a_refresh() {
        let fixture = fixture(600, false).await;
        let session = fixture.credentials.refresh("access-other").await.unwrap();
        assert_eq!(session, fixture.session);
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn a_refused_refresh_signs_out() {
        let fixture = fixture(30, true).await;
        assert_eq!(
            fixture.credentials.fresh().await,
            Err(SdkError::NotSignedIn)
        );
        assert_eq!(Session::load(fixture.secrets.as_ref()), Ok(None));
        assert_eq!(
            fixture.credentials.fresh().await,
            Err(SdkError::NotSignedIn)
        );
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_registration_keeps_its_id_until_it_is_answered() {
        let fixture = fixture(600, false).await;
        let first = fixture.credentials.registration_id().unwrap();
        assert_eq!(fixture.credentials.registration_id().unwrap(), first);
        fixture.credentials.registration_done().unwrap();
        assert_ne!(fixture.credentials.registration_id().unwrap(), first);
    }
}
