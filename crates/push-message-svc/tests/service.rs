//! The whole service on real Postgres and Redis (Docker must be running): readiness, the
//! worker's account events (a revocation kicks the device on Redis), a send that rings the
//! recipient's Redis channel, sync, ack, activity fan-out, the client's requests, auth-svc's
//! Directory and Templates (with the built-ins seeded at startup), and graceful shutdown.

use std::net::SocketAddr;
use std::time::Duration;

use prost::Message;
use push_message_svc::{Config, Listeners, serve};
use push_proto::internal::v1::accounts_client::AccountsClient;
use push_proto::internal::v1::directory_client::DirectoryClient;
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::internal::v1::templates_client::TemplatesClient;
use push_proto::internal::v1::{
    AccountDevice, AckRequest, ActivityRequest, CatalogRequest, EditRequest, IdentifyRequest, Kick,
    KickReason, Notification, ReadUpToRequest, RequestCall, SendRequest, SyncRequest,
    VisibleUsersRequest, notification::Kind,
};
use push_proto::v1::{
    self, Ack, ContactAccept, ContactRequest, ContentType, CreateGroup, CreateInvite, Edit,
    FetchHistory, GroupReceipts, ListConversations, OpenDirect, ReadUpTo, RedeemInvite, Send,
    conversation_summary, edit, request, response,
};
use push_proto::{id_bytes, kick_channel, notify_channel};
use push_store::StoreOptions;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::redis::Redis;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use tonic::Request;
use tonic::metadata::MetadataValue;
use tonic::service::Interceptor;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;
use uuid::Uuid;

/// The worker's token for the `Accounts` API.
const ACCOUNTS_TOKEN: &str = "accounts-token-for-the-service-tests";
/// auth-svc's token for the `Directory` API.
const DIRECTORY_TOKEN: &str = "directory-token-for-the-service-tests";
/// The gateway's token for the `Messaging` API.
const MESSAGING_TOKEN: &str = "messaging-token-for-the-service-tests";

struct Running {
    _postgres: ContainerAsync<Postgres>,
    _redis: ContainerAsync<Redis>,
    redis_url: String,
    grpc: SocketAddr,
    accounts: SocketAddr,
    directory: SocketAddr,
    health: SocketAddr,
    shutdown: CancellationToken,
    server: JoinHandle<anyhow::Result<()>>,
}

/// Starts Postgres, Redis and the service on free local ports.
async fn start() -> Running {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
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
    let postgres_port = postgres.get_host_port_ipv4(5432).await.unwrap();
    let redis_port = redis.get_host_port_ipv4(6379).await.unwrap();
    let redis_url = format!("redis://127.0.0.1:{redis_port}");

    let grpc_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let accounts_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let directory_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let health_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let accounts = accounts_listener.local_addr().unwrap();
    let directory = directory_listener.local_addr().unwrap();
    let (grpc, health) = (
        grpc_listener.local_addr().unwrap(),
        health_listener.local_addr().unwrap(),
    );
    let config = Config {
        database: push_store::DatabaseTarget::Url(format!(
            "postgres://postgres:postgres@127.0.0.1:{postgres_port}/postgres"
        )),
        redis: push_credentials::RedisTarget::Password {
            url: redis_url.clone(),
        },
        azure: None,
        run_migrations: true,
        grpc_addr: grpc,
        accounts_grpc_addr: accounts,
        directory_grpc_addr: directory,
        health_addr: health,
        store: StoreOptions::default(),
        accounts_token: Some(ACCOUNTS_TOKEN.to_owned()),
        directory_token: Some(DIRECTORY_TOKEN.to_owned()),
        messaging_token: Some(MESSAGING_TOKEN.to_owned()),
    };
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(serve(
        config,
        Listeners {
            messaging: grpc_listener,
            accounts: accounts_listener,
            directory: directory_listener,
            health: health_listener,
        },
        shutdown.clone(),
    ));
    let running = Running {
        _postgres: postgres,
        _redis: redis,
        redis_url,
        grpc,
        accounts,
        directory,
        health,
        shutdown,
        server,
    };
    for _ in 0..240 {
        if running.server.is_finished() {
            let outcome = running.server.await;
            panic!("the service stopped before it became ready: {outcome:?}");
        }
        let probe = tokio::time::timeout(Duration::from_secs(2), get(running.health, "/readyz"));
        if matches!(probe.await, Ok((200, _))) {
            return running;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("the service did not become ready within 60 s");
}

#[tokio::test]
async fn the_messaging_listener_does_not_serve_accounts() {
    let running = start().await;
    let mut accounts = accounts_client(format!("http://{}", running.grpc)).await;
    let result = accounts.device_registered(AccountDevice::default()).await;
    running.shutdown.cancel();
    running.server.await.unwrap().unwrap();
    assert_eq!(result.unwrap_err().code(), tonic::Code::Unimplemented);
}

#[tokio::test]
async fn each_listener_serves_only_its_own_api() {
    let running = start().await;
    let mut statuses = Vec::new();
    for (own_api, addr) in [
        ("Messaging", running.grpc),
        ("Accounts", running.accounts),
        ("Directory", running.directory),
    ] {
        let url = format!("http://{addr}");
        if own_api != "Accounts" {
            let status = accounts_client(url.clone())
                .await
                .device_registered(AccountDevice::default())
                .await
                .unwrap_err();
            statuses.push((own_api, "Accounts", status.code()));
        }
        if own_api != "Messaging" {
            let status = messaging_client(url.clone())
                .await
                .identify(IdentifyRequest::default())
                .await
                .unwrap_err();
            statuses.push((own_api, "Messaging", status.code()));
        }
        if own_api != "Directory" {
            let mut request = Request::new(CatalogRequest::default());
            request.metadata_mut().insert(
                "authorization",
                format!("Bearer {DIRECTORY_TOKEN}").parse().unwrap(),
            );
            let status = TemplatesClient::connect(url.clone())
                .await
                .unwrap()
                .catalog(request)
                .await
                .unwrap_err();
            statuses.push((own_api, "Templates", status.code()));
        }
        if own_api != "Directory" {
            let mut request = Request::new(VisibleUsersRequest::default());
            request.metadata_mut().insert(
                "authorization",
                format!("Bearer {DIRECTORY_TOKEN}").parse().unwrap(),
            );
            let status = DirectoryClient::connect(url)
                .await
                .unwrap()
                .visible_users(request)
                .await
                .unwrap_err();
            statuses.push((own_api, "Directory", status.code()));
        }
    }
    running.shutdown.cancel();
    running.server.await.unwrap().unwrap();
    for (listener, api, code) in statuses {
        assert_eq!(
            code,
            tonic::Code::Unimplemented,
            "{listener} listener must not serve {api}, even with its correct token"
        );
    }
    for addr in [
        running.grpc,
        running.accounts,
        running.directory,
        running.health,
    ] {
        assert!(
            TcpStream::connect(addr).await.is_err(),
            "{addr} is still serving"
        );
    }
}

#[tokio::test]
async fn shutdown_is_honoured_while_the_database_is_unreachable() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let grpc = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let accounts = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let directory = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let health = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let health_addr = health.local_addr().unwrap();
    let config = Config {
        database: push_store::DatabaseTarget::Url(
            "postgres://postgres:postgres@127.0.0.1:1/postgres".to_owned(),
        ),
        redis: push_credentials::RedisTarget::Password {
            url: "redis://127.0.0.1:1".to_owned(),
        },
        azure: None,
        run_migrations: true,
        grpc_addr: grpc.local_addr().unwrap(),
        accounts_grpc_addr: accounts.local_addr().unwrap(),
        directory_grpc_addr: directory.local_addr().unwrap(),
        health_addr: health.local_addr().unwrap(),
        store: StoreOptions::default(),
        accounts_token: None,
        directory_token: None,
        messaging_token: None,
    };
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(serve(
        config,
        Listeners {
            messaging: grpc,
            accounts,
            directory,
            health,
        },
        shutdown.clone(),
    ));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(get(health_addr, "/readyz").await.0, 503);

    let started = std::time::Instant::now();
    shutdown.cancel();
    let result = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("serve did not stop")
        .unwrap();
    assert!(result.is_ok(), "{result:?}");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "shutdown took {:?}",
        started.elapsed()
    );
}

/// A minimal HTTP/1.1 GET: the status code and the body.
async fn get(addr: SocketAddr, path: &str) -> (u16, String) {
    let Ok(mut stream) = TcpStream::connect(addr).await else {
        return (0, String::new());
    };
    let request = format!("GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    let status = response[9..12].parse().unwrap();
    let body = response.split("\r\n\r\n").nth(1).unwrap_or("").to_owned();
    (status, body)
}

#[tokio::test]
async fn a_message_goes_from_send_to_ack_and_rings_the_recipient() {
    let running = start().await;
    assert_eq!(
        get(running.health, "/healthz").await,
        (200, "ok".to_owned())
    );
    assert_eq!(
        get(running.health, "/readyz").await,
        (200, "ready".to_owned())
    );

    let url = format!("http://{}", running.grpc);
    let mut accounts = accounts_client(format!("http://{}", running.accounts)).await;
    let mut messaging = messaging_client(url).await;
    let (alice, bob) = (Uuid::now_v7(), Uuid::now_v7());
    let (alice_device, bob_device) = (Uuid::now_v7(), Uuid::now_v7());
    for (user, device) in [(alice, alice_device), (bob, bob_device)] {
        accounts
            .device_registered(AccountDevice {
                user_id: id_bytes(user),
                device_id: id_bytes(device),
            })
            .await
            .unwrap();
    }
    let ask = request::Body::ContactRequest(ContactRequest {
        user_id: id_bytes(bob),
    });
    messaging.request(call(alice, ask)).await.unwrap();
    let accept = request::Body::ContactAccept(ContactAccept {
        user_id: id_bytes(alice),
    });
    messaging.request(call(bob, accept)).await.unwrap();
    let open = request::Body::OpenDirect(OpenDirect {
        peer_user_id: id_bytes(bob),
    });
    let opened = messaging.request(call(alice, open)).await.unwrap();
    let Some(response::Body::DirectOpened(opened)) = opened.into_inner().body else {
        panic!("not an opened conversation");
    };
    let conversation = opened.conversation_id;

    let mut bell = redis::Client::open(running.redis_url.as_str())
        .unwrap()
        .get_async_pubsub()
        .await
        .unwrap();
    bell.subscribe(notify_channel(bob)).await.unwrap();

    let ack = messaging
        .send(SendRequest {
            user_id: id_bytes(alice),
            device_id: id_bytes(alice_device),
            send: Some(Send {
                client_msg_id: id_bytes(Uuid::now_v7()),
                conversation_id: conversation.clone(),
                content_type: ContentType::Text.into(),
                body: b"hello".to_vec(),
                client_ts: None,
            }),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ack.seq, 1);

    let rung = tokio::time::timeout(Duration::from_secs(5), bell.on_message().next())
        .await
        .expect("Bob's channel rings within 5 s")
        .unwrap();
    let notification = Notification::decode(rung.get_payload_bytes()).unwrap();
    assert!(matches!(notification.kind, Some(Kind::EntriesAvailable(_))));

    let batch = messaging
        .sync(SyncRequest {
            device_id: id_bytes(bob_device),
            limit: 0,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(batch.entries.len(), 1);
    assert_eq!(batch.entries[0].body.as_deref(), Some(b"hello".as_slice()));
    let acked = messaging
        .ack(AckRequest {
            device_id: id_bytes(bob_device),
            ack: Some(Ack {
                conversation_id: conversation,
                seq: 1,
            }),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(acked.moved);

    running.shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(10), running.server)
        .await
        .expect("the service stops within 10 s")
        .unwrap()
        .expect("a clean stop");
}

#[tokio::test]
async fn accounts_are_served_only_with_the_workers_token() {
    let running = start().await;
    let url = format!("http://{}", running.grpc);
    let channel = Channel::from_shared(format!("http://{}", running.accounts))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let messaging_channel = Channel::from_shared(url.clone())
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut messaging = messaging_client(url).await;
    let (user, device) = (Uuid::now_v7(), Uuid::now_v7());
    let event = AccountDevice {
        user_id: id_bytes(user),
        device_id: id_bytes(device),
    };
    // No password reset in this test: any issue time passes.
    let identify = IdentifyRequest {
        user_id: id_bytes(user),
        device_id: id_bytes(device),
        issued_at: 0,
    };
    // Not relayed yet: worth retrying, unlike a revoked device.
    let status = messaging.identify(identify.clone()).await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::Unavailable);

    // Without the worker's token (the gateway has none) the API refuses.
    let mut stranger = AccountsClient::new(channel.clone());
    let status = stranger.device_registered(event.clone()).await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::Unauthenticated);
    // And Messaging, which trusts the ids it is given, refuses anyone but the gateway.
    let status = MessagingClient::new(messaging_channel)
        .identify(identify.clone())
        .await
        .unwrap_err();
    assert_eq!(status.code(), tonic::Code::Unauthenticated);

    let bearer: MetadataValue<_> = format!("Bearer {ACCOUNTS_TOKEN}").parse().unwrap();
    let mut accounts = AccountsClient::with_interceptor(channel, move |mut request: Request<()>| {
        request
            .metadata_mut()
            .insert("authorization", bearer.clone());
        Ok(request)
    });
    accounts.device_registered(event.clone()).await.unwrap();
    messaging.identify(identify.clone()).await.unwrap();

    // A revocation kicks the device through its Redis channel.
    let mut bell = redis::Client::open(running.redis_url.as_str())
        .unwrap()
        .get_async_pubsub()
        .await
        .unwrap();
    bell.subscribe(kick_channel(device)).await.unwrap();
    accounts.device_revoked(event).await.unwrap();
    let kicked = tokio::time::timeout(Duration::from_secs(5), bell.on_message().next())
        .await
        .expect("the device's channel rings within 5 s")
        .unwrap();
    let kick = Kick::decode(kicked.get_payload_bytes()).unwrap();
    assert_eq!(kick.reason(), KickReason::DeviceRevoked);
    let status = messaging.identify(identify).await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::PermissionDenied);
    running.shutdown.cancel();
    running.server.await.unwrap().unwrap();
}

/// The Accounts client the worker uses, with the internal token.
async fn accounts_client(
    url: String,
) -> AccountsClient<InterceptedService<Channel, impl Interceptor>> {
    let channel = Channel::from_shared(url).unwrap().connect().await.unwrap();
    let bearer: MetadataValue<_> = format!("Bearer {ACCOUNTS_TOKEN}").parse().unwrap();
    AccountsClient::with_interceptor(channel, move |mut request: Request<()>| {
        request
            .metadata_mut()
            .insert("authorization", bearer.clone());
        Ok(request)
    })
}

/// The Messaging client the gateway uses, with its internal token.
async fn messaging_client(
    url: String,
) -> MessagingClient<InterceptedService<Channel, impl Interceptor>> {
    let channel = Channel::from_shared(url).unwrap().connect().await.unwrap();
    let bearer: MetadataValue<_> = format!("Bearer {MESSAGING_TOKEN}").parse().unwrap();
    MessagingClient::with_interceptor(channel, move |mut request: Request<()>| {
        request
            .metadata_mut()
            .insert("authorization", bearer.clone());
        Ok(request)
    })
}

/// A request frame as `user`.
fn call(user: Uuid, body: request::Body) -> RequestCall {
    RequestCall {
        user_id: id_bytes(user),
        request: Some(v1::Request {
            request_id: 1,
            body: Some(body),
        }),
    }
}

#[tokio::test]
async fn the_client_requests_and_the_directory_run_on_postgres() {
    let running = start().await;
    let url = format!("http://{}", running.grpc);
    let mut accounts = accounts_client(format!("http://{}", running.accounts)).await;
    let mut messaging = messaging_client(url).await;
    let (alice, bob, carol) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let devices = [Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7()];
    for (user, device) in [alice, bob, carol].into_iter().zip(devices) {
        accounts
            .device_registered(AccountDevice {
                user_id: id_bytes(user),
                device_id: id_bytes(device),
            })
            .await
            .unwrap();
    }

    // Alice invites Bob; Carol asks Alice, who accepts.
    let create = request::Body::CreateInvite(CreateInvite {
        expires_in_s: 600,
        max_uses: 1,
    });
    let Some(response::Body::Invite(invite)) = messaging
        .request(call(alice, create))
        .await
        .unwrap()
        .into_inner()
        .body
    else {
        panic!("not an invite");
    };
    let redeem = request::Body::RedeemInvite(RedeemInvite { code: invite.code });
    messaging.request(call(bob, redeem)).await.unwrap();
    let ask = request::Body::ContactRequest(ContactRequest {
        user_id: id_bytes(alice),
    });
    messaging.request(call(carol, ask)).await.unwrap();
    let accept = request::Body::ContactAccept(ContactAccept {
        user_id: id_bytes(carol),
    });
    messaging.request(call(alice, accept)).await.unwrap();

    let group = request::Body::CreateGroup(CreateGroup {
        title: "Trip".to_owned(),
        member_ids: vec![id_bytes(bob), id_bytes(carol)],
    });
    let Some(response::Body::GroupCreated(created)) = messaging
        .request(call(alice, group))
        .await
        .unwrap()
        .into_inner()
        .body
    else {
        panic!("not a created group");
    };
    let sent = messaging
        .send(SendRequest {
            user_id: id_bytes(alice),
            device_id: id_bytes(devices[0]),
            send: Some(Send {
                client_msg_id: id_bytes(Uuid::now_v7()),
                conversation_id: created.conversation_id.clone(),
                content_type: ContentType::Text.into(),
                body: b"helo".to_vec(),
                client_ts: None,
            }),
        })
        .await
        .unwrap()
        .into_inner();
    messaging
        .edit(EditRequest {
            user_id: id_bytes(alice),
            device_id: id_bytes(devices[0]),
            edit: Some(Edit {
                client_msg_id: id_bytes(Uuid::now_v7()),
                conversation_id: created.conversation_id.clone(),
                target: Some(edit::Target::TargetMessageId(sent.message_id)),
                body: b"hello".to_vec(),
                client_ts: None,
            }),
        })
        .await
        .unwrap();
    let read = messaging
        .read_up_to(ReadUpToRequest {
            user_id: id_bytes(bob),
            read: Some(ReadUpTo {
                conversation_id: created.conversation_id.clone(),
                seq: sent.seq,
            }),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(read.moved);

    let fetch = request::Body::FetchHistory(FetchHistory {
        conversation_id: created.conversation_id.clone(),
        before_seq: None,
        limit: 0,
    });
    let Some(response::Body::History(page)) = messaging
        .request(call(carol, fetch))
        .await
        .unwrap()
        .into_inner()
        .body
    else {
        panic!("not a history page");
    };
    let bodies: Vec<Option<&[u8]>> = page
        .entries
        .iter()
        .map(|entry| entry.body.as_deref())
        .collect();
    assert_eq!(bodies.len(), 2, "the created entry and the message");
    assert_eq!(bodies[1], Some(b"hello".as_slice()));

    let list = request::Body::ListConversations(ListConversations::default());
    let Some(response::Body::Conversations(listed)) = messaging
        .request(call(alice, list))
        .await
        .unwrap()
        .into_inner()
        .body
    else {
        panic!("not a conversation list");
    };
    assert_eq!(listed.conversations.len(), 1);
    assert_eq!(listed.conversations[0].title.as_deref(), Some("Trip"));
    assert_eq!(
        listed.conversations[0].receipts,
        Some(conversation_summary::Receipts::Group(GroupReceipts {
            all_delivered_seq: 1,
            all_read_seq: 1,
        })),
        "Bob read the message, Carol did not: only the created entry counts as read by all"
    );

    // auth-svc asks whose profiles Carol may see: her contact Alice and Bob, from the
    // group, but not a stranger.
    let channel = Channel::from_shared(format!("http://{}", running.directory))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let ask = VisibleUsersRequest {
        user_id: id_bytes(carol),
        user_ids: vec![id_bytes(Uuid::now_v7()), id_bytes(bob), id_bytes(alice)],
    };
    let status = DirectoryClient::new(channel.clone())
        .visible_users(ask.clone())
        .await
        .unwrap_err();
    assert_eq!(status.code(), tonic::Code::Unauthenticated);
    let bearer: MetadataValue<_> = format!("Bearer {DIRECTORY_TOKEN}").parse().unwrap();
    let mut directory =
        DirectoryClient::with_interceptor(channel, move |mut request: Request<()>| {
            request
                .metadata_mut()
                .insert("authorization", bearer.clone());
            Ok(request)
        });
    let visible = directory.visible_users(ask).await.unwrap().into_inner();
    let mut expected = vec![id_bytes(alice), id_bytes(bob)];
    expected.sort();
    assert_eq!(visible.user_ids, expected);

    running.shutdown.cancel();
    running.server.await.unwrap().unwrap();
}

#[tokio::test]
async fn templates_are_seeded_at_startup_and_served_only_with_auth_svcs_token() {
    let running = start().await;
    let channel = Channel::from_shared(format!("http://{}", running.directory))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let status = TemplatesClient::new(channel.clone())
        .catalog(CatalogRequest::default())
        .await
        .unwrap_err();
    assert_eq!(status.code(), tonic::Code::Unauthenticated);
    let bearer: MetadataValue<_> = format!("Bearer {MESSAGING_TOKEN}").parse().unwrap();
    let status =
        TemplatesClient::with_interceptor(channel.clone(), move |mut request: Request<()>| {
            request
                .metadata_mut()
                .insert("authorization", bearer.clone());
            Ok(request)
        })
        .catalog(CatalogRequest::default())
        .await
        .unwrap_err();
    assert_eq!(status.code(), tonic::Code::Unauthenticated);

    let bearer: MetadataValue<_> = format!("Bearer {DIRECTORY_TOKEN}").parse().unwrap();
    let mut templates =
        TemplatesClient::with_interceptor(channel, move |mut request: Request<()>| {
            request
                .metadata_mut()
                .insert("authorization", bearer.clone());
            Ok(request)
        });
    let catalog = templates
        .catalog(CatalogRequest::default())
        .await
        .unwrap()
        .into_inner();
    let mut refs: Vec<&str> = catalog
        .entries
        .iter()
        .map(|entry| entry.template_ref.as_str())
        .collect();
    refs.sort();
    assert_eq!(
        refs,
        [
            "agent.approval@1",
            "agent.choose@1",
            "agent.plan-approval@1",
            "agent.plan@1",
            "agent.question@1",
            "agent.reply@1",
            "agent.reply@2"
        ]
    );
    assert_eq!(catalog.seq, 7);
    assert!(
        catalog
            .entries
            .iter()
            .all(|entry| entry.definition.is_some())
    );

    running.shutdown.cancel();
    running.server.await.unwrap().unwrap();
}

/// Alice and Bob, relayed with a device each, as contacts with a direct conversation.
async fn direct_chat(running: &Running) -> ((Uuid, Uuid), (Uuid, Uuid), Vec<u8>) {
    let mut accounts = accounts_client(format!("http://{}", running.accounts)).await;
    let mut messaging = messaging_client(format!("http://{}", running.grpc)).await;
    let (alice, bob) = (
        (Uuid::now_v7(), Uuid::now_v7()),
        (Uuid::now_v7(), Uuid::now_v7()),
    );
    for (user, device) in [alice, bob] {
        accounts
            .device_registered(AccountDevice {
                user_id: id_bytes(user),
                device_id: id_bytes(device),
            })
            .await
            .unwrap();
    }
    let ask = request::Body::ContactRequest(ContactRequest {
        user_id: id_bytes(bob.0),
    });
    messaging.request(call(alice.0, ask)).await.unwrap();
    let accept = request::Body::ContactAccept(ContactAccept {
        user_id: id_bytes(alice.0),
    });
    messaging.request(call(bob.0, accept)).await.unwrap();
    let open = request::Body::OpenDirect(OpenDirect {
        peer_user_id: id_bytes(bob.0),
    });
    let opened = messaging.request(call(alice.0, open)).await.unwrap();
    let Some(response::Body::DirectOpened(opened)) = opened.into_inner().body else {
        panic!("not an opened conversation");
    };
    (alice, bob, opened.conversation_id)
}

fn activity(
    (user, device): (Uuid, Uuid),
    conversation: &[u8],
    state: v1::ActivityState,
    label: Option<&str>,
) -> ActivityRequest {
    ActivityRequest {
        user_id: id_bytes(user),
        device_id: id_bytes(device),
        activity: Some(v1::Activity {
            conversation_id: conversation.to_vec(),
            state: state.into(),
            label: label.map(str::to_owned),
        }),
    }
}

#[tokio::test]
async fn activity_rpc_publishes_to_members() {
    let running = start().await;
    let (alice, bob, conversation) = direct_chat(&running).await;
    let mut messaging = messaging_client(format!("http://{}", running.grpc)).await;
    let mut bell = redis::Client::open(running.redis_url.as_str())
        .unwrap()
        .get_async_pubsub()
        .await
        .unwrap();
    bell.subscribe(notify_channel(bob.0)).await.unwrap();
    bell.subscribe(notify_channel(alice.0)).await.unwrap();

    let working = activity(
        alice,
        &conversation,
        v1::ActivityState::Working,
        Some("tests"),
    );
    messaging.activity(working).await.unwrap();

    let rung = tokio::time::timeout(Duration::from_secs(5), bell.on_message().next())
        .await
        .expect("Bob's channel rings within 5 s")
        .unwrap();
    assert_eq!(rung.get_channel_name(), notify_channel(bob.0));
    let notification = Notification::decode(rung.get_payload_bytes()).unwrap();
    assert_eq!(
        notification.kind,
        Some(Kind::ActivityChanged(v1::ActivityEvent {
            conversation_id: conversation.clone(),
            user_id: id_bytes(alice.0),
            state: v1::ActivityState::Working.into(),
            label: Some("tests".to_owned()),
            ttl_ms: 30_000,
        }))
    );
    let quiet = tokio::time::timeout(Duration::from_millis(300), bell.on_message().next()).await;
    assert!(quiet.is_err(), "the sender does not hear its own activity");

    let unspecified = activity(alice, &conversation, v1::ActivityState::Unspecified, None);
    let status = messaging.activity(unspecified).await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    let mut missing = activity(alice, &conversation, v1::ActivityState::Typing, None);
    missing.activity = None;
    let status = messaging.activity(missing).await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    running.shutdown.cancel();
    running.server.await.unwrap().unwrap();
}

#[tokio::test]
async fn activity_rpc_refuses_a_non_member() {
    let running = start().await;
    let (_alice, _bob, conversation) = direct_chat(&running).await;
    let mut accounts = accounts_client(format!("http://{}", running.accounts)).await;
    let mut messaging = messaging_client(format!("http://{}", running.grpc)).await;
    let carol = (Uuid::now_v7(), Uuid::now_v7());
    accounts
        .device_registered(AccountDevice {
            user_id: id_bytes(carol.0),
            device_id: id_bytes(carol.1),
        })
        .await
        .unwrap();

    let typing = activity(carol, &conversation, v1::ActivityState::Typing, None);
    let status = messaging.activity(typing).await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    assert_eq!(
        push_proto::error_from_status(&status).code(),
        v1::ErrorCode::NotMember
    );
    let elsewhere = activity(
        carol,
        &id_bytes(Uuid::now_v7()),
        v1::ActivityState::Typing,
        None,
    );
    let status = messaging.activity(elsewhere).await.unwrap_err();
    assert_eq!(
        push_proto::error_from_status(&status).code(),
        v1::ErrorCode::ConversationNotFound
    );

    running.shutdown.cancel();
    running.server.await.unwrap().unwrap();
}
