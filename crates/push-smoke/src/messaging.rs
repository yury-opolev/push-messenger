//! The smoke test's calls to message-svc's `Messaging` API, made as the gateway makes them
//! for an identified device: the request frames, Edit, Delete and ReadUpTo. Until the
//! gateway carries these frames (Plan 8), the smoke test calls message-svc straight.

use anyhow::{Context, anyhow, bail};
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::internal::v1::{DeleteRequest, EditRequest, ReadUpToRequest, RequestCall};
use push_proto::v1::{self, Delete, Edit, ReadUpTo, SendAck, delete, edit, request, response};
use push_proto::{error_from_status, id_bytes};
use tonic::Status;
use tonic::transport::Channel;
use uuid::Uuid;

/// message-svc as the gateway calls it.
pub(crate) struct Messaging {
    client: MessagingClient<Channel>,
    next_request: u64,
}

impl Messaging {
    pub fn new(channel: Channel) -> Self {
        Self {
            client: MessagingClient::new(channel),
            next_request: 0,
        }
    }

    /// Sends a request frame as `user`; its answer must carry the request's id.
    pub async fn request(
        &mut self,
        user: Uuid,
        body: request::Body,
        what: &str,
    ) -> anyhow::Result<response::Body> {
        self.next_request += 1;
        let call = RequestCall {
            user_id: id_bytes(user),
            request: Some(v1::Request {
                request_id: self.next_request,
                body: Some(body),
            }),
        };
        let response = self
            .client
            .request(call)
            .await
            .map_err(|status| refused(what, &status))?
            .into_inner();
        if response.request_id != self.next_request {
            bail!("{what}: the answer carries another request's id");
        }
        response
            .body
            .with_context(|| format!("{what}: an empty answer"))
    }

    /// Replaces the text of `user`'s message `target` in `conversation`.
    pub async fn edit(
        &mut self,
        user: Uuid,
        device: Uuid,
        conversation: &[u8],
        target: &[u8],
        body: &str,
    ) -> anyhow::Result<SendAck> {
        let request = EditRequest {
            user_id: id_bytes(user),
            device_id: id_bytes(device),
            edit: Some(Edit {
                client_msg_id: id_bytes(Uuid::now_v7()),
                conversation_id: conversation.to_vec(),
                target: Some(edit::Target::TargetMessageId(target.to_vec())),
                body: body.as_bytes().to_vec(),
                client_ts: None,
            }),
        };
        let ack = self.client.edit(request).await;
        Ok(ack.map_err(|status| refused("edit", &status))?.into_inner())
    }

    /// Deletes `user`'s message that was sent with `client_msg_id`.
    pub async fn delete(
        &mut self,
        user: Uuid,
        device: Uuid,
        conversation: &[u8],
        client_msg_id: &[u8],
    ) -> anyhow::Result<SendAck> {
        let request = DeleteRequest {
            user_id: id_bytes(user),
            device_id: id_bytes(device),
            delete: Some(Delete {
                client_msg_id: id_bytes(Uuid::now_v7()),
                conversation_id: conversation.to_vec(),
                target: Some(delete::Target::TargetClientMsgId(client_msg_id.to_vec())),
            }),
        };
        let ack = self.client.delete(request).await;
        Ok(ack
            .map_err(|status| refused("delete", &status))?
            .into_inner())
    }

    /// Moves `user`'s read position; whether it moved.
    pub async fn read_up_to(
        &mut self,
        user: Uuid,
        conversation: &[u8],
        seq: i64,
    ) -> anyhow::Result<bool> {
        let request = ReadUpToRequest {
            user_id: id_bytes(user),
            read: Some(ReadUpTo {
                conversation_id: conversation.to_vec(),
                seq,
            }),
        };
        let moved = self.client.read_up_to(request).await;
        Ok(moved
            .map_err(|status| refused("read", &status))?
            .into_inner()
            .moved)
    }
}

/// A refusal, described by the client error message-svc sent: fixed texts, never a body.
fn refused(what: &str, status: &Status) -> anyhow::Error {
    let error = error_from_status(status);
    anyhow!("{what}: {:?} ({})", error.code(), error.message)
}
