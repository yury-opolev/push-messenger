//! The whole system on this machine: Postgres and Redis in containers (Docker must be
//! running), auth-svc, the worker, message-svc and the gateway in this process, wired as
//! the chart wires them (each internal API behind its own token), and a router serving
//! both public APIs from one origin as an ingress does. The smoke test and push-load run
//! against them as clients see them.

use std::net::SocketAddr;
use std::time::Duration;

use push_load::config::{PrepareOptions, RunOptions, Target as LoadTarget};
use push_load::report::Status;
use push_load::state::State;
use push_smoke::{Target, run};
use push_store::StoreOptions;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::redis::Redis;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

type Server = JoinHandle<Result<(), String>>;

/// Spawns a server; its result as text, so every server's handle has one type.
fn spawn<E: std::fmt::Debug + Send + 'static>(
    server: impl std::future::Future<Output = Result<(), E>> + Send + 'static,
) -> Server {
    tokio::spawn(async move { server.await.map_err(|error| format!("{error:?}")) })
}

struct Stack {
    auth_url: String,
    socket_url: String,
    worker_url: String,
    /// auth-svc and the gateway behind one origin, routed by path.
    public_url: String,
    signing_key: &'static str,
    shutdown: CancellationToken,
    servers: Vec<(&'static str, Server)>,
    _postgres: ContainerAsync<Postgres>,
    _redis: ContainerAsync<Redis>,
}

impl Stack {
    async fn start() -> Self {
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
        let database_url = format!(
            "postgres://postgres:postgres@127.0.0.1:{}/postgres",
            postgres.get_host_port_ipv4(5432).await.unwrap()
        );
        let redis_url = format!(
            "redis://127.0.0.1:{}",
            redis.get_host_port_ipv4(6379).await.unwrap()
        );

        let shutdown = CancellationToken::new();

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
        let (server_url, _) = database_url.rsplit_once('/').unwrap();
        let auth_database_url = format!("{server_url}/push_auth");
        let worker_server = format!(
            "postgres://push_worker:worker-secret@127.0.0.1:{}",
            postgres.get_host_port_ipv4(5432).await.unwrap()
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socket_url = format!("ws://{}/v1/socket", listener.local_addr().unwrap());
        let auth_http = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let auth_health = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let auth_health_addr = auth_health.local_addr().unwrap();
        let auth_url = format!("http://{}", auth_http.local_addr().unwrap());
        let auth_http_addr = auth_http.local_addr().unwrap();
        let gateway_addr = listener.local_addr().unwrap();
        let grpc = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let accounts = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let directory = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let message_svc_url = format!("http://{}", grpc.local_addr().unwrap());
        let accounts_url = format!("http://{}", accounts.local_addr().unwrap());
        let directory_url = format!("http://{}", directory.local_addr().unwrap());
        // auth-svc's token for message-svc's Directory API, as the chart's Secret holds it.
        // message-svc refuses it equal to the Accounts token.
        let directory_token = "directory-token-for-the-local-smoke-test";
        // The gateway's token for message-svc's Messaging API.
        let messaging_token = "messaging-token-for-the-local-smoke-test";
        // base64 of 32 bytes of 0x07: auth-svc's signing key, which the smoke test also gets
        // to sign tokens the gateway must refuse.
        let signing_key = "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=";
        let auth_variables = [
            ("PUSH_DATABASE_URL", auth_database_url.as_str()),
            ("PUSH_REDIS_URL", redis_url.as_str()),
            ("PUSH_AUTH_PUBLIC_URL", "http://push.local"),
            ("PUSH_AUTH_TOKEN_AUDIENCE", socket_url.as_str()),
            ("PUSH_AUTH_SIGNING_KEY", signing_key),
            // base64 of 32 bytes of 0x09.
            (
                "PUSH_AUTH_LINK_KEY",
                "CQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQk=",
            ),
            ("PUSH_AUTH_WORKER_ROLE", "push_worker"),
            ("PUSH_MESSAGE_SVC_URL", directory_url.as_str()),
            ("PUSH_DIRECTORY_TOKEN", directory_token),
        ];
        let auth_config = push_auth::Config::from_lookup(|name| {
            auth_variables
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        })
        .unwrap();
        let auth = spawn(push_auth::serve(
            auth_config,
            auth_http,
            auth_health,
            shutdown.clone(),
        ));

        let message_svc_health = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let message_svc_health_addr = message_svc_health.local_addr().unwrap();
        // The worker's token for message-svc's Accounts API, as the chart's Secret holds it.
        let accounts_token = "accounts-token-for-the-local-smoke-test".to_owned();
        let message_svc = spawn(push_message_svc::serve(
            push_message_svc::Config {
                database: push_store::DatabaseTarget::Url(database_url),
                redis: push_credentials::RedisTarget::Password {
                    url: redis_url.clone(),
                },
                azure: None,
                run_migrations: true,
                grpc_addr: grpc.local_addr().unwrap(),
                accounts_grpc_addr: accounts.local_addr().unwrap(),
                directory_grpc_addr: directory.local_addr().unwrap(),
                health_addr: message_svc_health.local_addr().unwrap(),
                store: StoreOptions::default(),
                accounts_token: Some(accounts_token.clone()),
                directory_token: Some(directory_token.to_owned()),
                messaging_token: Some(messaging_token.to_owned()),
            },
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
        let gateway = spawn(push_gateway::serve(
            push_gateway::Config {
                message_svc_url: message_svc_url.clone(),
                messaging_token: messaging_token.to_owned(),
                redis_url: redis_url.clone(),
                auth_url: auth_url.clone(),
                token_audience: socket_url.clone(),
                listen_addr: listener.local_addr().unwrap(),
                health_addr: gateway_health.local_addr().unwrap(),
                heartbeat: Duration::from_secs(30),
                groups: false,
                client_ip_header: None,
            },
            listener,
            gateway_health,
            shutdown.clone(),
        ));

        let worker_http = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let worker_health = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let worker_health_addr = worker_health.local_addr().unwrap();
        let worker_url = format!("http://{}", worker_http.local_addr().unwrap());
        let worker = spawn(push_worker::serve(
            push_worker::Config {
                database: push_store::DatabaseTarget::Url(format!("{worker_server}/push_worker")),
                auth_database: push_store::DatabaseTarget::Url(format!(
                    "{worker_server}/push_auth"
                )),
                azure: None,
                run_migrations: true,
                message_svc_url: accounts_url,
                accounts_token,
                sender: push_worker::Sender::Log,
                from: String::new(),
                product_name: "Push Messenger".to_owned(),
                webhook_secret: None,
                poll: Duration::from_millis(250),
                unverified_after: Duration::from_secs(7 * 24 * 3600),
                retention: Duration::from_secs(30 * 24 * 3600),
                http_addr: worker_http.local_addr().unwrap(),
                health_addr: worker_health_addr,
                store: StoreOptions::default(),
                dev_mailbox: true,
            },
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
            shutdown,
            servers: vec![
                ("auth-svc", auth),
                ("the worker", worker),
                ("message-svc", message_svc),
                ("the gateway", gateway),
            ],
            _postgres: postgres,
            _redis: redis,
        }
    }

    fn smoke_target(&self, data_dir: String) -> Target {
        Target {
            mailbox_url: self.worker_url.clone(),
            auth_url: self.auth_url.clone(),
            socket_url: self.socket_url.clone(),
            host: None,
            signing_key: self.signing_key.to_owned(),
            data_dir,
        }
    }

    async fn stop(self) {
        self.shutdown.cancel();
        for (name, server) in self.servers {
            server
                .await
                .unwrap()
                .unwrap_or_else(|error| panic!("{name} did not stop cleanly: {error}"));
        }
    }
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

#[tokio::test]
async fn the_smoke_test_passes_against_the_whole_stack() {
    let stack = Stack::start().await;
    // The SDK apps' databases, fresh for every run.
    let data_dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("push-smoke-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&data_dir).unwrap();
    let target = stack.smoke_target(data_dir.to_string_lossy().into_owned());
    tokio::time::timeout(Duration::from_secs(300), run(&target))
        .await
        .expect("the smoke test finishes within 300 s")
        .expect("the smoke test passes");
    stack.stop().await;
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// push-load against the whole stack: DEV ONLY accounts; a preparation with one unusable
/// account that resumes without new devices; a passing run with sample messages and no
/// credential in its report; a refused refresh token failing a run and repaired by the
/// next preparation; and too many connections refused before any request.
#[tokio::test]
async fn push_load_prepares_runs_and_reports_against_the_whole_stack() {
    let stack = Stack::start().await;
    let mut accounts = push_smoke::create_verified_accounts(&stack.smoke_target(String::new()), 4)
        .await
        .expect("four verified accounts");
    accounts.push(push_load::accounts::Account {
        email: format!("nobody.{}@load.test", uuid::Uuid::now_v7().simple()),
        password: "not-an-account".to_owned(),
    });
    // Credential files outside the repository: push-load refuses a Git working tree.
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

    let first = push_load::prepare::prepare(&target, &accounts, &state_path, &options)
        .await
        .unwrap();
    assert_eq!(
        (first.devices_ready, first.devices_wanted, first.pairs_ready),
        (8, 10, 1)
    );
    assert_eq!(first.failures.len(), 1, "{:?}", first.failures);
    assert!(
        first.failures[0].contains("entry 5") && first.failures[0].contains("invalid_credentials")
    );
    assert!(!first.complete());
    let devices = |state: &State| -> Vec<Option<uuid::Uuid>> {
        state
            .accounts
            .iter()
            .flat_map(|account| account.devices.iter().map(|device| device.device_id))
            .collect()
    };
    let prepared = State::load(&state_path).unwrap().unwrap();
    let again = push_load::prepare::prepare(&target, &accounts, &state_path, &options)
        .await
        .unwrap();
    assert_eq!((again.devices_ready, again.pairs_ready), (8, 1));
    assert_eq!(
        devices(&State::load(&state_path).unwrap().unwrap()),
        devices(&prepared),
        "resuming adds no devices"
    );

    let run_options = RunOptions {
        connections: 8,
        ramp_per_second: 8,
        duration: Duration::from_secs(35),
        message_interval: Duration::from_secs(2),
        target_build: "local whole-stack test".to_owned(),
        reconnect: true,
    };
    let report = push_load::run::run(&target, &state_path, &run_options)
        .await
        .unwrap();
    assert_eq!(report.status, Status::Passed, "{:?}", report.failures);
    assert_eq!(report.connections.ready_once, 8);
    assert!(
        report.connections.pongs >= 8,
        "a Pong per connection after one 30 s heartbeat"
    );
    assert!(
        report.messages.delivered >= 5 && report.messages.lost == 0,
        "{:?}",
        report.messages
    );
    assert!(report.latency.receipt.count >= 1);
    let json = serde_json::to_string(&report).unwrap();
    let state = State::load(&state_path).unwrap().unwrap();
    for account in &accounts {
        assert!(!json.contains(&account.email) && !json.contains(&account.password));
    }
    for device in state.accounts.iter().flat_map(|account| &account.devices) {
        if let Some(token) = &device.refresh_token {
            assert!(
                !json.contains(token.as_str()),
                "the report holds a refresh token"
            );
        }
    }

    // A refused refresh token: counted as an authentication failure, never a pass.
    let mut broken = state.clone();
    let victim = broken.ready_devices()[4];
    broken.device_mut(victim).refresh_token = Some("not-a-refresh-token".to_owned());
    broken.save(&state_path).unwrap();
    let short = RunOptions {
        duration: Duration::from_secs(10),
        ..run_options.clone()
    };
    let failed = push_load::run::run(&target, &state_path, &short)
        .await
        .unwrap();
    assert_eq!(failed.status, Status::Failed);
    assert_eq!(
        (
            failed.connections.auth_failures,
            failed.connections.ready_once
        ),
        (1, 7)
    );
    assert!(
        State::load(&state_path)
            .unwrap()
            .unwrap()
            .device(victim)
            .needs_sign_in
    );
    let repaired = push_load::prepare::prepare(&target, &accounts, &state_path, &options)
        .await
        .unwrap();
    assert_eq!(repaired.devices_ready, 8);
    assert_eq!(
        devices(&State::load(&state_path).unwrap().unwrap()),
        devices(&prepared),
        "the same device signed in again"
    );

    let too_many = RunOptions {
        connections: 9,
        ..run_options
    };
    let refused = push_load::run::run(&target, &state_path, &too_many)
        .await
        .unwrap_err()
        .to_string();
    assert!(refused.contains("prepared devices"), "{refused}");

    stack.stop().await;
    std::fs::remove_dir_all(dir).unwrap();
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
