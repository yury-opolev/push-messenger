//! The smoke test's calls to message-svc's `Messaging` API, made as the gateway makes them
//! for an identified device. Until the gateway carries these frames (Plan 8), the smoke
//! test calls message-svc straight.

use anyhow::{Context, anyhow, bail};
use push_proto::internal::v1::RequestCall;
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::v1::{self, request, response};
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
}

/// A refusal, described by the client error message-svc sent: fixed texts, never a body.
fn refused(what: &str, status: &Status) -> anyhow::Error {
    let error = error_from_status(status);
    anyhow!("{what}: {:?} ({})", error.code(), error.message)
}
