//! The gRPC services: `Messaging` for the gateway, `Accounts` for the worker, `Directory`
//! for auth-svc and the DEV ONLY `DevSetup`.

use std::sync::Arc;

use push_domain::{
    ClientMsgId, Clock, ContactState, ConversationId, DeviceId, MAX_SYNC_BATCH, Messaging,
    Notifier, SendText, Store, UserId,
};
use push_proto::id_bytes;
use push_proto::internal::v1::{
    AccountDevice, AccountUser, AccountsResponse, AckRequest, AckResponse, DeleteRequest,
    EditRequest, IdentifyRequest, IdentifyResponse, MakeContactsRequest, MakeContactsResponse,
    OpenDirectRequest, OpenDirectResponse, ReadUpToRequest, ReadUpToResponse,
    RegisterDeviceRequest, RegisterDeviceResponse, RegisterUserRequest, RegisterUserResponse,
    RequestCall, SendRequest, SyncRequest, SyncResponse, VisibleUsersRequest, VisibleUsersResponse,
    accounts_server, dev_setup_server, directory_server, messaging_server,
};
use push_proto::v1::{ContentType, SendAck};
use tonic::service::Interceptor;
use tonic::{Request, Response, Status};

use crate::convert::{deliver, domain_error, from_micros, invalid, parse_id, send_ack};
use crate::requests;

/// The `Messaging` service: what the gateway calls for a connected device. The gateway
/// passes the user and device it verified.
pub struct MessagingApi<S, C, N> {
    messaging: Arc<Messaging<S, C, N>>,
}

impl<S, C, N> MessagingApi<S, C, N> {
    pub fn new(messaging: Arc<Messaging<S, C, N>>) -> Self {
        Self { messaging }
    }
}

#[tonic::async_trait]
impl<S, C, N> messaging_server::Messaging for MessagingApi<S, C, N>
where
    S: Store + 'static,
    C: Clock + 'static,
    N: Notifier + 'static,
{
    async fn identify(
        &self,
        request: Request<IdentifyRequest>,
    ) -> Result<Response<IdentifyResponse>, Status> {
        let request = request.into_inner();
        let user = parse_id(&request.user_id, "user_id", UserId)?;
        let device = parse_id(&request.device_id, "device_id", DeviceId)?;
        self.messaging
            .identify(device, user)
            .await
            .map_err(domain_error)?;
        Ok(Response::new(IdentifyResponse {}))
    }

    async fn send(&self, request: Request<SendRequest>) -> Result<Response<SendAck>, Status> {
        let request = request.into_inner();
        let sender = parse_id(&request.user_id, "user_id", UserId)?;
        let device = parse_id(&request.device_id, "device_id", DeviceId)?;
        let send = request.send.ok_or_else(|| invalid("send is required"))?;
        if send.content_type() != ContentType::Text {
            return Err(invalid("only text messages can be sent"));
        }
        let command = SendText {
            sender,
            device,
            conversation: parse_id(&send.conversation_id, "conversation_id", ConversationId)?,
            client_msg_id: parse_id(&send.client_msg_id, "client_msg_id", ClientMsgId)?,
            client_ts: send.client_ts.map(from_micros).transpose()?,
            body: send.body,
        };
        let receipt = self
            .messaging
            .send_text(command)
            .await
            .map_err(domain_error)?;
        Ok(Response::new(send_ack(&receipt)))
    }

    async fn sync(&self, request: Request<SyncRequest>) -> Result<Response<SyncResponse>, Status> {
        let request = request.into_inner();
        let device = parse_id(&request.device_id, "device_id", DeviceId)?;
        let limit = match request.limit {
            0 => MAX_SYNC_BATCH,
            limit => limit,
        };
        let batch = self
            .messaging
            .sync(device, limit)
            .await
            .map_err(domain_error)?;
        Ok(Response::new(SyncResponse {
            entries: batch.entries.into_iter().map(deliver).collect(),
            more: batch.more,
        }))
    }

    async fn ack(&self, request: Request<AckRequest>) -> Result<Response<AckResponse>, Status> {
        let request = request.into_inner();
        let device = parse_id(&request.device_id, "device_id", DeviceId)?;
        let ack = request.ack.ok_or_else(|| invalid("ack is required"))?;
        let conversation = parse_id(&ack.conversation_id, "conversation_id", ConversationId)?;
        let moved = self
            .messaging
            .ack(device, conversation, ack.seq)
            .await
            .map_err(domain_error)?;
        Ok(Response::new(AckResponse { moved }))
    }

    async fn edit(&self, request: Request<EditRequest>) -> Result<Response<SendAck>, Status> {
        requests::edit(&self.messaging, request.into_inner())
            .await
            .map(Response::new)
    }

    async fn delete(&self, request: Request<DeleteRequest>) -> Result<Response<SendAck>, Status> {
        requests::delete(&self.messaging, request.into_inner())
            .await
            .map(Response::new)
    }

    async fn read_up_to(
        &self,
        request: Request<ReadUpToRequest>,
    ) -> Result<Response<ReadUpToResponse>, Status> {
        requests::read_up_to(&self.messaging, request.into_inner())
            .await
            .map(Response::new)
    }

    async fn request(
        &self,
        request: Request<RequestCall>,
    ) -> Result<Response<push_proto::v1::Response>, Status> {
        requests::request(&self.messaging, request.into_inner())
            .await
            .map(Response::new)
    }
}

/// The `Accounts` service: auth-svc's user and device events, relayed by the worker. Every
/// call is idempotent and any order of calls ends in the same state (see
/// [`Messaging::device_registered`] and [`Messaging::device_revoked`]).
pub struct AccountsApi<S, C, N> {
    messaging: Arc<Messaging<S, C, N>>,
}

impl<S, C, N> AccountsApi<S, C, N> {
    pub fn new(messaging: Arc<Messaging<S, C, N>>) -> Self {
        Self { messaging }
    }
}

#[tonic::async_trait]
impl<S, C, N> accounts_server::Accounts for AccountsApi<S, C, N>
where
    S: Store + 'static,
    C: Clock + 'static,
    N: Notifier + 'static,
{
    async fn user_verified(
        &self,
        request: Request<AccountUser>,
    ) -> Result<Response<AccountsResponse>, Status> {
        let user = parse_id(&request.into_inner().user_id, "user_id", UserId)?;
        self.messaging
            .register_user(user)
            .await
            .map_err(domain_error)?;
        Ok(Response::new(AccountsResponse {}))
    }

    async fn device_registered(
        &self,
        request: Request<AccountDevice>,
    ) -> Result<Response<AccountsResponse>, Status> {
        let request = request.into_inner();
        let user = parse_id(&request.user_id, "user_id", UserId)?;
        let device = parse_id(&request.device_id, "device_id", DeviceId)?;
        self.messaging
            .device_registered(device, user)
            .await
            .map_err(domain_error)?;
        Ok(Response::new(AccountsResponse {}))
    }

    async fn device_revoked(
        &self,
        request: Request<AccountDevice>,
    ) -> Result<Response<AccountsResponse>, Status> {
        let request = request.into_inner();
        let user = parse_id(&request.user_id, "user_id", UserId)?;
        let device = parse_id(&request.device_id, "device_id", DeviceId)?;
        self.messaging
            .device_revoked(device, user)
            .await
            .map_err(domain_error)?;
        Ok(Response::new(AccountsResponse {}))
    }

    async fn password_reset(
        &self,
        request: Request<AccountUser>,
    ) -> Result<Response<AccountsResponse>, Status> {
        let user = parse_id(&request.into_inner().user_id, "user_id", UserId)?;
        self.messaging
            .password_reset(user)
            .await
            .map_err(domain_error)?;
        Ok(Response::new(AccountsResponse {}))
    }
}

/// The `Directory` service: whose profiles a user may see, for auth-svc's
/// `GET /v1/users?ids=…` (see [`Messaging::visible_users`]).
pub struct DirectoryApi<S, C, N> {
    messaging: Arc<Messaging<S, C, N>>,
}

impl<S, C, N> DirectoryApi<S, C, N> {
    pub fn new(messaging: Arc<Messaging<S, C, N>>) -> Self {
        Self { messaging }
    }
}

/// Most user ids one `VisibleUsers` call may ask about.
pub const MAX_VISIBLE_USERS: usize = 100;

#[tonic::async_trait]
impl<S, C, N> directory_server::Directory for DirectoryApi<S, C, N>
where
    S: Store + 'static,
    C: Clock + 'static,
    N: Notifier + 'static,
{
    async fn visible_users(
        &self,
        request: Request<VisibleUsersRequest>,
    ) -> Result<Response<VisibleUsersResponse>, Status> {
        let request = request.into_inner();
        let user = parse_id(&request.user_id, "user_id", UserId)?;
        if request.user_ids.len() > MAX_VISIBLE_USERS {
            return Err(invalid(format!(
                "user_ids: at most {MAX_VISIBLE_USERS} ids"
            )));
        }
        let candidates = request
            .user_ids
            .iter()
            .map(|id| parse_id(id, "user_ids", UserId))
            .collect::<Result<Vec<_>, _>>()?;
        let visible = self
            .messaging
            .visible_users(user, &candidates)
            .await
            .map_err(domain_error)?;
        Ok(Response::new(VisibleUsersResponse {
            user_ids: visible.iter().map(|user| id_bytes(user.0)).collect(),
        }))
    }
}

/// Guards an internal service: a call passes only with its shared internal token in its
/// metadata, `authorization: Bearer <token>`, compared in constant time. Each service
/// has a token of its own, given only to its caller: the worker's for `Accounts`,
/// auth-svc's for `Directory`. The gateway has neither, so it cannot register devices or
/// read relationships even though it can reach message-svc. Without a configured token
/// every call is refused.
#[derive(Clone)]
pub struct InternalToken {
    expected: Option<Arc<str>>,
    /// Why every call is refused without a configured token.
    off: &'static str,
    /// Why a call without the right token is refused.
    refused: &'static str,
}

impl InternalToken {
    /// The worker's token for `Accounts` (`PUSH_ACCOUNTS_TOKEN`).
    pub fn accounts(expected: Option<String>) -> Self {
        Self {
            expected: expected.map(Arc::from),
            off: "the Accounts API is off: PUSH_ACCOUNTS_TOKEN is not set",
            refused: "the Accounts API needs the internal token",
        }
    }

    /// auth-svc's token for `Directory` (`PUSH_DIRECTORY_TOKEN`).
    pub fn directory(expected: Option<String>) -> Self {
        Self {
            expected: expected.map(Arc::from),
            off: "the Directory API is off: PUSH_DIRECTORY_TOKEN is not set",
            refused: "the Directory API needs the internal token",
        }
    }
}

impl std::fmt::Debug for InternalToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InternalToken")
            .field("expected", &self.expected.as_ref().map(|_| "<redacted>"))
            .field("off", &self.off)
            .finish()
    }
}

impl Interceptor for InternalToken {
    fn call(&mut self, request: Request<()>) -> Result<Request<()>, Status> {
        let Some(expected) = &self.expected else {
            return Err(Status::unauthenticated(self.off));
        };
        let presented = request
            .metadata()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        match presented {
            Some(token) if same(token.as_bytes(), expected.as_bytes()) => Ok(request),
            _ => Err(Status::unauthenticated(self.refused)),
        }
    }
}

/// Compares in time that depends only on the lengths, never on where the bytes differ.
fn same(presented: &[u8], expected: &[u8]) -> bool {
    presented.len() == expected.len()
        && presented
            .iter()
            .zip(expected)
            .fold(0u8, |difference, (a, b)| difference | (a ^ b))
            == 0
}

/// DEV ONLY: sets up users, devices, contacts and direct conversations without auth-svc.
/// `serve` registers it only when the configuration enables it (`PUSH_DEV_SETUP=1`).
pub struct DevSetupApi<S, C, N> {
    messaging: Arc<Messaging<S, C, N>>,
}

impl<S, C, N> DevSetupApi<S, C, N> {
    pub fn new(messaging: Arc<Messaging<S, C, N>>) -> Self {
        Self { messaging }
    }
}

#[tonic::async_trait]
impl<S, C, N> dev_setup_server::DevSetup for DevSetupApi<S, C, N>
where
    S: Store + 'static,
    C: Clock + 'static,
    N: Notifier + 'static,
{
    async fn register_user(
        &self,
        request: Request<RegisterUserRequest>,
    ) -> Result<Response<RegisterUserResponse>, Status> {
        let user = parse_id(&request.into_inner().user_id, "user_id", UserId)?;
        self.messaging
            .register_user(user)
            .await
            .map_err(domain_error)?;
        Ok(Response::new(RegisterUserResponse {}))
    }

    async fn register_device(
        &self,
        request: Request<RegisterDeviceRequest>,
    ) -> Result<Response<RegisterDeviceResponse>, Status> {
        let request = request.into_inner();
        let user = parse_id(&request.user_id, "user_id", UserId)?;
        let device = parse_id(&request.device_id, "device_id", DeviceId)?;
        self.messaging
            .register_device(device, user)
            .await
            .map_err(domain_error)?;
        Ok(Response::new(RegisterDeviceResponse {}))
    }

    async fn make_contacts(
        &self,
        request: Request<MakeContactsRequest>,
    ) -> Result<Response<MakeContactsResponse>, Status> {
        let request = request.into_inner();
        let user = parse_id(&request.user_id, "user_id", UserId)?;
        let peer = parse_id(&request.peer_id, "peer_id", UserId)?;
        let state = self
            .messaging
            .contact_request(user, peer)
            .await
            .map_err(domain_error)?;
        if state != ContactState::Accepted {
            self.messaging
                .contact_accept(peer, user)
                .await
                .map_err(domain_error)?;
        }
        Ok(Response::new(MakeContactsResponse {}))
    }

    async fn open_direct(
        &self,
        request: Request<OpenDirectRequest>,
    ) -> Result<Response<OpenDirectResponse>, Status> {
        let request = request.into_inner();
        let user = parse_id(&request.user_id, "user_id", UserId)?;
        let peer = parse_id(&request.peer_id, "peer_id", UserId)?;
        let conversation = self
            .messaging
            .open_direct(user, peer)
            .await
            .map_err(domain_error)?;
        Ok(Response::new(OpenDirectResponse {
            conversation_id: id_bytes(conversation.0),
        }))
    }
}

#[cfg(test)]
mod tests {
    use push_domain::{Event, InMemoryStore, KickReason, RecordingNotifier, SystemClock};
    use push_proto::internal::v1::accounts_server::Accounts as AccountsRpc;
    use push_proto::internal::v1::dev_setup_server::DevSetup;
    use push_proto::internal::v1::directory_server::Directory as DirectoryRpc;
    use push_proto::internal::v1::messaging_server::Messaging as MessagingRpc;
    use push_proto::v1::{Ack, ErrorCode, Send};
    use push_proto::{error_from_status, id};
    use tonic::Code;

    use super::*;

    type Api = MessagingApi<InMemoryStore, SystemClock, Arc<RecordingNotifier>>;
    type Accounts = AccountsApi<InMemoryStore, SystemClock, Arc<RecordingNotifier>>;
    type Setup = DevSetupApi<InMemoryStore, SystemClock, Arc<RecordingNotifier>>;
    type Directory = DirectoryApi<InMemoryStore, SystemClock, Arc<RecordingNotifier>>;

    struct World {
        api: Api,
        accounts: Accounts,
        setup: Setup,
        directory: Directory,
        notifier: Arc<RecordingNotifier>,
    }

    fn world() -> World {
        let notifier = Arc::new(RecordingNotifier::default());
        let messaging = Arc::new(Messaging::new(
            InMemoryStore::default(),
            SystemClock,
            Arc::clone(&notifier),
        ));
        World {
            api: MessagingApi::new(Arc::clone(&messaging)),
            accounts: AccountsApi::new(Arc::clone(&messaging)),
            setup: DevSetupApi::new(Arc::clone(&messaging)),
            directory: DirectoryApi::new(messaging),
            notifier,
        }
    }

    /// A user with one device.
    struct Person {
        user: UserId,
        device: DeviceId,
    }

    async fn person(world: &World) -> Person {
        let person = Person {
            user: UserId::new(),
            device: DeviceId::new(),
        };
        world
            .setup
            .register_user(Request::new(RegisterUserRequest {
                user_id: id_bytes(person.user.0),
            }))
            .await
            .unwrap();
        world
            .setup
            .register_device(Request::new(RegisterDeviceRequest {
                user_id: id_bytes(person.user.0),
                device_id: id_bytes(person.device.0),
            }))
            .await
            .unwrap();
        person
    }

    /// Alice and Bob as contacts with a direct conversation.
    async fn chat(world: &World) -> (Person, Person, ConversationId) {
        let (alice, bob) = (person(world).await, person(world).await);
        world
            .setup
            .make_contacts(Request::new(MakeContactsRequest {
                user_id: id_bytes(alice.user.0),
                peer_id: id_bytes(bob.user.0),
            }))
            .await
            .unwrap();
        let opened = world
            .setup
            .open_direct(Request::new(OpenDirectRequest {
                user_id: id_bytes(alice.user.0),
                peer_id: id_bytes(bob.user.0),
            }))
            .await
            .unwrap()
            .into_inner();
        let conversation = ConversationId(id(&opened.conversation_id).unwrap());
        (alice, bob, conversation)
    }

    fn send_request(from: &Person, conversation: ConversationId, body: &str) -> SendRequest {
        SendRequest {
            user_id: id_bytes(from.user.0),
            device_id: id_bytes(from.device.0),
            send: Some(Send {
                client_msg_id: id_bytes(ClientMsgId::new().0),
                conversation_id: id_bytes(conversation.0),
                content_type: ContentType::Text.into(),
                body: body.as_bytes().to_vec(),
                client_ts: None,
            }),
        }
    }

    async fn sync(world: &World, device: DeviceId, limit: u32) -> SyncResponse {
        world
            .api
            .sync(Request::new(SyncRequest {
                device_id: id_bytes(device.0),
                limit,
            }))
            .await
            .unwrap()
            .into_inner()
    }

    #[tokio::test]
    async fn a_sent_message_is_synced_and_acked() {
        let world = world();
        let (alice, bob, conversation) = chat(&world).await;
        world.notifier.take();

        let request = send_request(&alice, conversation, "hello");
        let client_msg_id = request.send.as_ref().unwrap().client_msg_id.clone();
        let sent = world
            .api
            .send(Request::new(request))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(sent.client_msg_id, client_msg_id);
        assert_eq!(sent.seq, 1);
        let mut rung = Vec::new();
        for event in world.notifier.take() {
            if let Event::EntriesAvailable { users } = event {
                rung.extend(users);
            }
        }
        rung.sort();
        let mut members = vec![alice.user, bob.user];
        members.sort();
        assert_eq!(rung, members);

        let batch = sync(&world, bob.device, 0).await;
        assert_eq!(batch.entries.len(), 1);
        assert!(!batch.more);
        let entry = &batch.entries[0];
        assert_eq!(entry.seq, 1);
        assert_eq!(entry.message_id, sent.message_id);
        assert_eq!(entry.sender_id, id_bytes(alice.user.0));
        assert_eq!(entry.body.as_deref(), Some(b"hello".as_slice()));

        let acked = world
            .api
            .ack(Request::new(AckRequest {
                device_id: id_bytes(bob.device.0),
                ack: Some(Ack {
                    conversation_id: id_bytes(conversation.0),
                    seq: 1,
                }),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(acked.moved);
        assert!(sync(&world, bob.device, 0).await.entries.is_empty());
    }

    #[tokio::test]
    async fn a_sync_limit_of_zero_means_the_largest_batch() {
        let world = world();
        let (alice, bob, conversation) = chat(&world).await;
        for body in ["one", "two"] {
            let request = send_request(&alice, conversation, body);
            world.api.send(Request::new(request)).await.unwrap();
        }
        assert_eq!(sync(&world, bob.device, 0).await.entries.len(), 2);
        let first = sync(&world, bob.device, 1).await;
        assert_eq!(first.entries.len(), 1);
        assert!(first.more);
    }

    #[tokio::test]
    async fn identify_refuses_a_device_of_another_user() {
        let world = world();
        let (alice, bob) = (person(&world).await, person(&world).await);
        let own = world
            .api
            .identify(Request::new(IdentifyRequest {
                user_id: id_bytes(alice.user.0),
                device_id: id_bytes(alice.device.0),
            }))
            .await;
        assert!(own.is_ok());
        let status = world
            .api
            .identify(Request::new(IdentifyRequest {
                user_id: id_bytes(alice.user.0),
                device_id: id_bytes(bob.device.0),
            }))
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::PermissionDenied);
        assert_eq!(error_from_status(&status).code(), ErrorCode::UnknownDevice);
    }

    #[tokio::test]
    async fn a_send_to_an_unknown_conversation_is_not_found() {
        let world = world();
        let alice = person(&world).await;
        let request = send_request(&alice, ConversationId::new(), "hello");
        let status = world.api.send(Request::new(request)).await.unwrap_err();
        assert_eq!(status.code(), Code::NotFound);
        assert_eq!(
            error_from_status(&status).code(),
            ErrorCode::ConversationNotFound
        );
    }

    #[tokio::test]
    async fn malformed_sends_are_invalid() {
        let world = world();
        let (alice, _bob, conversation) = chat(&world).await;

        let mut short_id = send_request(&alice, conversation, "hello");
        short_id.send.as_mut().unwrap().conversation_id = vec![1, 2, 3];
        let not_text = SendRequest {
            send: Some(Send {
                content_type: ContentType::SystemEvent.into(),
                ..send_request(&alice, conversation, "hello").send.unwrap()
            }),
            ..send_request(&alice, conversation, "hello")
        };
        let empty = send_request(&alice, conversation, "");
        let missing = SendRequest {
            send: None,
            ..send_request(&alice, conversation, "hello")
        };
        for request in [short_id, not_text, empty, missing] {
            let status = world.api.send(Request::new(request)).await.unwrap_err();
            assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
            assert_eq!(error_from_status(&status).code(), ErrorCode::Invalid);
        }
    }

    /// The error a call answered with, which must be `INVALID_ARGUMENT` naming `field`.
    fn assert_invalid(status: Status, field: &str) {
        assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
        let error = error_from_status(&status);
        assert_eq!(error.code(), ErrorCode::Invalid);
        assert!(!error.retryable);
        assert!(error.message.starts_with(field), "{}", error.message);
    }

    #[tokio::test]
    async fn malformed_ids_on_identify_sync_and_ack_are_invalid() {
        let world = world();
        let (_alice, bob, conversation) = chat(&world).await;
        let short = vec![1, 2, 3];

        let identify = |user_id: Vec<u8>, device_id: Vec<u8>| {
            Request::new(IdentifyRequest { user_id, device_id })
        };
        let status = world
            .api
            .identify(identify(short.clone(), id_bytes(bob.device.0)))
            .await
            .unwrap_err();
        assert_invalid(status, "user_id");
        let status = world
            .api
            .identify(identify(id_bytes(bob.user.0), Vec::new()))
            .await
            .unwrap_err();
        assert_invalid(status, "device_id");

        let status = world
            .api
            .sync(Request::new(SyncRequest {
                device_id: short.clone(),
                limit: 0,
            }))
            .await
            .unwrap_err();
        assert_invalid(status, "device_id");

        let ack = |device_id: Vec<u8>, conversation_id: Vec<u8>| {
            Request::new(AckRequest {
                device_id,
                ack: Some(Ack {
                    conversation_id,
                    seq: 0,
                }),
            })
        };
        let status = world
            .api
            .ack(ack(short.clone(), id_bytes(conversation.0)))
            .await
            .unwrap_err();
        assert_invalid(status, "device_id");
        let status = world
            .api
            .ack(ack(id_bytes(bob.device.0), short))
            .await
            .unwrap_err();
        assert_invalid(status, "conversation_id");
        let status = world
            .api
            .ack(Request::new(AckRequest {
                device_id: id_bytes(bob.device.0),
                ack: None,
            }))
            .await
            .unwrap_err();
        assert_invalid(status, "ack");
    }

    #[tokio::test]
    async fn a_sync_without_an_ack_returns_the_same_entries() {
        let world = world();
        let (alice, bob, conversation) = chat(&world).await;
        for body in ["one", "two"] {
            let request = send_request(&alice, conversation, body);
            world.api.send(Request::new(request)).await.unwrap();
        }
        let first = sync(&world, bob.device, 0).await;
        assert_eq!(first.entries.len(), 2);
        assert_eq!(sync(&world, bob.device, 0).await, first);
    }

    fn account_device(user: UserId, device: DeviceId) -> Request<AccountDevice> {
        Request::new(AccountDevice {
            user_id: id_bytes(user.0),
            device_id: id_bytes(device.0),
        })
    }

    async fn identify(world: &World, user: UserId, device: DeviceId) -> Result<(), Code> {
        world
            .api
            .identify(Request::new(IdentifyRequest {
                user_id: id_bytes(user.0),
                device_id: id_bytes(device.0),
            }))
            .await
            .map(|_| ())
            .map_err(|status| status.code())
    }

    #[tokio::test]
    async fn relayed_account_events_register_and_revoke_devices_in_any_order() {
        let world = world();
        let (user, phone, tablet) = (UserId::new(), DeviceId::new(), DeviceId::new());
        let verified = AccountUser {
            user_id: id_bytes(user.0),
        };
        world
            .accounts
            .user_verified(Request::new(verified.clone()))
            .await
            .unwrap();
        world
            .accounts
            .user_verified(Request::new(verified))
            .await
            .unwrap();
        for _ in 0..2 {
            world
                .accounts
                .device_registered(account_device(user, phone))
                .await
                .unwrap();
        }
        assert_eq!(identify(&world, user, phone).await, Ok(()));
        // A device not relayed yet is worth retrying, unlike a revoked one.
        assert_eq!(identify(&world, user, tablet).await, Err(Code::Unavailable));

        // The revocation of the tablet overtakes its registration.
        world
            .accounts
            .device_revoked(account_device(user, tablet))
            .await
            .unwrap();
        world
            .accounts
            .device_registered(account_device(user, tablet))
            .await
            .unwrap();
        assert_eq!(
            identify(&world, user, tablet).await,
            Err(Code::PermissionDenied)
        );
        world.notifier.take();
        world
            .accounts
            .password_reset(Request::new(AccountUser {
                user_id: id_bytes(user.0),
            }))
            .await
            .unwrap();
        // The phone is kicked, to sign in again; the revoked tablet is not active.
        assert_eq!(
            world.notifier.take(),
            [Event::Kick {
                devices: vec![phone],
                reason: KickReason::PasswordReset,
            }]
        );
        assert_eq!(identify(&world, user, phone).await, Ok(()));
    }

    #[tokio::test]
    async fn relayed_events_that_can_never_succeed_are_refused_for_good() {
        let world = world();
        let alice = person(&world).await;
        let status = world
            .accounts
            .device_registered(account_device(UserId::new(), alice.device))
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::PermissionDenied);
        assert!(!error_from_status(&status).retryable);

        let malformed = AccountDevice {
            user_id: vec![1, 2, 3],
            device_id: id_bytes(alice.device.0),
        };
        let status = world
            .accounts
            .device_revoked(Request::new(malformed))
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::InvalidArgument);
        assert_eq!(identify(&world, alice.user, alice.device).await, Ok(()));
    }

    fn with_authorization(value: &str) -> Request<()> {
        let mut request = Request::new(());
        request
            .metadata_mut()
            .insert("authorization", value.parse().unwrap());
        request
    }

    #[test]
    fn accounts_calls_pass_only_with_the_internal_token() {
        let token = "a".repeat(32);
        let mut guard = InternalToken::accounts(Some(token.clone()));
        assert!(
            guard
                .call(with_authorization(&format!("Bearer {token}")))
                .is_ok()
        );
        for refused in [
            Request::new(()),
            with_authorization(&token),
            with_authorization(&format!("Bearer {}", "b".repeat(32))),
            with_authorization(&format!("Basic {token}")),
            with_authorization("Bearer "),
        ] {
            let status = guard.call(refused).unwrap_err();
            assert_eq!(status.code(), Code::Unauthenticated);
            assert!(!status.message().contains(&token));
        }
        let mut off = InternalToken::accounts(None);
        let status = off
            .call(with_authorization(&format!("Bearer {token}")))
            .unwrap_err();
        assert_eq!(status.code(), Code::Unauthenticated);
        assert!(!format!("{guard:?}").contains(&token));
    }

    #[tokio::test]
    async fn the_directory_answers_who_shares_a_contact_or_a_conversation() {
        let world = world();
        let (alice, bob, _chat) = chat(&world).await;
        let stranger = person(&world).await;
        let ask = |user: &Person, user_ids: Vec<Vec<u8>>| {
            Request::new(VisibleUsersRequest {
                user_id: id_bytes(user.user.0),
                user_ids,
            })
        };
        let candidates = vec![
            id_bytes(stranger.user.0),
            id_bytes(bob.user.0),
            id_bytes(alice.user.0),
        ];
        let visible = world
            .directory
            .visible_users(ask(&alice, candidates))
            .await
            .unwrap()
            .into_inner()
            .user_ids;
        let mut expected = vec![id_bytes(alice.user.0), id_bytes(bob.user.0)];
        expected.sort();
        assert_eq!(visible, expected);

        let status = world
            .directory
            .visible_users(ask(&alice, vec![vec![1, 2, 3]]))
            .await
            .unwrap_err();
        assert_invalid(status, "user_ids");
        // Malformed ids would fail the parse; the limit must be checked first.
        let too_many = vec![vec![1, 2, 3]; MAX_VISIBLE_USERS + 1];
        let status = world
            .directory
            .visible_users(ask(&alice, too_many))
            .await
            .unwrap_err();
        assert_invalid(status.clone(), "user_ids");
        assert!(
            error_from_status(&status).message.contains("at most 100"),
            "{status:?}"
        );
    }

    #[test]
    fn each_internal_api_has_a_token_of_its_own() {
        let (directory_token, accounts_token) = ("d".repeat(32), "a".repeat(32));
        let mut directory = InternalToken::directory(Some(directory_token.clone()));
        let bearer = |token: &str| with_authorization(&format!("Bearer {token}"));
        assert!(directory.call(bearer(&directory_token)).is_ok());
        let status = directory.call(bearer(&accounts_token)).unwrap_err();
        assert_eq!(status.code(), Code::Unauthenticated);
        assert_eq!(
            status.message(),
            "the Directory API needs the internal token"
        );
        let mut accounts = InternalToken::accounts(Some(accounts_token));
        assert!(accounts.call(bearer(&directory_token)).is_err());
        let status = InternalToken::directory(None)
            .call(bearer(&directory_token))
            .unwrap_err();
        assert_eq!(
            status.message(),
            "the Directory API is off: PUSH_DIRECTORY_TOKEN is not set"
        );
        assert!(!format!("{directory:?}").contains(&directory_token));
    }
}
