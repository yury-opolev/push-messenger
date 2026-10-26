//! The whole stack in this process, shared by the password (`local.rs`) and the Entra ID
//! (`entra.rs`) tests: auth-svc, the worker, message-svc and the gateway, wired as the
//! chart wires them (each internal API behind its own token), and a router serving both
//! public APIs from one origin as an ingress does. The caller prepares the datastores and
//! passes each service's datastore variables.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::time::Duration;

use push_load::config::{PrepareOptions, RunOptions, Target as LoadTarget};
use push_load::report::Report;
use push_smoke::Target;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub type Server = JoinHandle<Result<(), String>>;

/// Spawns a server; its result as text, so every server's handle has one type.
pub fn spawn<E: std::fmt::Debug + Send + 'static>(
    server: impl std::future::Future<Output = Result<(), E>> + Send + 'static,
) -> Server {
    tokio::spawn(async move { server.await.map_err(|error| format!("{error:?}")) })
}

/// A variable lookup over `variables`, as each service's `Config::from_lookup` takes. A
/// later entry wins, so a test can override what `Stack` sets.
pub fn lookup(variables: Vec<(String, String)>) -> impl Fn(&str) -> Option<String> {
    move |name| {
        variables
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    }
}

fn owned(variables: &[(&str, &str)]) -> Vec<(String, String)> {
    variables
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

/// What differs between the password stack and the Entra stack: each service's
/// datastore variables. `Stack` adds the rest (addresses, internal tokens, keys).
pub struct Datastores {
    pub auth: Vec<(String, String)>,
    pub message_svc: Vec<(String, String)>,
    pub gateway: Vec<(String, String)>,
    pub worker: Vec<(String, String)>,
}

/// The containers the stack runs on; they stop when dropped.
pub type Containers = (
    testcontainers_modules::testcontainers::ContainerAsync<
        testcontainers_modules::postgres::Postgres,
    >,
    testcontainers_modules::testcontainers::ContainerAsync<testcontainers_modules::redis::Redis>,
);

/// Password-mode PostgreSQL and Redis in containers (Docker must be running), with
/// databases for auth-svc and the worker as the chart creates them.
pub async fn password_datastores() -> (Datastores, Containers) {
    use testcontainers_modules::postgres::Postgres;
    use testcontainers_modules::redis::Redis;
    use testcontainers_modules::testcontainers::ImageExt;
    use testcontainers_modules::testcontainers::runners::AsyncRunner;

    let postgres = Postgres::default()
        .with_tag("17-alpine")
        .start()
        .await
        .expect("start Postgres (is Docker running?)");
    let redis = Redis::default()
        .with_tag("7-alpine")
        .start()
        .await
        .expect("start Redis");
    let port = postgres.get_host_port_ipv4(5432).await.unwrap();
    let database_url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let redis_url = format!(
        "redis://127.0.0.1:{}",
        redis.get_host_port_ipv4(6379).await.unwrap()
    );
    // auth-svc and the worker have databases of their own in the same server, as in the
    // chart; the worker's role may also drain auth-svc's outbox, which auth-svc grants.
    let admin = sqlx::PgPool::connect(&database_url).await.unwrap();
    for statement in [
        "CREATE DATABASE push_auth",
        "CREATE ROLE push_worker LOGIN PASSWORD 'worker-secret'",
        "CREATE DATABASE push_worker OWNER push_worker",
    ] {
        sqlx::query(statement).execute(&admin).await.unwrap();
    }
    admin.close().await;
    let auth_database_url = format!("postgres://postgres:postgres@127.0.0.1:{port}/push_auth");
    let worker_server = format!("postgres://push_worker:worker-secret@127.0.0.1:{port}");
    let datastores = Datastores {
        auth: owned(&[
            ("PUSH_DATABASE_URL", &auth_database_url),
            ("PUSH_REDIS_URL", &redis_url),
            ("PUSH_AUTH_WORKER_ROLE", "push_worker"),
        ]),
        message_svc: owned(&[
            ("PUSH_DATABASE_URL", &database_url),
            ("PUSH_REDIS_URL", &redis_url),
        ]),
        gateway: owned(&[("PUSH_REDIS_URL", &redis_url)]),
        worker: owned(&[
            ("PUSH_DATABASE_URL", &format!("{worker_server}/push_worker")),
            (
                "PUSH_AUTH_DATABASE_URL",
                &format!("{worker_server}/push_auth"),
            ),
        ]),
    };
    (datastores, (postgres, redis))
}

pub struct Stack {
    pub auth_url: String,
    pub socket_url: String,
    pub worker_url: String,
    /// auth-svc and the gateway behind one origin, routed by path.
    pub public_url: String,
    pub signing_key: &'static str,
    /// auth-svc's admin listener, never behind `public_url`, and its bearer token.
    pub admin_url: String,
    pub admin_token: &'static str,
    shutdown: CancellationToken,
    servers: Vec<(&'static str, Server)>,
}

// auth-svc's token for message-svc's Directory API, as the chart's Secret holds it.
// message-svc refuses it equal to the Accounts token.
const DIRECTORY_TOKEN: &str = "directory-token-for-the-local-smoke-test";
// The gateway's token for message-svc's Messaging API.
const MESSAGING_TOKEN: &str = "messaging-token-for-the-local-smoke-test";
// The worker's token for message-svc's Accounts API.
const ACCOUNTS_TOKEN: &str = "accounts-token-for-the-local-smoke-test";
// The bearer token of auth-svc's admin API.
const ADMIN_TOKEN: &str = "admin-token-for-the-local-smoke-test";

impl Stack {
    /// Starts the four services on `datastores` and waits until each is ready.
    pub async fn start(datastores: Datastores) -> Self {
        let shutdown = CancellationToken::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gateway_addr = listener.local_addr().unwrap();
        let socket_url = format!("ws://{gateway_addr}/v1/socket");
        let auth_http = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let auth_health = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let auth_health_addr = auth_health.local_addr().unwrap();
        let auth_http_addr = auth_http.local_addr().unwrap();
        let auth_url = format!("http://{auth_http_addr}");
        // auth-svc binds its admin listener itself: reserve a free port for it.
        let admin_addr = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let admin_url = format!("http://{admin_addr}");
        let grpc = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let accounts = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let directory = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let message_svc_url = format!("http://{}", grpc.local_addr().unwrap());
        let accounts_url = format!("http://{}", accounts.local_addr().unwrap());
        let directory_url = format!("http://{}", directory.local_addr().unwrap());
        // base64 of 32 bytes of 0x07: auth-svc's signing key, which the smoke test also gets
        // to sign tokens the gateway must refuse.
        let signing_key = "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=";

        let mut auth_variables = owned(&[
            ("PUSH_AUTH_PUBLIC_URL", "http://push.local"),
            ("PUSH_AUTH_TOKEN_AUDIENCE", &socket_url),
            ("PUSH_AUTH_SIGNING_KEY", signing_key),
            // base64 of 32 bytes of 0x09.
            (
                "PUSH_AUTH_LINK_KEY",
                "CQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQk=",
            ),
            ("PUSH_MESSAGE_SVC_URL", &directory_url),
            ("PUSH_DIRECTORY_TOKEN", DIRECTORY_TOKEN),
            ("PUSH_HTTP_ADDR", &auth_http_addr.to_string()),
            ("PUSH_HEALTH_ADDR", &auth_health_addr.to_string()),
            ("PUSH_AUTH_ADMIN_TOKEN", ADMIN_TOKEN),
            ("PUSH_AUTH_ADMIN_ADDR", &admin_addr.to_string()),
        ]);
        auth_variables.extend(datastores.auth);
        let auth_config = push_auth_core::Config::from_lookup(lookup(auth_variables)).unwrap();
        let auth = spawn(push_auth_core::serve(
            auth_config,
            auth_http,
            auth_health,
            shutdown.clone(),
        ));

        let message_svc_health = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let message_svc_health_addr = message_svc_health.local_addr().unwrap();
        let mut message_svc_variables = owned(&[
            ("PUSH_GRPC_ADDR", &grpc.local_addr().unwrap().to_string()),
            (
                "PUSH_ACCOUNTS_GRPC_ADDR",
                &accounts.local_addr().unwrap().to_string(),
            ),
            (
                "PUSH_DIRECTORY_GRPC_ADDR",
                &directory.local_addr().unwrap().to_string(),
            ),
            ("PUSH_HEALTH_ADDR", &message_svc_health_addr.to_string()),
            ("PUSH_ACCOUNTS_TOKEN", ACCOUNTS_TOKEN),
            ("PUSH_DIRECTORY_TOKEN", DIRECTORY_TOKEN),
            ("PUSH_MESSAGING_TOKEN", MESSAGING_TOKEN),
        ]);
        message_svc_variables.extend(datastores.message_svc);
        let message_svc = spawn(push_message_svc::serve(
            push_message_svc::Config::from_lookup(lookup(message_svc_variables)).unwrap(),
            push_message_svc::Listeners {
                messaging: grpc,
                accounts,
                directory,
                health: message_svc_health,
            },
            shutdown.clone(),
        ));

        let gateway_health = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gateway_health_addr = gateway_health.local_addr().unwrap();
        let mut gateway_variables = owned(&[
            ("PUSH_MESSAGE_SVC_URL", &message_svc_url),
            ("PUSH_MESSAGING_TOKEN", MESSAGING_TOKEN),
            ("PUSH_AUTH_URL", &auth_url),
            ("PUSH_TOKEN_AUDIENCE", &socket_url),
            ("PUSH_LISTEN_ADDR", &gateway_addr.to_string()),
            ("PUSH_HEALTH_ADDR", &gateway_health_addr.to_string()),
        ]);
        gateway_variables.extend(datastores.gateway);
        let gateway = spawn(push_gateway::serve(
            push_gateway::Config::from_lookup(lookup(gateway_variables)).unwrap(),
            listener,
            gateway_health,
            shutdown.clone(),
        ));

        let worker_http = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let worker_health = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let worker_http_addr = worker_http.local_addr().unwrap();
        let worker_health_addr = worker_health.local_addr().unwrap();
        let worker_url = format!("http://{worker_http_addr}");
        let mut worker_variables = owned(&[
            ("PUSH_MESSAGE_SVC_URL", &accounts_url),
            ("PUSH_ACCOUNTS_TOKEN", ACCOUNTS_TOKEN),
            ("PUSH_EMAIL_SENDER", "log"),
            ("PUSH_WORKER_POLL_MS", "250"),
            ("PUSH_HTTP_ADDR", &worker_http_addr.to_string()),
            ("PUSH_HEALTH_ADDR", &worker_health_addr.to_string()),
            ("PUSH_DEV_MAILBOX", "1"),
        ]);
        worker_variables.extend(datastores.worker);
        let worker = spawn(push_worker::serve(
            push_worker::Config::from_lookup(lookup(worker_variables)).unwrap(),
            worker_http,
            worker_health,
            shutdown.clone(),
        ));

        wait_until_ready("auth-svc", auth_health_addr, &auth).await;
        wait_until_ready("the worker", worker_health_addr, &worker).await;
        wait_until_ready("message-svc", message_svc_health_addr, &message_svc).await;
        wait_until_ready("the gateway", gateway_health_addr, &gateway).await;

        let router = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let public_url = format!("http://{}", router.local_addr().unwrap());
        tokio::spawn(route(router, auth_http_addr, gateway_addr));
        Self {
            auth_url,
            socket_url,
            worker_url,
            public_url,
            signing_key,
            admin_url,
            admin_token: ADMIN_TOKEN,
            shutdown,
            servers: vec![
                ("auth-svc", auth),
                ("the worker", worker),
                ("message-svc", message_svc),
                ("the gateway", gateway),
            ],
        }
    }

    pub fn smoke_target(&self, data_dir: String) -> Target {
        Target {
            mailbox_url: self.worker_url.clone(),
            auth_url: self.auth_url.clone(),
            socket_url: self.socket_url.clone(),
            host: None,
            signing_key: self.signing_key.to_owned(),
            data_dir,
        }
    }

    pub async fn stop(self) {
        self.shutdown.cancel();
        for (name, server) in self.servers {
            server
                .await
                .unwrap()
                .unwrap_or_else(|error| panic!("{name} did not stop cleanly: {error}"));
        }
    }
}

/// push-load against `stack` for `duration`: two verified accounts with two devices each
/// and one sample pair, four connections that never reconnect (a dropped connection fails
/// the run), a message every 2 s. Credential files go outside the repository.
pub async fn load_run(stack: &Stack, duration: Duration) -> Report {
    let accounts = push_smoke::create_verified_accounts(&stack.smoke_target(String::new()), 2)
        .await
        .expect("two verified accounts");
    let dir = std::env::temp_dir().join(format!("push-load-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let state_path = dir.join("state.json");
    let authority = stack.public_url.trim_start_matches("http://").to_owned();
    let target = LoadTarget::new(&stack.public_url, &authority, None).unwrap();
    let options = PrepareOptions {
        devices_per_account: 2,
        sample_pairs: 1,
        concurrency: 2,
    };
    let prepared = push_load::prepare::prepare(&target, &accounts, &state_path, &options)
        .await
        .unwrap();
    assert!(prepared.complete(), "{:?}", prepared.failures);
    let run_options = RunOptions {
        connections: 4,
        ramp_per_second: 4,
        duration,
        message_interval: Duration::from_secs(2),
        target_build: "local whole-stack test".to_owned(),
        reconnect: false,
    };
    let report = push_load::run::run(&target, &state_path, &run_options)
        .await
        .unwrap();
    let _ = std::fs::remove_dir_all(dir);
    report
}

/// The gateway's variables other than its datastore's, for a start that fails before it
/// uses any of them.
pub fn gateway_variables_for_a_failing_start() -> Vec<(String, String)> {
    owned(&[
        ("PUSH_MESSAGE_SVC_URL", "http://127.0.0.1:1"),
        ("PUSH_MESSAGING_TOKEN", MESSAGING_TOKEN),
        ("PUSH_AUTH_URL", "http://127.0.0.1:1"),
        ("PUSH_TOKEN_AUDIENCE", "ws://127.0.0.1:1/v1/socket"),
        ("PUSH_LISTEN_ADDR", "127.0.0.1:0"),
        ("PUSH_HEALTH_ADDR", "127.0.0.1:0"),
    ])
}

/// Routes each connection by its request line: the WebSocket to the gateway, everything
/// else to auth-svc (one HTTP client connection only ever carries auth-svc requests).
async fn route(listener: TcpListener, auth: SocketAddr, gateway: SocketAddr) {
    while let Ok((mut client, _)) = listener.accept().await {
        tokio::spawn(async move {
            let mut line = Vec::new();
            let mut byte = [0u8; 1];
            while !line.ends_with(b"\r\n") && line.len() < 8192 {
                if client.read_exact(&mut byte).await.is_err() {
                    return;
                }
                line.push(byte[0]);
            }
            let upstream = if line.starts_with(b"GET /v1/socket ") {
                gateway
            } else {
                auth
            };
            let Ok(mut upstream) = TcpStream::connect(upstream).await else {
                return;
            };
            if upstream.write_all(&line).await.is_ok() {
                let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            }
        });
    }
}

/// Polls `/readyz` of a server task until it answers 200; fails if the task stops first.
async fn wait_until_ready<T: std::fmt::Debug>(
    name: &str,
    health: SocketAddr,
    server: &JoinHandle<T>,
) {
    for _ in 0..240 {
        assert!(
            !server.is_finished(),
            "{name} stopped before it became ready"
        );
        let probe = tokio::time::timeout(Duration::from_secs(2), readyz_status(health));
        if matches!(probe.await, Ok(200)) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("{name} did not become ready within 60 s");
}

async fn readyz_status(addr: SocketAddr) -> u16 {
    let Ok(mut stream) = TcpStream::connect(addr).await else {
        return 0;
    };
    let request = "GET /readyz HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n";
    if stream.write_all(request.as_bytes()).await.is_err() {
        return 0;
    }
    let mut response = String::new();
    if stream.read_to_string(&mut response).await.is_err() {
        return 0;
    }
    response
        .get(9..12)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0)
}
