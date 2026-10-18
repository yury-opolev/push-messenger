//! Start-up helpers shared by the services.

use std::future::Future;
use std::time::Duration;

use ::redis::RedisError;
use tokio_util::sync::CancellationToken;

use crate::{AuthMode, AzureConfig, ConfigProblem, is_permanent_redis};

/// The identity variables, read only when one of `modes` is `azure-entra`.
pub fn azure_for(
    lookup: &dyn Fn(&str) -> Option<String>,
    modes: &[AuthMode],
) -> Result<Option<AzureConfig>, ConfigProblem> {
    if modes.contains(&AuthMode::AzureEntra) {
        AzureConfig::from_lookup(lookup).map(Some)
    } else {
        Ok(None)
    }
}

/// Whether `error`'s chain holds a Redis error that waiting will not fix
/// ([`crate::is_permanent_redis`]).
pub fn redis_permanent(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<RedisError>()
            .is_some_and(is_permanent_redis)
    })
}

/// Calls `connect` until it succeeds, logging each failure, and waits `delay` between
/// attempts. `Ok(None)` if `shutdown` came first; `Err` at once for an error `permanent`
/// recognises (a refused token, an untrusted certificate: configuration, not an outage).
pub async fn retry<T, F, Fut>(
    what: &str,
    delay: Duration,
    shutdown: &CancellationToken,
    permanent: &dyn Fn(&anyhow::Error) -> bool,
    connect: F,
) -> anyhow::Result<Option<T>>
where
    F: Fn() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    loop {
        let attempt = tokio::select! {
            () = shutdown.cancelled() => return Ok(None),
            attempt = connect() => attempt,
        };
        match attempt {
            Ok(value) => {
                tracing::info!(dependency = what, "connected");
                return Ok(Some(value));
            }
            Err(error) if permanent(&error) => {
                tracing::error!(dependency = what, error = %format!("{error:#}"), "refused; not retrying");
                return Err(error.context(what.to_owned()));
            }
            Err(error) => {
                tracing::warn!(dependency = what, error = %format!("{error:#}"), "not reachable yet; retrying");
            }
        }
        tokio::select! {
            () = shutdown.cancelled() => return Ok(None),
            () = tokio::time::sleep(delay) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn retry_retries_outages_stops_on_permanent_errors_and_on_shutdown() {
        let calls = Arc::new(AtomicUsize::new(0));
        let attempts = {
            let calls = Arc::clone(&calls);
            move || {
                let calls = Arc::clone(&calls);
                async move {
                    match calls.fetch_add(1, Ordering::SeqCst) {
                        0 | 1 => Err(anyhow::anyhow!("outage")),
                        _ => Ok(7),
                    }
                }
            }
        };
        let shutdown = CancellationToken::new();
        let never = |_: &anyhow::Error| false;
        let value = retry("db", Duration::from_secs(2), &shutdown, &never, attempts)
            .await
            .unwrap();
        assert_eq!((value, calls.load(Ordering::SeqCst)), (Some(7), 3));

        let always = |_: &anyhow::Error| true;
        let failed = retry("db", Duration::from_secs(2), &shutdown, &always, || async {
            Err::<(), _>(anyhow::anyhow!("refused"))
        })
        .await;
        assert_eq!(format!("{:#}", failed.unwrap_err()), "db: refused");

        shutdown.cancel();
        let stopped = retry("db", Duration::from_secs(2), &shutdown, &never, || async {
            Err::<(), _>(anyhow::anyhow!("outage"))
        })
        .await;
        assert!(stopped.unwrap().is_none());
    }

    #[test]
    fn the_identity_is_read_only_when_a_datastore_uses_entra() {
        let lookup = |_: &str| None;
        assert_eq!(
            azure_for(&lookup, &[AuthMode::Password, AuthMode::Password]),
            Ok(None)
        );
        let config = azure_for(&lookup, &[AuthMode::Password, AuthMode::AzureEntra])
            .unwrap()
            .unwrap();
        assert!(config.federated_token_file.is_none());
    }
}
