//! The client's frames over the gRPC API, on the in-memory store. Users, devices and
//! contacts are set up straight through the domain.

use std::sync::Arc;

use chrono::{TimeDelta, TimeZone, Utc};
use push_domain::{
    ClientMsgId, ConversationId, DeviceId, Event, InMemoryStore, ManualClock, Messaging,
    ReceiptUpdate, RecordingNotifier, UserId,
};
use push_proto::internal::v1::messaging_server::Messaging as MessagingRpc;
use push_proto::internal::v1::{
    DeleteRequest, EditRequest, ReadUpToRequest, RequestCall, SendRequest,
};
use push_proto::v1::{
    self, ContentType, ConversationKind, Delete, Deliver, DirectReceipts, Edit, EntryKind,
    ErrorCode, FetchHistory, ListConversations, ReadUpTo, SendAck, conversation_summary, delete,
    edit, request, response,
};
use push_proto::{error_from_status, id_bytes};
use tonic::{Code, Request, Status};

use crate::grpc::MessagingApi;

type Svc = Messaging<InMemoryStore, Arc<ManualClock>, Arc<RecordingNotifier>>;

struct World {
    api: MessagingApi<InMemoryStore, Arc<ManualClock>, Arc<RecordingNotifier>>,
    svc: Arc<Svc>,
    clock: Arc<ManualClock>,
    notifier: Arc<RecordingNotifier>,
}

fn world() -> World {
    let clock = Arc::new(ManualClock::new(
        Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap(),
    ));
    let notifier = Arc::new(RecordingNotifier::default());
    let svc = Arc::new(Messaging::new(
        InMemoryStore::default(),
        Arc::clone(&clock),
        Arc::clone(&notifier),
    ));
    World {
        api: MessagingApi::new(Arc::clone(&svc)),
        svc,
        clock,
        notifier,
    }
}

/// A user with one device.
#[derive(Clone, Copy)]
struct Person {
    user: UserId,
    device: DeviceId,
}

impl World {
    async fn person(&self) -> Person {
        let person = Person {
            user: UserId::new(),
            device: DeviceId::new(),
        };
        self.svc
            .device_registered(person.device, person.user)
            .await
            .unwrap();
        person
    }

    async fn befriend(&self, a: &Person, b: &Person) {
        self.svc.contact_request(a.user, b.user).await.unwrap();
        self.svc.contact_accept(b.user, a.user).await.unwrap();
    }

    /// Two contacts with their direct conversation.
    async fn chat(&self) -> (Person, Person, ConversationId) {
        let (alice, bob) = (self.person().await, self.person().await);
        self.befriend(&alice, &bob).await;
        let chat = self.svc.open_direct(alice.user, bob.user).await.unwrap();
        (alice, bob, chat)
    }

    async fn send(&self, from: &Person, conversation: ConversationId, body: &str) -> SendAck {
        let request = SendRequest {
            user_id: id_bytes(from.user.0),
            device_id: id_bytes(from.device.0),
            send: Some(v1::Send {
                client_msg_id: id_bytes(ClientMsgId::new().0),
                conversation_id: id_bytes(conversation.0),
                content_type: ContentType::Text.into(),
                body: body.as_bytes().to_vec(),
                client_ts: None,
            }),
        };
        self.api
            .send(Request::new(request))
            .await
            .unwrap()
            .into_inner()
    }

    async fn edit(
        &self,
        from: &Person,
        conversation: ConversationId,
        target: edit::Target,
        body: &str,
    ) -> Result<SendAck, Status> {
        let request = EditRequest {
            user_id: id_bytes(from.user.0),
            device_id: id_bytes(from.device.0),
            edit: Some(Edit {
                client_msg_id: id_bytes(ClientMsgId::new().0),
                conversation_id: id_bytes(conversation.0),
                target: Some(target),
                body: body.as_bytes().to_vec(),
                client_ts: None,
            }),
        };
        self.api
            .edit(Request::new(request))
            .await
            .map(|response| response.into_inner())
    }

    async fn delete(
        &self,
        from: &Person,
        conversation: ConversationId,
        target: Option<delete::Target>,
    ) -> Result<SendAck, Status> {
        let request = DeleteRequest {
            user_id: id_bytes(from.user.0),
            device_id: id_bytes(from.device.0),
            delete: Some(Delete {
                client_msg_id: id_bytes(ClientMsgId::new().0),
                conversation_id: id_bytes(conversation.0),
                target,
            }),
        };
        self.api
            .delete(Request::new(request))
            .await
            .map(|response| response.into_inner())
    }

    async fn read(
        &self,
        reader: &Person,
        conversation: ConversationId,
        seq: i64,
    ) -> Result<bool, Status> {
        let request = ReadUpToRequest {
            user_id: id_bytes(reader.user.0),
            read: Some(ReadUpTo {
                conversation_id: id_bytes(conversation.0),
                seq,
            }),
        };
        self.api
            .read_up_to(Request::new(request))
            .await
            .map(|response| response.into_inner().moved)
    }

    /// Sends a request frame as `from`, numbered 42.
    async fn request(&self, from: &Person, body: request::Body) -> Result<response::Body, Status> {
        let call = RequestCall {
            user_id: id_bytes(from.user.0),
            request: Some(v1::Request {
                request_id: 42,
                body: Some(body),
            }),
        };
        let response = self.api.request(Request::new(call)).await?.into_inner();
        assert_eq!(response.request_id, 42);
        Ok(response.body.unwrap())
    }

    async fn sync(&self, device: DeviceId) -> Vec<Deliver> {
        self.svc
            .sync(device, 100)
            .await
            .unwrap()
            .entries
            .into_iter()
            .map(crate::convert::deliver)
            .collect()
    }
}

/// The client error of a failed call: its code, and whether it carries request 42.
fn failure(status: &Status) -> (ErrorCode, bool) {
    let error = error_from_status(status);
    (error.code(), error.request_id == Some(42))
}

fn message_id(ack: &SendAck) -> edit::Target {
    edit::Target::TargetMessageId(ack.message_id.clone())
}

fn list_conversations(changed_since: Option<i64>) -> request::Body {
    request::Body::ListConversations(ListConversations { changed_since })
}

#[tokio::test]
async fn an_edit_and_a_delete_are_stored_as_entries_and_delivered() {
    let world = world();
    let (alice, bob, chat) = world.chat().await;
    let sent = world.send(&alice, chat, "helo").await;

    let edited = world
        .edit(&alice, chat, message_id(&sent), "hello")
        .await
        .unwrap();
    assert_eq!(edited.seq, 2);
    let entries = world.sync(bob.device).await;
    assert_eq!(entries[0].body.as_deref(), Some(b"hello".as_slice()));
    assert_eq!(entries[0].edit_seq, Some(2));
    assert_eq!(entries[1].kind(), EntryKind::Edit);
    assert_eq!(entries[1].target_seq, Some(1));

    // The sender may name the message by the client_msg_id it was sent with.
    let target = delete::Target::TargetClientMsgId(sent.client_msg_id.clone());
    let deleted = world.delete(&alice, chat, Some(target)).await.unwrap();
    assert_eq!(deleted.seq, 3);
    let entries = world.sync(bob.device).await;
    assert!(entries[0].deleted);
    assert_eq!(entries[0].body, None);
    assert_eq!(entries[1].body, None, "the edit's text is erased too");
    assert_eq!(entries[2].kind(), EntryKind::Delete);
}

#[tokio::test]
async fn edits_and_deletes_are_refused_with_their_own_errors() {
    let world = world();
    let (alice, bob, chat) = world.chat().await;
    let sent = world.send(&alice, chat, "mine").await;

    let by_bob = world.edit(&bob, chat, message_id(&sent), "theirs").await;
    assert_eq!(by_bob.unwrap_err().code(), Code::PermissionDenied);
    let unknown = edit::Target::TargetMessageId(id_bytes(ClientMsgId::new().0));
    let status = world.edit(&alice, chat, unknown, "x").await.unwrap_err();
    assert_eq!(failure(&status).0, ErrorCode::TargetNotFound);
    let status = world.delete(&alice, chat, None).await.unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(error_from_status(&status).message, "target is required");

    world
        .clock
        .advance(TimeDelta::hours(48) + TimeDelta::seconds(1));
    let status = world
        .edit(&alice, chat, message_id(&sent), "late")
        .await
        .unwrap_err();
    assert_eq!(failure(&status).0, ErrorCode::WindowExpired);
    let late = delete::Target::TargetMessageId(sent.message_id.clone());
    let status = world.delete(&alice, chat, Some(late)).await.unwrap_err();
    assert_eq!(failure(&status).0, ErrorCode::WindowExpired);
}

#[tokio::test]
async fn reading_moves_the_position_and_tells_both_sides() {
    let world = world();
    let (alice, bob, chat) = world.chat().await;
    world.send(&alice, chat, "one").await;
    world.notifier.take();

    assert!(world.read(&bob, chat, 1).await.unwrap());
    let events = world.notifier.take();
    assert!(events.contains(&Event::ReadPositionChanged {
        user: bob.user,
        conversation: chat,
        read_seq: 1,
    }));
    assert!(events.contains(&Event::Receipt {
        conversation: chat,
        to: vec![alice.user],
        receipt: ReceiptUpdate::Direct {
            user_id: bob.user,
            delivered_seq: 1,
            read_seq: 1,
        },
    }));
    assert!(!world.read(&bob, chat, 1).await.unwrap());
    let status = world.read(&bob, chat, 2).await.unwrap_err();
    assert_eq!(status.code(), Code::OutOfRange);
}

#[tokio::test]
async fn a_listing_has_positions_unread_counts_and_receipts() {
    let world = world();
    let (alice, bob, chat) = world.chat().await;
    world.send(&alice, chat, "one").await;
    world.send(&alice, chat, "two").await;
    world.read(&bob, chat, 1).await.unwrap();

    let response::Body::Conversations(listed) =
        world.request(&bob, list_conversations(None)).await.unwrap()
    else {
        panic!("not a conversation list");
    };
    assert_eq!(listed.conversations.len(), 1);
    let summary = &listed.conversations[0];
    assert_eq!(summary.conversation_id, id_bytes(chat.0));
    assert_eq!(summary.kind(), ConversationKind::Direct);
    assert_eq!(
        (summary.last_seq, summary.read_seq, summary.unread),
        (2, 1, 1)
    );
    let mut members = vec![id_bytes(alice.user.0), id_bytes(bob.user.0)];
    members.sort();
    assert_eq!(summary.members, members);

    let response::Body::Conversations(alices) = world
        .request(&alice, list_conversations(None))
        .await
        .unwrap()
    else {
        panic!("not a conversation list");
    };
    assert_eq!(
        alices.conversations[0].receipts,
        Some(conversation_summary::Receipts::Direct(DirectReceipts {
            peer_delivered_seq: 1,
            peer_read_seq: 1,
        }))
    );

    // Nothing changed after the last append.
    let later = list_conversations(Some(summary.updated_at));
    let response::Body::Conversations(changed) = world.request(&bob, later).await.unwrap() else {
        panic!("not a conversation list");
    };
    assert!(changed.conversations.is_empty());
}

#[tokio::test]
async fn history_pages_back_through_messages_in_their_current_state() {
    let world = world();
    let (alice, bob, chat) = world.chat().await;
    let first = world.send(&alice, chat, "one").await;
    world.send(&bob, chat, "two").await;
    world.send(&alice, chat, "three").await;
    world
        .edit(&alice, chat, message_id(&first), "one!")
        .await
        .unwrap();

    let fetch = |before_seq| {
        request::Body::FetchHistory(FetchHistory {
            conversation_id: id_bytes(chat.0),
            before_seq,
            limit: 2,
        })
    };
    let response::Body::History(newest) = world.request(&bob, fetch(None)).await.unwrap() else {
        panic!("not a history page");
    };
    let seqs: Vec<i64> = newest.entries.iter().map(|entry| entry.seq).collect();
    assert_eq!(seqs, [2, 3], "the edit entry is not history");
    assert!(newest.has_more);
    let response::Body::History(oldest) = world.request(&bob, fetch(Some(2))).await.unwrap() else {
        panic!("not a history page");
    };
    assert_eq!(oldest.entries.len(), 1);
    assert_eq!(oldest.entries[0].body.as_deref(), Some(b"one!".as_slice()));
    assert!(!oldest.has_more);
}

#[tokio::test]
async fn a_failed_request_carries_its_request_id() {
    let world = world();
    let alice = world.person().await;
    let unknown = request::Body::FetchHistory(FetchHistory {
        conversation_id: id_bytes(ConversationId::new().0),
        before_seq: None,
        limit: 0,
    });
    let status = world.request(&alice, unknown).await.unwrap_err();
    assert_eq!(status.code(), Code::NotFound);
    assert_eq!(failure(&status), (ErrorCode::ConversationNotFound, true));

    let malformed = request::Body::FetchHistory(FetchHistory {
        conversation_id: vec![1, 2, 3],
        before_seq: None,
        limit: 0,
    });
    let status = world.request(&alice, malformed).await.unwrap_err();
    assert_eq!(failure(&status), (ErrorCode::Invalid, true));

    let empty = RequestCall {
        user_id: id_bytes(alice.user.0),
        request: Some(v1::Request {
            request_id: 42,
            body: None,
        }),
    };
    let status = world.api.request(Request::new(empty)).await.unwrap_err();
    assert_eq!(failure(&status), (ErrorCode::Invalid, true));
}
