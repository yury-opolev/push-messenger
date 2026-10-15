//! The client's requests (spec §7.1, §5.5): they go over the connection once it is up and
//! are answered there; and auth-svc's calls for a signed-in device.

use push_domain::{
    ConversationId, DeviceId, MAX_INVITE_LIFETIME_SECS, MAX_INVITE_USES, MIN_INVITE_LIFETIME_SECS,
    UserId,
};
use push_proto::v1::{
    ContactAccept, ContactBlock, ContactDecline, ContactRequest, ContactUnblock, CreateInvite,
    FetchHistory, ListContacts, ListConversations, OpenDirect, RedeemInvite, RevokeInvite, request,
    response,
};
use tokio::sync::oneshot;
use uuid::Uuid;

use super::Client;
use crate::SdkError;
use crate::auth::{Device, Profile};
use crate::connection::Command;
use crate::model::{ConnectionState, Contact, ContactState, Conversation, Event, Invite, Message};
use crate::store::bytes;

impl Client {
    /// Sends a request once connected, waiting for the connection and then for the
    /// answer up to the request timeout each. Fails at once while signed out, removed,
    /// opened elsewhere or not asked to connect.
    async fn request(&self, body: request::Body) -> Result<response::Body, SdkError> {
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
                Err(_) => Err(SdkError::Network("no answer in time".to_owned())),
            }
        })
        .await
    }

    /// Lists the conversations from the server and stores what changed.
    pub async fn list_conversations(&self) -> Result<Vec<Conversation>, SdkError> {
        let body = request::Body::ListConversations(ListConversations::default());
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

fn unexpected() -> SdkError {
    SdkError::Network("the server answered with something unexpected".to_owned())
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
    async fn an_invite_is_checked_before_it_is_asked_for() {
        let f = Fixture::new().await;
        let invalid =
            |result: Result<Invite, SdkError>| matches!(result, Err(SdkError::Invalid(_)));
        assert!(invalid(f.client.create_invite(30, 1).await));
        assert!(invalid(f.client.create_invite(600, 0).await));
    }
}
