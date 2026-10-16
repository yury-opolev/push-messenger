use anyhow::Context;
use tokio::net::TcpListener;

use crate::Config;

/// The service's listeners, bound together before any server starts.
pub struct Listeners {
    pub messaging: TcpListener,
    pub accounts: TcpListener,
    pub directory: TcpListener,
    pub health: TcpListener,
}

impl Listeners {
    /// Binds every listener, releasing earlier bindings if any bind fails.
    pub async fn bind(config: &Config) -> anyhow::Result<Self> {
        let messaging = TcpListener::bind(config.grpc_addr)
            .await
            .context("bind Messaging listener")?;
        let accounts = TcpListener::bind(config.accounts_grpc_addr)
            .await
            .context("bind Accounts listener")?;
        let directory = TcpListener::bind(config.directory_grpc_addr)
            .await
            .context("bind Directory listener")?;
        let health = TcpListener::bind(config.health_addr)
            .await
            .context("bind health listener")?;
        Ok(Self {
            messaging,
            accounts,
            directory,
            health,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn config() -> Config {
        Config::from_lookup(|name| {
            match name {
                "PUSH_DATABASE_URL" => Some("postgres://localhost/push_messaging"),
                "PUSH_REDIS_URL" => Some("redis://localhost:6379"),
                "PUSH_GRPC_ADDR"
                | "PUSH_ACCOUNTS_GRPC_ADDR"
                | "PUSH_DIRECTORY_GRPC_ADDR"
                | "PUSH_HEALTH_ADDR" => Some("127.0.0.1:0"),
                _ => None,
            }
            .map(str::to_owned)
        })
        .unwrap()
    }

    #[tokio::test]
    async fn binds_four_distinct_listeners() {
        let listeners = Listeners::bind(&config()).await.unwrap();
        let addresses: HashSet<_> = [
            &listeners.messaging,
            &listeners.accounts,
            &listeners.directory,
            &listeners.health,
        ]
        .map(|listener| listener.local_addr().unwrap())
        .into_iter()
        .collect();
        assert_eq!(addresses.len(), 4);
    }

    #[tokio::test]
    async fn accounts_bind_failure_releases_messaging_listener() {
        let reservation = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let messaging_addr = reservation.local_addr().unwrap();
        let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = config();
        config.grpc_addr = messaging_addr;
        config.accounts_grpc_addr = occupied.local_addr().unwrap();
        drop(reservation);

        let error = Listeners::bind(&config)
            .await
            .err()
            .expect("bind must fail");
        assert!(
            error.to_string().contains("bind Accounts listener"),
            "{error:#}"
        );
        let _rebound = TcpListener::bind(messaging_addr).await.unwrap();
    }
}
