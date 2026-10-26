//! The client's frames beyond Send and Ack (spec §7.1): Edit, Delete, ReadUpTo and the
//! request frames, from protobuf to the domain and back. The gateway calls them on behalf
//! of the user and device it verified.

use chrono::TimeDelta;
use push_domain::{
    ClientMsgId, Clock, ConversationId, ConversationKind, ConversationSummary, DeleteMessage,
    DeviceId, DomainResult, EditText, MAX_HISTORY_PAGE, MessageId, MessageRef, Messaging, Notifier,
    Receipts, Role, Store, UserId,
};
use push_proto::internal::v1::{
    DeleteRequest, EditRequest, ReadUpToRequest, ReadUpToResponse, RequestCall,
};
use push_proto::v1::{
    self, Contact, ContactList, ConversationList, DirectOpened, DirectReceipts, Done, GroupCreated,
    GroupReceipts, HistoryPage, Invite, InviteRedeemed, ListContacts, SendAck,
    conversation_summary, delete, edit, request, response,
};
use push_proto::{error_from_status, error_status, id_bytes};
use tonic::Status;

use crate::convert::{
    contact_state, deliver, domain_error, from_micros, invalid, micros, parse_id, send_ack,
};

/// `Edit`: the edit entry's receipt, after commit.
pub(crate) async fn edit<S: Store, C: Clock, N: Notifier>(
    messaging: &Messaging<S, C, N>,
    call: EditRequest,
) -> Result<SendAck, Status> {
    let sender = parse_id(&call.user_id, "user_id", UserId)?;
    let device = parse_id(&call.device_id, "device_id", DeviceId)?;
    let edit = call.edit.ok_or_else(|| invalid("edit is required"))?;
    let target = match edit.target {
        Some(edit::Target::TargetMessageId(id)) => by_message_id(&id)?,
        Some(edit::Target::TargetClientMsgId(id)) => by_client_msg_id(&id)?,
        None => return Err(invalid("target is required")),
    };
    let command = EditText {
        sender,
        device,
        conversation: parse_id(&edit.conversation_id, "conversation_id", ConversationId)?,
        client_msg_id: parse_id(&edit.client_msg_id, "client_msg_id", ClientMsgId)?,
        target,
        body: edit.body,
        client_ts: edit.client_ts.map(from_micros).transpose()?,
    };
    let receipt = messaging.edit_text(command).await.map_err(domain_error)?;
    Ok(send_ack(&receipt))
}

/// `Delete`: the delete entry's receipt, after commit.
pub(crate) async fn delete<S: Store, C: Clock, N: Notifier>(
    messaging: &Messaging<S, C, N>,
    call: DeleteRequest,
) -> Result<SendAck, Status> {
    let sender = parse_id(&call.user_id, "user_id", UserId)?;
    let device = parse_id(&call.device_id, "device_id", DeviceId)?;
    let delete = call.delete.ok_or_else(|| invalid("delete is required"))?;
    let target = match delete.target {
        Some(delete::Target::TargetMessageId(id)) => by_message_id(&id)?,
        Some(delete::Target::TargetClientMsgId(id)) => by_client_msg_id(&id)?,
        None => return Err(invalid("target is required")),
    };
    let command = DeleteMessage {
        sender,
        device,
        conversation: parse_id(&delete.conversation_id, "conversation_id", ConversationId)?,
        client_msg_id: parse_id(&delete.client_msg_id, "client_msg_id", ClientMsgId)?,
        target,
    };
    let receipt = messaging
        .delete_message(command)
        .await
        .map_err(domain_error)?;
    Ok(send_ack(&receipt))
}

fn by_message_id(id: &[u8]) -> Result<MessageRef, Status> {
    Ok(MessageRef::Id(parse_id(
        id,
        "target_message_id",
        MessageId,
    )?))
}

fn by_client_msg_id(id: &[u8]) -> Result<MessageRef, Status> {
    Ok(MessageRef::ClientId(parse_id(
        id,
        "target_client_msg_id",
        ClientMsgId,
    )?))
}

/// `ReadUpTo`: whether the user's read position moved.
pub(crate) async fn read_up_to<S: Store, C: Clock, N: Notifier>(
    messaging: &Messaging<S, C, N>,
    call: ReadUpToRequest,
) -> Result<ReadUpToResponse, Status> {
    let user = parse_id(&call.user_id, "user_id", UserId)?;
    let read = call.read.ok_or_else(|| invalid("read is required"))?;
    let conversation = parse_id(&read.conversation_id, "conversation_id", ConversationId)?;
    let moved = messaging
        .read_up_to(user, conversation, read.seq)
        .await
        .map_err(domain_error)?;
    Ok(ReadUpToResponse { moved })
}

/// A request frame: its `Response`, or a status whose error carries its `request_id`.
pub(crate) async fn request<S: Store, C: Clock, N: Notifier>(
    messaging: &Messaging<S, C, N>,
    call: RequestCall,
) -> Result<v1::Response, Status> {
    let request = call.request.ok_or_else(|| invalid("request is required"))?;
    let request_id = request.request_id;
    let answered = async {
        let user = parse_id(&call.user_id, "user_id", UserId)?;
        let body = request
            .body
            .ok_or_else(|| invalid("the request has no body"))?;
        answer(messaging, user, body).await
    }
    .await;
    match answered {
        Ok(body) => Ok(v1::Response {
            request_id,
            body: Some(body),
        }),
        Err(status) => {
            let mut error = error_from_status(&status);
            error.request_id = Some(request_id);
            Err(error_status(status.code(), error))
        }
    }
}

/// What the domain answers to one request.
async fn answer<S: Store, C: Clock, N: Notifier>(
    messaging: &Messaging<S, C, N>,
    user: UserId,
    body: request::Body,
) -> Result<response::Body, Status> {
    Ok(match body {
        request::Body::ListConversations(list) => {
            let since = list.changed_since.map(from_micros).transpose()?;
            let summaries = messaging
                .list_conversations(user, since)
                .await
                .map_err(domain_error)?;
            response::Body::Conversations(ConversationList {
                conversations: summaries.into_iter().map(summary).collect(),
            })
        }
        request::Body::FetchHistory(fetch) => {
            let conversation = parse_id(&fetch.conversation_id, "conversation_id", ConversationId)?;
            let limit = match fetch.limit {
                0 => MAX_HISTORY_PAGE,
                limit => limit,
            };
            let page = messaging
                .history(user, conversation, fetch.before_seq, limit)
                .await
                .map_err(domain_error)?;
            response::Body::History(HistoryPage {
                entries: page.entries.into_iter().map(deliver).collect(),
                has_more: page.has_more,
            })
        }
        request::Body::OpenDirect(open) => {
            let peer = parse_id(&open.peer_user_id, "peer_user_id", UserId)?;
            let conversation = messaging
                .open_direct(user, peer)
                .await
                .map_err(domain_error)?;
            response::Body::DirectOpened(DirectOpened {
                conversation_id: id_bytes(conversation.0),
            })
        }
        request::Body::ContactRequest(contact) => {
            let state = messaging
                .contact_request(user, other(&contact.user_id)?)
                .await
                .map_err(domain_error)?;
            response::Body::Contact(Contact {
                user_id: contact.user_id,
                state: contact_state(Some(state)).into(),
            })
        }
        request::Body::ContactAccept(contact) => done(
            messaging
                .contact_accept(user, other(&contact.user_id)?)
                .await,
        )?,
        request::Body::ContactDecline(contact) => done(
            messaging
                .contact_decline(user, other(&contact.user_id)?)
                .await,
        )?,
        request::Body::ContactBlock(contact) => done(
            messaging
                .contact_block(user, other(&contact.user_id)?)
                .await,
        )?,
        request::Body::ContactUnblock(contact) => done(
            messaging
                .contact_unblock(user, other(&contact.user_id)?)
                .await,
        )?,
        request::Body::ListContacts(ListContacts {}) => {
            let contacts = messaging.list_contacts(user).await.map_err(domain_error)?;
            response::Body::Contacts(ContactList {
                contacts: contacts
                    .into_iter()
                    .map(|contact| Contact {
                        user_id: id_bytes(contact.user_id.0),
                        state: contact_state(Some(contact.state)).into(),
                    })
                    .collect(),
            })
        }
        request::Body::CreateInvite(create) => {
            let expires_in = TimeDelta::seconds(i64::from(create.expires_in_s));
            let invite = messaging
                .create_invite(user, expires_in, create.max_uses)
                .await
                .map_err(domain_error)?;
            response::Body::Invite(Invite {
                code: invite.code,
                expires_at: micros(invite.expires_at),
                max_uses: invite.max_uses,
                uses: invite.uses,
            })
        }
        request::Body::RedeemInvite(redeem) => {
            let creator = messaging
                .redeem_invite(user, &redeem.code)
                .await
                .map_err(domain_error)?;
            response::Body::InviteRedeemed(InviteRedeemed {
                user_id: id_bytes(creator.0),
            })
        }
        request::Body::RevokeInvite(revoke) => {
            done(messaging.revoke_invite(user, &revoke.code).await)?
        }
        request::Body::CreateGroup(create) => {
            let members = user_ids(&create.member_ids, "member_ids")?;
            let conversation = messaging
                .create_group(user, &create.title, &members)
                .await
                .map_err(domain_error)?;
            response::Body::GroupCreated(GroupCreated {
                conversation_id: id_bytes(conversation.0),
            })
        }
        request::Body::AddMembers(add) => {
            let group = group(&add.conversation_id)?;
            let users = user_ids(&add.user_ids, "user_ids")?;
            done(messaging.add_members(user, group, &users).await)?
        }
        request::Body::RemoveMember(remove) => {
            let group = group(&remove.conversation_id)?;
            done(
                messaging
                    .remove_member(user, group, other(&remove.user_id)?)
                    .await,
            )?
        }
        request::Body::LeaveGroup(leave) => done(
            messaging
                .leave_group(user, group(&leave.conversation_id)?)
                .await,
        )?,
        request::Body::RenameGroup(rename) => {
            let group = group(&rename.conversation_id)?;
            done(messaging.rename_group(user, group, &rename.title).await)?
        }
        request::Body::SetRole(set) => {
            let group = group(&set.conversation_id)?;
            let role = match set.role() {
                v1::Role::Admin => Role::Admin,
                v1::Role::Member => Role::Member,
                v1::Role::Unspecified => return Err(invalid("role must be admin or member")),
            };
            done(
                messaging
                    .set_role(user, group, other(&set.user_id)?, role)
                    .await,
            )?
        }
        // Presence lands with the presence domain; until then these are refused.
        request::Body::GetPresence(_) | request::Body::SetPresenceSharing(_) => {
            return Err(invalid("not supported yet"));
        }
    })
}

/// The group a request names.
fn group(conversation_id: &[u8]) -> Result<ConversationId, Status> {
    parse_id(conversation_id, "conversation_id", ConversationId)
}

/// A list of user ids; any malformed one is `INVALID_ARGUMENT` naming `field`.
fn user_ids(ids: &[Vec<u8>], field: &str) -> Result<Vec<UserId>, Status> {
    ids.iter().map(|id| parse_id(id, field, UserId)).collect()
}

/// The other user a contact request names.
fn other(user_id: &[u8]) -> Result<UserId, Status> {
    parse_id(user_id, "user_id", UserId)
}

/// The answer to a request that returns nothing but success.
fn done(result: DomainResult<()>) -> Result<response::Body, Status> {
    result.map_err(domain_error)?;
    Ok(response::Body::Done(Done {}))
}

fn summary(summary: ConversationSummary) -> v1::ConversationSummary {
    let receipts = match summary.receipts {
        Receipts::Direct {
            peer_delivered_seq,
            peer_read_seq,
        } => conversation_summary::Receipts::Direct(DirectReceipts {
            peer_delivered_seq,
            peer_read_seq,
        }),
        Receipts::Group {
            all_delivered_seq,
            all_read_seq,
        } => conversation_summary::Receipts::Group(GroupReceipts {
            all_delivered_seq,
            all_read_seq,
        }),
    };
    let kind = match summary.kind {
        ConversationKind::Direct => v1::ConversationKind::Direct,
        ConversationKind::Group => v1::ConversationKind::Group,
    };
    v1::ConversationSummary {
        conversation_id: id_bytes(summary.conversation_id.0),
        kind: kind.into(),
        title: summary.title,
        last_seq: summary.last_seq,
        read_seq: summary.read_seq,
        unread: summary.unread,
        receipts: Some(receipts),
        updated_at: micros(summary.updated_at),
        members: users(&summary.members),
        admins: users(&summary.admins),
    }
}

fn users(users: &[UserId]) -> Vec<Vec<u8>> {
    users.iter().map(|user| id_bytes(user.0)).collect()
}

#[cfg(test)]
mod tests;
