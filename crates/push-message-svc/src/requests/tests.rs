//! The client's frames over the gRPC API, on the in-memory store. Users, devices and
//! contacts are set up straight through the domain.

use std::sync::Arc;

use chrono::{TimeDelta, TimeZone, Utc};
use push_domain::{
    ClientMsgId, ConversationId, DeviceId, Event, InMemoryStore, ManualClock, Messaging,
    PresenceView, ReceiptUpdate, RecordingNotifier, UserId,
};
use push_proto::internal::v1::messaging_server::Messaging as MessagingRpc;
use push_proto::internal::v1::{
    DeleteRequest, EditRequest, ReadUpToRequest, RequestCall, SendRequest,
};
use push_proto::v1::{
    self, AddMembers, Contact, ContactAccept, ContactBlock, ContactRequest, ContactState,
    ContactUnblock, ContentType, ConversationKind, CreateGroup, CreateInvite, Delete, Deliver,
    DirectReceipts, Done, Edit, EntryKind, ErrorCode, FetchHistory, GetPresence, InviteRedeemed,
    LeaveGroup, ListContacts, ListConversations, OpenDirect, Presence, PresenceSharing,
    PresenceState, ReadUpTo, RedeemInvite, RemoveMember, RenameGroup, RevokeInvite, Role, SendAck,
    SetPresenceSharing, SetRole, conversation_summary, delete, edit, request, response,
};
use push_proto::{error_from_status, id_bytes};
use tonic::{Code, Request, Status};

use crate::grpc::MessagingApi;
use crate::presence::FixedPresence;

type Svc = Messaging<InMemoryStore, Arc<ManualClock>, Arc<RecordingNotifier>>;

struct World {
    api: MessagingApi<InMemoryStore, Arc<ManualClock>, Arc<RecordingNotifier>>,
    svc: Arc<Svc>,
    clock: Arc<ManualClock>,
    notifier: Arc<RecordingNotifier>,
    presence: Arc<FixedPresence>,
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
    let presence = Arc::new(FixedPresence::default());
    World {
        api: MessagingApi::with_presence(Arc::clone(&svc), presence.clone()),
        svc,
        clock,
        notifier,
        presence,
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

async fn contacts(world: &World, of: &Person) -> Vec<Contact> {
    let list = request::Body::ListContacts(ListContacts {});
    let response::Body::Contacts(listed) = world.request(of, list).await.unwrap() else {
        panic!("not a contact list");
    };
    listed.contacts
}

fn contact(person: &Person, state: ContactState) -> Contact {
    Contact {
        user_id: id_bytes(person.user.0),
        state: state.into(),
    }
}

#[tokio::test]
async fn contacts_are_requested_accepted_and_blocked_and_a_block_freezes_the_chat() {
    let world = world();
    let (alice, bob) = (world.person().await, world.person().await);
    let ask = request::Body::ContactRequest(ContactRequest {
        user_id: id_bytes(bob.user.0),
    });
    assert_eq!(
        world.request(&alice, ask).await.unwrap(),
        response::Body::Contact(contact(&bob, ContactState::RequestedOut))
    );
    assert_eq!(
        contacts(&world, &bob).await,
        [contact(&alice, ContactState::RequestedIn)]
    );

    // A direct conversation needs an accepted contact.
    let open = request::Body::OpenDirect(OpenDirect {
        peer_user_id: id_bytes(bob.user.0),
    });
    let status = world.request(&alice, open.clone()).await.unwrap_err();
    assert_eq!(failure(&status), (ErrorCode::NotContact, true));
    let accept = request::Body::ContactAccept(ContactAccept {
        user_id: id_bytes(alice.user.0),
    });
    assert_eq!(
        world.request(&bob, accept.clone()).await.unwrap(),
        response::Body::Done(Done {})
    );
    let status = world.request(&bob, accept).await.unwrap_err();
    assert_eq!(failure(&status).0, ErrorCode::NoPendingRequest);
    let response::Body::DirectOpened(opened) = world.request(&alice, open).await.unwrap() else {
        panic!("not an opened conversation");
    };
    let from_bob = request::Body::OpenDirect(OpenDirect {
        peer_user_id: id_bytes(alice.user.0),
    });
    assert_eq!(
        world.request(&bob, from_bob).await.unwrap(),
        response::Body::DirectOpened(opened.clone())
    );
    let chat = ConversationId(push_proto::id(&opened.conversation_id).unwrap());
    let sent = world.send(&alice, chat, "before the block").await;

    // After a block nobody can change the chat, which stays as evidence; nobody is told.
    let block = request::Body::ContactBlock(ContactBlock {
        user_id: id_bytes(alice.user.0),
    });
    world.request(&bob, block).await.unwrap();
    let status = world
        .edit(&alice, chat, message_id(&sent), "after the block")
        .await
        .unwrap_err();
    assert_eq!(failure(&status).0, ErrorCode::NotContact);
    let target = delete::Target::TargetMessageId(sent.message_id.clone());
    let status = world.delete(&alice, chat, Some(target)).await.unwrap_err();
    assert_eq!(failure(&status).0, ErrorCode::NotContact);
    assert!(contacts(&world, &alice).await.is_empty());
    assert_eq!(
        contacts(&world, &bob).await,
        [contact(&alice, ContactState::Blocked)]
    );
    let entries = world.sync(bob.device).await;
    assert_eq!(
        entries[0].body.as_deref(),
        Some(b"before the block".as_slice())
    );

    let unblock = request::Body::ContactUnblock(ContactUnblock {
        user_id: id_bytes(alice.user.0),
    });
    world.request(&bob, unblock).await.unwrap();
    assert!(contacts(&world, &bob).await.is_empty());

    // An id that is no user looks like someone who has not answered yet.
    let unknown = UserId::new();
    let stranger = request::Body::ContactRequest(ContactRequest {
        user_id: id_bytes(unknown.0),
    });
    assert_eq!(
        world.request(&alice, stranger).await.unwrap(),
        response::Body::Contact(Contact {
            user_id: id_bytes(unknown.0),
            state: ContactState::RequestedOut.into(),
        })
    );
    let own = request::Body::ContactRequest(ContactRequest {
        user_id: id_bytes(alice.user.0),
    });
    let status = world.request(&alice, own).await.unwrap_err();
    assert_eq!(failure(&status).0, ErrorCode::Invalid);
}

#[tokio::test]
async fn an_invite_makes_contacts_until_it_is_revoked_and_never_reveals_a_block() {
    let world = world();
    let (alice, carol, dave, erin) = (
        world.person().await,
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let create = request::Body::CreateInvite(CreateInvite {
        expires_in_s: 3600,
        max_uses: 3,
    });
    let response::Body::Invite(invite) = world.request(&alice, create).await.unwrap() else {
        panic!("not an invite");
    };
    assert_eq!(invite.code.len(), 10);
    assert_eq!((invite.max_uses, invite.uses), (3, 0));

    let redeem = |code: &str| {
        request::Body::RedeemInvite(RedeemInvite {
            code: code.to_owned(),
        })
    };
    // Codes are read case-insensitively.
    let lower = invite.code.to_lowercase();
    assert_eq!(
        world.request(&carol, redeem(&lower)).await.unwrap(),
        response::Body::InviteRedeemed(InviteRedeemed {
            user_id: id_bytes(alice.user.0),
        })
    );
    assert_eq!(
        contacts(&world, &carol).await,
        [contact(&alice, ContactState::Accepted)]
    );

    // The creator's block looks like an unknown code.
    world
        .svc
        .contact_block(alice.user, dave.user)
        .await
        .unwrap();
    let status = world
        .request(&dave, redeem(&invite.code))
        .await
        .unwrap_err();
    assert_eq!(failure(&status), (ErrorCode::InviteNotFound, true));

    let revoke = request::Body::RevokeInvite(RevokeInvite {
        code: invite.code.clone(),
    });
    assert_eq!(
        world.request(&alice, revoke).await.unwrap(),
        response::Body::Done(Done {})
    );
    let status = world
        .request(&erin, redeem(&invite.code))
        .await
        .unwrap_err();
    assert_eq!(failure(&status).0, ErrorCode::InviteNotFound);

    let too_many = request::Body::CreateInvite(CreateInvite {
        expires_in_s: 3600,
        max_uses: 101,
    });
    let status = world.request(&alice, too_many).await.unwrap_err();
    assert_eq!(failure(&status).0, ErrorCode::Invalid);
}

#[tokio::test]
async fn a_group_is_created_changed_and_handed_over_by_its_admins() {
    let world = world();
    let (alice, bob, carol, dave) = (
        world.person().await,
        world.person().await,
        world.person().await,
        world.person().await,
    );
    for other in [&bob, &carol, &dave] {
        world.befriend(&alice, other).await;
    }
    let create = request::Body::CreateGroup(CreateGroup {
        title: " Trip ".to_owned(),
        member_ids: vec![id_bytes(bob.user.0), id_bytes(carol.user.0)],
    });
    let response::Body::GroupCreated(created) = world.request(&alice, create).await.unwrap() else {
        panic!("not a created group");
    };
    let group = created.conversation_id;

    // Only admins change the group, and add only their own contacts.
    let rename = |title: &str| {
        request::Body::RenameGroup(RenameGroup {
            conversation_id: group.clone(),
            title: title.to_owned(),
        })
    };
    let status = world.request(&bob, rename("Bob's")).await.unwrap_err();
    assert_eq!(failure(&status), (ErrorCode::NotAdmin, true));
    world.request(&alice, rename("Holiday")).await.unwrap();
    let add = |person: &Person| {
        request::Body::AddMembers(AddMembers {
            conversation_id: group.clone(),
            user_ids: vec![id_bytes(person.user.0)],
        })
    };
    assert_eq!(
        world.request(&alice, add(&dave)).await.unwrap(),
        response::Body::Done(Done {})
    );
    let stranger = world.person().await;
    let status = world.request(&alice, add(&stranger)).await.unwrap_err();
    assert_eq!(failure(&status).0, ErrorCode::NotContact);

    // Several admins; the last one cannot step down.
    let set_role = |person: &Person, role: Role| {
        request::Body::SetRole(SetRole {
            conversation_id: group.clone(),
            user_id: id_bytes(person.user.0),
            role: role.into(),
        })
    };
    world
        .request(&alice, set_role(&bob, Role::Admin))
        .await
        .unwrap();
    world
        .request(&alice, set_role(&alice, Role::Member))
        .await
        .unwrap();
    let status = world
        .request(&bob, set_role(&bob, Role::Member))
        .await
        .unwrap_err();
    assert_eq!(failure(&status).0, ErrorCode::LastAdmin);
    let remove = request::Body::RemoveMember(RemoveMember {
        conversation_id: group.clone(),
        user_id: id_bytes(carol.user.0),
    });
    let status = world.request(&alice, remove.clone()).await.unwrap_err();
    assert_eq!(failure(&status).0, ErrorCode::NotAdmin);
    world.request(&bob, remove).await.unwrap();

    // The last admin leaves: the member who joined first, Alice, takes over.
    let leave = request::Body::LeaveGroup(LeaveGroup {
        conversation_id: group.clone(),
    });
    world.request(&bob, leave).await.unwrap();
    let response::Body::Conversations(listed) = world
        .request(&dave, list_conversations(None))
        .await
        .unwrap()
    else {
        panic!("not a conversation list");
    };
    let summary = &listed.conversations[0];
    assert_eq!(summary.kind(), ConversationKind::Group);
    assert_eq!(summary.title.as_deref(), Some("Holiday"));
    assert_eq!(summary.admins, [id_bytes(alice.user.0)]);
    let mut members = vec![id_bytes(alice.user.0), id_bytes(dave.user.0)];
    members.sort();
    assert_eq!(summary.members, members);
    assert!(matches!(
        summary.receipts,
        Some(conversation_summary::Receipts::Group(_))
    ));
}

#[tokio::test]
async fn group_requests_refuse_what_the_rules_forbid() {
    let world = world();
    let (alice, _bob, chat) = world.chat().await;
    let crowd = request::Body::CreateGroup(CreateGroup {
        title: "Crowd".to_owned(),
        member_ids: (0..100).map(|_| id_bytes(UserId::new().0)).collect(),
    });
    let status = world.request(&alice, crowd).await.unwrap_err();
    assert_eq!(failure(&status).0, ErrorCode::GroupFull);

    let malformed = request::Body::CreateGroup(CreateGroup {
        title: "Odd".to_owned(),
        member_ids: vec![vec![1, 2, 3]],
    });
    let status = world.request(&alice, malformed).await.unwrap_err();
    let error = error_from_status(&status);
    assert_eq!(error.code(), ErrorCode::Invalid);
    assert!(error.message.starts_with("member_ids"), "{}", error.message);

    let rename = request::Body::RenameGroup(RenameGroup {
        conversation_id: id_bytes(chat.0),
        title: "Not a group".to_owned(),
    });
    let status = world.request(&alice, rename).await.unwrap_err();
    assert_eq!(failure(&status).0, ErrorCode::NotAGroup);

    let unspecified = request::Body::SetRole(SetRole {
        conversation_id: id_bytes(chat.0),
        user_id: id_bytes(alice.user.0),
        role: Role::Unspecified.into(),
    });
    let status = world.request(&alice, unspecified).await.unwrap_err();
    assert_eq!(failure(&status), (ErrorCode::Invalid, true));

    // A title PostgreSQL could not store is refused as invalid, not retried as a store error.
    for title in ["a\u{0}b", "", &"t".repeat(101)] {
        let bad_title = request::Body::CreateGroup(CreateGroup {
            title: title.to_owned(),
            member_ids: vec![],
        });
        let status = world.request(&alice, bad_title).await.unwrap_err();
        assert_eq!(failure(&status).0, ErrorCode::Invalid, "{title:?}");
    }
}

async fn get_presence(
    world: &World,
    viewer: &Person,
    ids: Vec<Vec<u8>>,
) -> Result<Vec<Presence>, Status> {
    let get = request::Body::GetPresence(GetPresence { user_ids: ids });
    let response::Body::Presences(list) = world.request(viewer, get).await? else {
        panic!("not a presence list");
    };
    Ok(list.presences)
}

fn presence(person: &Person, state: PresenceState, last_seen: Option<i64>) -> Presence {
    Presence {
        user_id: id_bytes(person.user.0),
        state: state.into(),
        last_seen,
    }
}

async fn sharing(world: &World, of: &Person, share: Option<bool>) -> bool {
    let set = request::Body::SetPresenceSharing(SetPresenceSharing { share });
    let response::Body::PresenceSharing(PresenceSharing { share }) =
        world.request(of, set).await.unwrap()
    else {
        panic!("not a presence sharing answer");
    };
    share
}

#[tokio::test]
async fn get_presence_answers_unknown_for_strangers_and_the_state_for_contacts() {
    let world = world();
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let dave = world.person().await;
    world.befriend(&alice, &bob).await;
    world.befriend(&alice, &dave).await;
    let minute = Utc.with_ymd_and_hms(2026, 10, 13, 11, 58, 0).unwrap();
    world.presence.set(bob.user, PresenceView::Online);
    world.presence.set(
        dave.user,
        PresenceView::Offline {
            last_seen: Some(minute),
        },
    );
    world.presence.set(carol.user, PresenceView::Online);
    world.presence.set(alice.user, PresenceView::Online);

    let ids = [&bob, &carol, &dave, &alice]
        .iter()
        .map(|person| id_bytes(person.user.0))
        .chain([id_bytes(UserId::new().0)])
        .collect();
    let answered = get_presence(&world, &alice, ids).await.unwrap();
    assert_eq!(answered.len(), 5);
    assert_eq!(
        answered[..4],
        [
            presence(&bob, PresenceState::Online, None),
            presence(&carol, PresenceState::Unknown, None),
            presence(
                &dave,
                PresenceState::Offline,
                Some(minute.timestamp_micros())
            ),
            presence(&alice, PresenceState::Online, None),
        ]
    );
    assert_eq!(answered[4].state(), PresenceState::Unknown);
    assert_eq!(answered[4].last_seen, None);
    assert!(
        world
            .notifier
            .take()
            .iter()
            .all(|event| !matches!(event, Event::Presence { .. }))
    );
}

#[tokio::test]
async fn get_presence_refuses_more_than_100_ids() {
    let world = world();
    let alice = world.person().await;
    let crowd = (0..101).map(|_| id_bytes(UserId::new().0)).collect();
    let status = get_presence(&world, &alice, crowd).await.unwrap_err();
    assert_eq!(failure(&status), (ErrorCode::Invalid, true));
    let status = get_presence(&world, &alice, vec![]).await.unwrap_err();
    assert_eq!(failure(&status), (ErrorCode::Invalid, true));
    let status = get_presence(&world, &alice, vec![vec![1, 2, 3]])
        .await
        .unwrap_err();
    assert_eq!(failure(&status), (ErrorCode::Invalid, true));

    let full = (0..100).map(|_| id_bytes(UserId::new().0)).collect();
    assert_eq!(get_presence(&world, &alice, full).await.unwrap().len(), 100);
}

#[tokio::test]
async fn get_presence_answers_once_per_distinct_id_in_order() {
    let world = world();
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    world.befriend(&alice, &bob).await;
    world.presence.set(bob.user, PresenceView::Online);
    let ids = vec![
        id_bytes(carol.user.0),
        id_bytes(bob.user.0),
        id_bytes(carol.user.0),
        id_bytes(bob.user.0),
    ];
    assert_eq!(
        get_presence(&world, &alice, ids).await.unwrap(),
        [
            presence(&carol, PresenceState::Unknown, None),
            presence(&bob, PresenceState::Online, None),
        ]
    );
}

#[tokio::test]
async fn get_presence_announces_users_the_read_found_newly_offline() {
    let world = world();
    let (alice, bob) = (world.person().await, world.person().await);
    world.befriend(&alice, &bob).await;
    world.presence.flip(bob.user);
    world.notifier.take();
    let offline = PresenceView::Offline { last_seen: None };

    let ids = vec![id_bytes(bob.user.0)];
    assert_eq!(
        get_presence(&world, &alice, ids).await.unwrap(),
        [presence(&bob, PresenceState::Offline, None)]
    );
    assert_eq!(
        world.notifier.take(),
        vec![Event::Presence {
            to: vec![alice.user],
            user: bob.user,
            view: offline,
        }]
    );
}

#[tokio::test]
async fn opting_out_answers_unknown_to_contacts_and_hides_them_from_the_hider() {
    let world = world();
    let (alice, bob) = (world.person().await, world.person().await);
    world.befriend(&alice, &bob).await;
    world.presence.set(alice.user, PresenceView::Online);
    world.presence.set(bob.user, PresenceView::Online);
    world.notifier.take();

    assert!(!sharing(&world, &alice, Some(false)).await);
    assert_eq!(
        world.notifier.take(),
        vec![Event::Presence {
            to: vec![bob.user],
            user: alice.user,
            view: PresenceView::Unknown,
        }]
    );
    let of_alice = vec![id_bytes(alice.user.0)];
    assert_eq!(
        get_presence(&world, &bob, of_alice.clone()).await.unwrap(),
        [presence(&alice, PresenceState::Unknown, None)]
    );
    let of_bob = vec![id_bytes(bob.user.0)];
    assert_eq!(
        get_presence(&world, &alice, of_bob.clone()).await.unwrap(),
        [presence(&bob, PresenceState::Unknown, None)]
    );
    // The hider still sees itself.
    assert_eq!(
        get_presence(&world, &alice, of_alice.clone())
            .await
            .unwrap(),
        [presence(&alice, PresenceState::Online, None)]
    );

    assert!(sharing(&world, &alice, Some(true)).await);
    assert_eq!(
        world.notifier.take(),
        vec![Event::Presence {
            to: vec![bob.user],
            user: alice.user,
            view: PresenceView::Online,
        }]
    );
    assert_eq!(
        get_presence(&world, &bob, of_alice).await.unwrap(),
        [presence(&alice, PresenceState::Online, None)]
    );
}

#[tokio::test]
async fn set_presence_sharing_without_a_value_reads_it() {
    let world = world();
    let alice = world.person().await;
    assert!(sharing(&world, &alice, None).await);
    assert!(!sharing(&world, &alice, Some(false)).await);
    assert!(!sharing(&world, &alice, None).await);
    assert!(!sharing(&world, &alice, Some(false)).await);
    assert!(sharing(&world, &alice, Some(true)).await);
    assert!(sharing(&world, &alice, None).await);
}
