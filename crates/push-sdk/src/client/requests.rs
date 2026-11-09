//! The client's requests (spec §7.1, §5.5): they go over the connection once it is up and
//! are answered there; and auth-svc's calls for a signed-in device.

use push_domain::{
    ConversationId, DeviceId, MAX_INVITE_LIFETIME_SECS, MAX_INVITE_USES, MIN_INVITE_LIFETIME_SECS,
    UserId,
};
use push_proto::v1::{
    AddMembers, ContactAccept, ContactBlock, ContactDecline, ContactRequest, ContactUnblock,
    CreateGroup, CreateInvite, FetchHistory, GetAgentSettings, HideConversation, ListContacts,
    ListConversations, ListOutgoingActions, OpenDirect, RedeemInvite, RevokeInvite,
    UnhideConversation, request, response,
};
use tokio::sync::oneshot;
use uuid::Uuid;

use super::Client;
use crate::SdkError;
use crate::auth::{Device, Profile};
use crate::connection::Command;
use crate::model::{ConnectionState, Contact, ContactState, Conversation, Event, Invite, Message};
use crate::outgoing::{AgentSettings, OutgoingAction};
use crate::store::bytes;

impl Client {
    /// Sends a request once connected, waiting for the connection and then for the
    /// answer up to the request timeout each. Fails at once while signed out, removed,
    /// opened elsewhere or not asked to connect.
    pub(super) async fn request(&self, body: request::Body) -> Result<response::Body, SdkError> {
        let inner = self.inner.clone();
        self.run(async move {
            let mut state = inner.state.clone();
            let ready = tokio::time::timeout(
                inner.request_timeout,
                state.wait_for(|state| {
                    !matches!(
                        state,
                        ConnectionState::Connecting | ConnectionState::Reconnecting { .. }
                    )
                }),
            )
            .await;
            match ready.map(|state| state.map(|state| *state)) {
                Ok(Ok(ConnectionState::Connected)) => {}
                Ok(Ok(ConnectionState::SignedOut)) => return Err(SdkError::NotSignedIn),
                Ok(Ok(ConnectionState::DeviceRemoved)) => return Err(SdkError::DeviceRemoved),
                Ok(Ok(ConnectionState::OpenedElsewhere)) => {
                    return Err(SdkError::OpenedElsewhere);
                }
                Ok(Ok(_)) | Err(_) => return Err(SdkError::Offline),
                Ok(Err(_)) => return Err(SdkError::Closed),
            }
            let (reply, answer) = oneshot::channel();
            inner
                .commands
                .send(Command::Request { body, reply })
                .map_err(|_| SdkError::Closed)?;
            match tokio::time::timeout(inner.request_timeout, answer).await {
                Ok(Ok(result)) => result,
                Ok(Err(_)) => Err(SdkError::Disconnected),
                Err(_) => Err(SdkError::Timeout),
            }
        })
        .await
    }

    /// Lists the conversations from the server and stores what changed. Hidden ones are
    /// listed too, with `hidden` set.
    pub async fn list_conversations(&self) -> Result<Vec<Conversation>, SdkError> {
        let body = request::Body::ListConversations(ListConversations {
            changed_since: None,
            include_hidden: true,
        });
        let response::Body::Conversations(list) = self.request(body).await? else {
            return Err(unexpected());
        };
        for summary in &list.conversations {
            if let Some(changed) = self.inner.shared.store().apply_summary(summary)? {
                self.inner.shared.emit(Event::ConversationChanged(changed));
            }
        }
        self.conversations()
    }

    /// Hides a direct conversation from the user's own listings, on all of their devices;
    /// the peer's view is unchanged. A new message in it shows it again. Idempotent.
    /// `invalid` for a group.
    pub async fn hide_conversation(&self, id: ConversationId) -> Result<Conversation, SdkError> {
        self.set_hidden(id, true).await
    }

    /// Shows a hidden direct conversation again, on all of the user's devices. Idempotent.
    pub async fn unhide_conversation(&self, id: ConversationId) -> Result<Conversation, SdkError> {
        self.set_hidden(id, false).await
    }

    async fn set_hidden(&self, id: ConversationId, hidden: bool) -> Result<Conversation, SdkError> {
        let conversation_id = bytes(id.0);
        let body = if hidden {
            request::Body::HideConversation(HideConversation { conversation_id })
        } else {
            request::Body::UnhideConversation(UnhideConversation { conversation_id })
        };
        let response::Body::Done(_) = self.request(body).await? else {
            return Err(unexpected());
        };
        let stored = self.inner.shared.store().conversation(id)?;
        let Some(stored) = stored else {
            // Not stored yet: a listing brings it with its flags.
            return self
                .list_conversations()
                .await?
                .into_iter()
                .find(|conversation| conversation.id == id)
                .ok_or_else(unexpected);
        };
        let changed = self
            .inner
            .shared
            .store()
            .apply_flags(id, hidden, stored.peer_deleted)?;
        match changed {
            Some(changed) => {
                self.inner
                    .shared
                    .emit(Event::ConversationChanged(changed.clone()));
                Ok(changed)
            }
            None => Ok(stored),
        }
    }

    /// Loads older messages from the server (spec §4.6: messages and system entries in
    /// their current state) and stores them; oldest first.
    pub async fn fetch_history(
        &self,
        conversation: ConversationId,
        before_seq: Option<i64>,
        limit: u32,
    ) -> Result<Vec<Message>, SdkError> {
        let body = request::Body::FetchHistory(FetchHistory {
            conversation_id: bytes(conversation.0),
            before_seq,
            limit,
        });
        let response::Body::History(page) = self.request(body).await? else {
            return Err(unexpected());
        };
        self.inner.shared.store().store_history(&page.entries)
    }

    /// The direct conversation with an accepted contact, created the first time.
    pub async fn open_direct(&self, peer: UserId) -> Result<Conversation, SdkError> {
        let body = request::Body::OpenDirect(OpenDirect {
            peer_user_id: bytes(peer.0),
        });
        let response::Body::DirectOpened(opened) = self.request(body).await? else {
            return Err(unexpected());
        };
        let id = Uuid::from_slice(&opened.conversation_id).map_err(|_| unexpected())?;
        let conversation = self
            .inner
            .shared
            .store()
            .open_direct(ConversationId(id), peer)?;
        self.inner
            .shared
            .emit(Event::ConversationChanged(conversation.clone()));
        Ok(conversation)
    }

    /// Lists the user's relationships from the server and stores what changed.
    pub async fn list_contacts(&self) -> Result<Vec<Contact>, SdkError> {
        let response::Body::Contacts(list) = self
            .request(request::Body::ListContacts(ListContacts {}))
            .await?
        else {
            return Err(unexpected());
        };
        let listed: Vec<Contact> = list
            .contacts
            .iter()
            .filter_map(|contact| {
                Some(Contact {
                    user_id: UserId(Uuid::from_slice(&contact.user_id).ok()?),
                    state: ContactState::from_wire(contact.state()),
                })
            })
            .collect();
        let changed = self.inner.shared.store().replace_contacts(&listed)?;
        for contact in changed {
            self.inner.shared.emit(Event::ContactChanged(contact));
        }
        Ok(listed)
    }

    /// Runs a contact request that answers `Done`, then lists the contacts: the user's
    /// relationship with `user` now.
    async fn contact_change(&self, body: request::Body, user: UserId) -> Result<Contact, SdkError> {
        self.request(body).await?;
        let listed = self.list_contacts().await?;
        Ok(listed
            .into_iter()
            .find(|contact| contact.user_id == user)
            .unwrap_or(Contact {
                user_id: user,
                state: ContactState::None,
            }))
    }

    /// Asks `user` to become a contact (spec §5.5).
    pub async fn request_contact(&self, user: UserId) -> Result<Contact, SdkError> {
        let body = request::Body::ContactRequest(ContactRequest {
            user_id: bytes(user.0),
        });
        let response::Body::Contact(contact) = self.request(body).await? else {
            return Err(unexpected());
        };
        let contact = Contact {
            user_id: user,
            state: ContactState::from_wire(contact.state()),
        };
        if self.inner.shared.store().set_contact(contact)? {
            self.inner.shared.emit(Event::ContactChanged(contact));
        }
        Ok(contact)
    }

    pub async fn accept_contact(&self, user: UserId) -> Result<Contact, SdkError> {
        let body = request::Body::ContactAccept(ContactAccept {
            user_id: bytes(user.0),
        });
        self.contact_change(body, user).await
    }

    pub async fn decline_contact(&self, user: UserId) -> Result<Contact, SdkError> {
        let body = request::Body::ContactDecline(ContactDecline {
            user_id: bytes(user.0),
        });
        self.contact_change(body, user).await
    }

    pub async fn block(&self, user: UserId) -> Result<Contact, SdkError> {
        let body = request::Body::ContactBlock(ContactBlock {
            user_id: bytes(user.0),
        });
        self.contact_change(body, user).await
    }

    pub async fn unblock(&self, user: UserId) -> Result<Contact, SdkError> {
        let body = request::Body::ContactUnblock(ContactUnblock {
            user_id: bytes(user.0),
        });
        self.contact_change(body, user).await
    }

    /// An invite code (spec §5.5): valid for `expires_in_s` (60 s to 30 days) and
    /// `max_uses` redemptions (1 to 100).
    pub async fn create_invite(
        &self,
        expires_in_s: u32,
        max_uses: u32,
    ) -> Result<Invite, SdkError> {
        if !(MIN_INVITE_LIFETIME_SECS..=MAX_INVITE_LIFETIME_SECS).contains(&i64::from(expires_in_s))
        {
            return Err(SdkError::Invalid(format!(
                "an invite lasts {MIN_INVITE_LIFETIME_SECS} to {MAX_INVITE_LIFETIME_SECS} seconds"
            )));
        }
        if !(1..=MAX_INVITE_USES).contains(&max_uses) {
            return Err(SdkError::Invalid(format!(
                "an invite allows 1 to {MAX_INVITE_USES} uses"
            )));
        }
        let body = request::Body::CreateInvite(CreateInvite {
            expires_in_s,
            max_uses,
        });
        let response::Body::Invite(invite) = self.request(body).await? else {
            return Err(unexpected());
        };
        Ok(Invite {
            code: invite.code,
            expires_at: invite.expires_at,
            max_uses: invite.max_uses,
            uses: invite.uses,
        })
    }

    /// Redeems an invite: its creator becomes an accepted contact. Returns the creator.
    pub async fn redeem_invite(&self, code: &str) -> Result<UserId, SdkError> {
        let body = request::Body::RedeemInvite(RedeemInvite {
            code: code.to_owned(),
        });
        let response::Body::InviteRedeemed(redeemed) = self.request(body).await? else {
            return Err(unexpected());
        };
        let creator = UserId(Uuid::from_slice(&redeemed.user_id).map_err(|_| unexpected())?);
        let contact = Contact {
            user_id: creator,
            state: ContactState::Accepted,
        };
        if self.inner.shared.store().set_contact(contact)? {
            self.inner.shared.emit(Event::ContactChanged(contact));
        }
        Ok(creator)
    }

    pub async fn revoke_invite(&self, code: &str) -> Result<(), SdkError> {
        let body = request::Body::RevokeInvite(RevokeInvite {
            code: code.to_owned(),
        });
        self.request(body).await?;
        Ok(())
    }

    /// Creates a group with the user as its admin and `members` (accepted contacts, at
    /// most 99) in it; `title` is 1 to 100 characters after trimming. Then lists the
    /// conversations: the group as the server lists it.
    ///
    /// An agent passes its owner's outgoing policy for groups first:
    /// `waiting_for_owner` (the error's [`SdkError::action_id`] names the action; once the
    /// owner approves it the group is created, [`Event::OutgoingActionChanged`]) or
    /// `not_allowed_by_owner`.
    pub async fn create_group(
        &self,
        title: &str,
        members: Vec<UserId>,
    ) -> Result<Conversation, SdkError> {
        let body = request::Body::CreateGroup(CreateGroup {
            title: title.to_owned(),
            member_ids: members.iter().map(|member| bytes(member.0)).collect(),
        });
        let response::Body::GroupCreated(created) = self.request(body).await? else {
            return Err(unexpected());
        };
        let id =
            ConversationId(Uuid::from_slice(&created.conversation_id).map_err(|_| unexpected())?);
        self.list_conversations()
            .await?
            .into_iter()
            .find(|conversation| conversation.id == id)
            .ok_or_else(unexpected)
    }

    /// Adds `users` (accepted contacts of the user) to a group the user is an admin of;
    /// they see the log from the entry that adds them. An agent passes its owner's
    /// outgoing policy for groups first, as [`Client::create_group`] does.
    pub async fn add_members(
        &self,
        group: ConversationId,
        users: Vec<UserId>,
    ) -> Result<(), SdkError> {
        let body = request::Body::AddMembers(AddMembers {
            conversation_id: bytes(group.0),
            user_ids: users.iter().map(|user| bytes(user.0)).collect(),
        });
        let response::Body::Done(_) = self.request(body).await? else {
            return Err(unexpected());
        };
        Ok(())
    }

    /// The user is an agent: its settings, as its owner set them. `not_allowed` for
    /// people.
    pub async fn agent_settings(&self) -> Result<AgentSettings, SdkError> {
        let body = request::Body::GetAgentSettings(GetAgentSettings {});
        let response::Body::AgentSettings(settings) = self.request(body).await? else {
            return Err(unexpected());
        };
        AgentSettings::from_wire(&settings).ok_or_else(unexpected)
    }

    /// The user is an agent: its actions waiting for its owner and those resolved in the
    /// last 24 hours, oldest first. `not_allowed` for people. (An owner lists an agent's
    /// waiting actions with [`Client::agent_outgoing`].)
    pub async fn outgoing_actions(&self) -> Result<Vec<OutgoingAction>, SdkError> {
        let body = request::Body::ListOutgoingActions(ListOutgoingActions {});
        let response::Body::OutgoingActions(list) = self.request(body).await? else {
            return Err(unexpected());
        };
        list.actions
            .iter()
            .map(OutgoingAction::from_wire)
            .collect::<Option<_>>()
            .ok_or_else(unexpected)
    }

    // ----- auth-svc, signed in -----

    /// The profiles among `ids` the user may see: themselves, contacts, and anyone they
    /// share or shared a conversation with (spec §5.5).
    pub async fn profiles(&self, ids: Vec<UserId>) -> Result<Vec<Profile>, SdkError> {
        let inner = self.inner.clone();
        self.run(async move {
            let session = inner.credentials.fresh().await?;
            inner.auth.profiles(&session.access_token, &ids).await
        })
        .await
    }

    /// The verified user with this handle.
    pub async fn lookup(&self, handle: &str) -> Result<Profile, SdkError> {
        let (inner, handle) = (self.inner.clone(), handle.to_owned());
        self.run(async move {
            let session = inner.credentials.fresh().await?;
            inner.auth.lookup(&session.access_token, &handle).await
        })
        .await
    }

    /// Changes the user's display name, handle or both (at least one); the profile after
    /// the change. A handle someone else holds is `handle_taken`; the old handle is free
    /// at once. Any signed-in user may do this, with an Entra ID account too.
    pub async fn update_profile(
        &self,
        display_name: Option<&str>,
        handle: Option<&str>,
    ) -> Result<Profile, SdkError> {
        if display_name.is_none() && handle.is_none() {
            return Err(SdkError::Invalid(
                "give a display name, a handle or both".to_owned(),
            ));
        }
        let inner = self.inner.clone();
        let (display_name, handle) = (display_name.map(str::to_owned), handle.map(str::to_owned));
        self.run(async move {
            let session = inner.credentials.fresh().await?;
            inner
                .auth
                .update_profile(
                    &session.access_token,
                    display_name.as_deref(),
                    handle.as_deref(),
                )
                .await
        })
        .await
    }

    /// The user's devices.
    pub async fn devices(&self) -> Result<Vec<Device>, SdkError> {
        let inner = self.inner.clone();
        self.run(async move {
            let session = inner.credentials.fresh().await?;
            inner.auth.devices(&session.access_token).await
        })
        .await
    }

    /// Revokes another of the user's devices; it is disconnected within seconds.
    pub async fn revoke_device(&self, device: DeviceId) -> Result<(), SdkError> {
        let inner = self.inner.clone();
        self.run(async move {
            let session = inner.credentials.fresh().await?;
            inner
                .auth
                .revoke_device(&session.access_token, device)
                .await
        })
        .await
    }
}

pub(super) fn unexpected() -> SdkError {
    SdkError::UnexpectedAnswer("the server answered with something unexpected".to_owned())
}

#[cfg(test)]
mod tests {
    use push_proto::v1::{self, frame::Body};

    use super::*;
    use crate::client::tests::Fixture;
    use crate::testing::{recv_until, send};

    #[tokio::test]
    async fn requests_fail_fast_offline_and_round_trip_once_connected() {
        let mut f = Fixture::new().await;
        let peer = UserId::new();
        assert_eq!(f.client.open_direct(peer).await, Err(SdkError::Offline));

        f.client.connect().unwrap();
        let mut socket = f.ready().await;
        let client = f.client.clone();
        let opening = tokio::spawn(async move { client.open_direct(peer).await });
        let Body::Request(request) = recv_until(&mut socket, |body| {
            matches!(body, Body::Request(request) if matches!(request.body, Some(request::Body::OpenDirect(_))))
        })
        .await
        else {
            unreachable!();
        };
        let conversation = ConversationId::new();
        let answer = v1::Response {
            request_id: request.request_id,
            body: Some(response::Body::DirectOpened(v1::DirectOpened {
                conversation_id: bytes(conversation.0),
            })),
        };
        send(&mut socket, Body::Response(answer)).await;
        let opened = opening.await.unwrap().unwrap();
        assert_eq!((opened.id, opened.peer), (conversation, Some(peer)));
        assert_eq!(f.client.conversation(conversation).unwrap(), Some(opened));
    }

    #[tokio::test]
    async fn a_request_right_after_connect_waits_for_the_connection() {
        let mut f = Fixture::new().await;
        f.client.connect().unwrap();
        // Set by `connect` itself, before the connection task has even seen the command.
        assert_eq!(f.client.connection_state(), ConnectionState::Connecting);
        let client = f.client.clone();
        let peer = UserId::new();
        let opening = tokio::spawn(async move { client.open_direct(peer).await });
        let mut socket = f.ready().await;
        let Body::Request(request) = recv_until(&mut socket, |body| {
            matches!(body, Body::Request(request) if matches!(request.body, Some(request::Body::OpenDirect(_))))
        })
        .await
        else {
            unreachable!();
        };
        let answer = v1::Response {
            request_id: request.request_id,
            body: Some(response::Body::DirectOpened(v1::DirectOpened {
                conversation_id: bytes(Uuid::now_v7()),
            })),
        };
        send(&mut socket, Body::Response(answer)).await;
        assert!(opening.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn an_unanswered_or_misanswered_request_is_not_retryable() {
        let mut f = Fixture::new().await;
        f.client.connect().unwrap();
        let mut socket = f.ready().await;
        let (client, peer) = (f.client.clone(), UserId::new());
        let unanswered = tokio::spawn(async move { client.create_invite(600, 1).await });
        recv_until(&mut socket, |body| matches!(body, Body::Request(_))).await;
        let error = unanswered.await.unwrap().unwrap_err();
        assert_eq!(error, SdkError::Timeout);
        assert_eq!(error.code(), "timeout");
        assert!(!error.is_retryable());

        let client = f.client.clone();
        let wrong = tokio::spawn(async move { client.open_direct(peer).await });
        let Body::Request(request) = recv_until(&mut socket, |body| {
            matches!(body, Body::Request(request) if matches!(request.body, Some(request::Body::OpenDirect(_))))
        })
        .await
        else {
            unreachable!();
        };
        let answer = v1::Response {
            request_id: request.request_id,
            body: Some(response::Body::Conversations(Default::default())),
        };
        send(&mut socket, Body::Response(answer)).await;
        let error = wrong.await.unwrap().unwrap_err();
        assert_eq!(error.code(), "unexpected_answer");
        assert!(!error.is_retryable());
    }

    #[tokio::test]
    async fn an_invite_is_checked_before_it_is_asked_for() {
        let f = Fixture::new().await;
        let invalid =
            |result: Result<Invite, SdkError>| matches!(result, Err(SdkError::Invalid(_)));
        assert!(invalid(f.client.create_invite(30, 1).await));
        assert!(invalid(f.client.create_invite(600, 0).await));
    }

    /// The next request the app made (not the engine's own listings).
    async fn next_request(socket: &mut crate::testing::Server) -> v1::Request {
        let Body::Request(request) = recv_until(socket, |body| {
            matches!(body, Body::Request(request)
                if request.request_id < crate::engine::INTERNAL_REQUESTS)
        })
        .await
        else {
            unreachable!();
        };
        request
    }

    async fn answer(socket: &mut crate::testing::Server, request_id: u64, body: response::Body) {
        let answer = v1::Response {
            request_id,
            body: Some(body),
        };
        send(socket, Body::Response(answer)).await;
    }

    #[tokio::test]
    async fn a_group_is_created_then_listed_and_members_are_added() {
        let mut f = Fixture::new().await;
        f.client.connect().unwrap();
        let mut socket = f.ready().await;
        let (one, two) = (UserId::new(), UserId::new());
        let client = f.client.clone();
        let creating =
            tokio::spawn(async move { client.create_group("plans", vec![one, two]).await });
        let request = next_request(&mut socket).await;
        let Some(request::Body::CreateGroup(create)) = &request.body else {
            panic!("{request:?}");
        };
        assert_eq!(create.title, "plans");
        assert_eq!(create.member_ids, [bytes(one.0), bytes(two.0)]);
        let group = ConversationId::new();
        answer(
            &mut socket,
            request.request_id,
            response::Body::GroupCreated(v1::GroupCreated {
                conversation_id: bytes(group.0),
            }),
        )
        .await;
        let listing = next_request(&mut socket).await;
        assert!(matches!(
            listing.body,
            Some(request::Body::ListConversations(_))
        ));
        let summary = v1::ConversationSummary {
            conversation_id: bytes(group.0),
            kind: v1::ConversationKind::Group.into(),
            title: Some("plans".to_owned()),
            members: vec![bytes(f.auth.user.0), bytes(one.0), bytes(two.0)],
            started_by_agent: true,
            ..Default::default()
        };
        answer(
            &mut socket,
            listing.request_id,
            response::Body::Conversations(v1::ConversationList {
                conversations: vec![summary],
            }),
        )
        .await;
        let created = creating.await.unwrap().unwrap();
        assert_eq!(created.id, group);
        assert_eq!(created.kind, crate::ConversationKind::Group);
        assert_eq!(created.title.as_deref(), Some("plans"));
        assert!(created.started_by_agent);

        let client = f.client.clone();
        let adding = tokio::spawn(async move { client.add_members(group, vec![two]).await });
        let request = next_request(&mut socket).await;
        let Some(request::Body::AddMembers(add)) = &request.body else {
            panic!("{request:?}");
        };
        assert_eq!(
            (&add.conversation_id, &add.user_ids),
            (&bytes(group.0), &vec![bytes(two.0)])
        );
        answer(
            &mut socket,
            request.request_id,
            response::Body::Done(v1::Done {}),
        )
        .await;
        adding.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn an_action_waiting_for_the_owner_is_an_error_naming_it() {
        let mut f = Fixture::new().await;
        f.client.connect().unwrap();
        let mut socket = f.ready().await;
        let client = f.client.clone();
        let peer = UserId::new();
        let opening = tokio::spawn(async move { client.open_direct(peer).await });
        let request = next_request(&mut socket).await;
        let action = Uuid::now_v7();
        let refusal = v1::Error {
            code: v1::ErrorCode::WaitingForOwner.into(),
            message: "waits for the owner".to_owned(),
            request_id: Some(request.request_id),
            action_id: Some(bytes(action)),
            ..Default::default()
        };
        send(&mut socket, Body::Error(refusal)).await;
        let error = opening.await.unwrap().unwrap_err();
        assert_eq!(
            error,
            SdkError::Server {
                code: "waiting_for_owner".to_owned(),
                message: "waits for the owner".to_owned(),
                retryable: false,
                retry_after_s: None,
                action_id: Some(action.to_string()),
            }
        );
    }

    #[tokio::test]
    async fn an_agent_reads_its_settings_and_actions() {
        let mut f = Fixture::new().await;
        f.client.connect().unwrap();
        let mut socket = f.ready().await;
        let client = f.client.clone();
        let reading = tokio::spawn(async move { client.agent_settings().await });
        let request = next_request(&mut socket).await;
        assert!(matches!(
            request.body,
            Some(request::Body::GetAgentSettings(_))
        ));
        let settings = v1::AgentSettings {
            inbound: v1::InboundPolicy::OwnerOnly.into(),
            requests: v1::OutgoingPolicy::Ask.into(),
            chats: v1::OutgoingPolicy::Ask.into(),
            groups: v1::OutgoingPolicy::Off.into(),
            calls: v1::OutgoingPolicy::Allowed.into(),
        };
        answer(
            &mut socket,
            request.request_id,
            response::Body::AgentSettings(settings),
        )
        .await;
        let settings = reading.await.unwrap().unwrap();
        assert_eq!(settings.inbound, crate::InboundPolicy::OwnerOnly);
        assert_eq!(settings.calls, crate::OutgoingPolicy::Allowed);

        let client = f.client.clone();
        let listing = tokio::spawn(async move { client.outgoing_actions().await });
        let request = next_request(&mut socket).await;
        assert!(matches!(
            request.body,
            Some(request::Body::ListOutgoingActions(_))
        ));
        let (action, group) = (Uuid::now_v7(), ConversationId::new());
        let actions = vec![v1::OutgoingAction {
            action_id: bytes(action),
            agent_user_id: bytes(f.auth.user.0),
            kind: v1::OutgoingKind::Invite.into(),
            conversation_id: bytes(group.0),
            member_ids: vec![bytes(Uuid::now_v7())],
            state: v1::OutgoingState::Waiting.into(),
            created_at: 5_000,
            ..Default::default()
        }];
        answer(
            &mut socket,
            request.request_id,
            response::Body::OutgoingActions(v1::OutgoingActionList { actions }),
        )
        .await;
        let listed = listing.await.unwrap().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].action_id, action.to_string());
        assert_eq!(listed[0].kind, crate::OutgoingKind::Invite);
        assert_eq!(listed[0].conversation, Some(group));
        assert_eq!(listed[0].state, crate::OutgoingState::Waiting);
        assert_eq!(
            (listed[0].created_at_ms, listed[0].resolved_at_ms),
            (5, None)
        );
    }
}
