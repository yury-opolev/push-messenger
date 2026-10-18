//! A scope's current access token: fetched at start-up (failing start-up when it cannot
//! be had), refreshed at 75% of its lifetime, and published on a `watch` channel. A
//! failed refresh keeps the old token and is retried after 1 s, doubling to 30 s.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::token::{AccessToken, TokenError, TokenSource};

const FIRST_RETRY: Duration = Duration::from_secs(1);
const MAX_RETRY: Duration = Duration::from_secs(30);

/// The current token of one scope. Cheap to clone.
#[derive(Clone)]
pub struct Tokens {
    receiver: watch::Receiver<AccessToken>,
}

impl Tokens {
    /// Fetches the first token, then refreshes in the background until `shutdown` or
    /// until every clone is dropped.
    pub async fn start(
        source: Arc<dyn TokenSource>,
        scope: &str,
        shutdown: CancellationToken,
    ) -> Result<Self, TokenError> {
        let first = source.fetch(scope).await?;
        let (sender, receiver) = watch::channel(first);
        tokio::spawn(refresh(source, scope.to_owned(), sender, shutdown));
        Ok(Self { receiver })
    }

    pub fn current(&self) -> AccessToken {
        self.receiver.borrow().clone()
    }

    /// A receiver that sees every later token (the current one counts as seen).
    pub fn watch(&self) -> watch::Receiver<AccessToken> {
        self.receiver.clone()
    }
}

/// When `token` is refreshed: after 75% of its lifetime.
pub fn refresh_at(token: &AccessToken) -> Instant {
    token.issued_at() + (token.expires_at() - token.issued_at()) * 3 / 4
}

async fn refresh(
    source: Arc<dyn TokenSource>,
    scope: String,
    sender: watch::Sender<AccessToken>,
    shutdown: CancellationToken,
) {
    loop {
        let due = refresh_at(&sender.borrow());
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = sender.closed() => return,
            () = tokio::time::sleep_until(due) => {}
        }
        let mut retry = FIRST_RETRY;
        loop {
            let fetched = tokio::select! {
                () = shutdown.cancelled() => return,
                fetched = source.fetch(&scope) => fetched,
            };
            match fetched {
                Ok(token) => {
                    sender.send_replace(token);
                    tracing::debug!(%scope, "refreshed an access token");
                    break;
                }
                Err(error) => {
                    if sender.borrow().is_expired() {
                        tracing::error!(%scope, %error, retry_in = ?retry, "the access token expired and cannot be refreshed");
                    } else {
                        tracing::warn!(%scope, %error, retry_in = ?retry, "cannot refresh the access token yet");
                    }
                    tokio::select! {
                        () = shutdown.cancelled() => return,
                        () = tokio::time::sleep(retry) => {}
                    }
                    retry = (retry * 2).min(MAX_RETRY);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use super::*;

    type Answer = Result<(&'static str, u64), TokenError>;

    /// Answers fetches from a script; counts them.
    struct Script {
        answers: Mutex<VecDeque<Answer>>,
        fetches: AtomicUsize,
    }

    impl Script {
        fn new(answers: Vec<Answer>) -> Arc<Self> {
            Arc::new(Self {
                answers: Mutex::new(answers.into()),
                fetches: AtomicUsize::new(0),
            })
        }
        fn fetches(&self) -> usize {
            self.fetches.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl TokenSource for Script {
        async fn fetch(&self, _scope: &str) -> Result<AccessToken, TokenError> {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            let next = self
                .answers
                .lock()
                .unwrap()
                .pop_front()
                .expect("a scripted answer");
            next.map(|(secret, seconds)| AccessToken::new(secret, Duration::from_secs(seconds)))
        }
    }

    fn down() -> TokenError {
        TokenError::Unreachable("connection failed".to_owned())
    }

    /// Lets the refresher run until it waits on time again.
    async fn settle() {
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn start_fails_when_the_first_token_cannot_be_had() {
        let script = Script::new(vec![Err(down())]);
        let started = Tokens::start(script, "scope/.default", CancellationToken::new()).await;
        assert_eq!(started.err(), Some(down()));
    }

    #[tokio::test(start_paused = true)]
    async fn a_token_is_refreshed_at_three_quarters_of_its_lifetime() {
        let script = Script::new(vec![Ok(("first", 100)), Ok(("second", 100))]);
        let tokens = Tokens::start(script.clone(), "scope/.default", CancellationToken::new())
            .await
            .unwrap();
        let mut watch = tokens.watch();
        assert_eq!(tokens.current().secret(), "first");
        tokio::time::advance(Duration::from_secs(74)).await;
        settle().await;
        assert_eq!(script.fetches(), 1);
        tokio::time::advance(Duration::from_secs(2)).await;
        watch.changed().await.unwrap();
        assert_eq!(tokens.current().secret(), "second");
        assert_eq!(script.fetches(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn failures_keep_the_old_token_and_back_off_up_to_30_s() {
        let mut answers = vec![Ok(("first", 100))];
        answers.extend((0..7).map(|_| Err(down())));
        answers.push(Ok(("second", 100)));
        let script = Script::new(answers);
        let tokens = Tokens::start(script.clone(), "scope/.default", CancellationToken::new())
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(75)).await;
        settle().await;
        assert_eq!(script.fetches(), 2, "the first refresh failed");
        // Retries after 1, 2, 4, 8, 16, 30 and 30 s.
        for (delay, fetches) in [(1, 3), (2, 4), (4, 5), (8, 6), (16, 7), (30, 8)] {
            tokio::time::advance(Duration::from_secs(delay)).await;
            settle().await;
            assert_eq!(script.fetches(), fetches, "after waiting {delay} s");
            assert_eq!(tokens.current().secret(), "first");
        }
        tokio::time::advance(Duration::from_secs(30)).await;
        settle().await;
        assert_eq!(tokens.current().secret(), "second");
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_stops_refreshing() {
        let script = Script::new(vec![Ok(("first", 100))]);
        let shutdown = CancellationToken::new();
        let _tokens = Tokens::start(script.clone(), "scope/.default", shutdown.clone())
            .await
            .unwrap();
        shutdown.cancel();
        tokio::time::advance(Duration::from_secs(1000)).await;
        settle().await;
        assert_eq!(script.fetches(), 1);
    }

    #[test]
    fn refresh_comes_at_75_percent() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let _guard = runtime.enter();
        let token = AccessToken::new("t", Duration::from_secs(3600));
        assert_eq!(
            refresh_at(&token) - token.issued_at(),
            Duration::from_secs(2700)
        );
    }
}
