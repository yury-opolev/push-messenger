//! Call history (voice calls spec §9): message-svc keeps it, and auth-svc serves it over
//! HTTP (`GET /v1/calls`, `DELETE /v1/calls/{call_id}`). auth-svc calls message-svc's
//! internal `Calls` API with the same internal token and channel as `Directory`.
//!
//! Cursors are opaque to clients: base64url (no padding) of `<ended_at µs>:<call_id>`.

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use push_proto::internal::v1 as internal;
use push_proto::internal::v1::calls_client::CallsClient;
use push_proto::v1::{CallDirection, CallEndReason};
use push_proto::{id, id_bytes};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::directory::{MessageSvc, message_svc};
use crate::error::AuthError;
use crate::templates::rfc3339;

/// A position in a user's history: calls are ordered by `(ended_at, call_id)`, newest
/// first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallCursor {
    /// Microseconds since the Unix epoch.
    pub ended_at: i64,
    pub call_id: Uuid,
}

impl CallCursor {
    /// The opaque form clients see.
    pub fn encode(&self) -> String {
        URL_SAFE_NO_PAD.encode(format!("{}:{}", self.ended_at, self.call_id))
    }

    /// A cursor as [`Self::encode`] writes it (one spelling only); `None` for anything
    /// else.
    pub fn decode(text: &str) -> Option<Self> {
        if text.is_empty() || text.len() > 128 {
            return None;
        }
        let bytes = URL_SAFE_NO_PAD.decode(text).ok()?;
        let (ended_at, call_id) = std::str::from_utf8(&bytes).ok()?.split_once(':')?;
        let ended: i64 = ended_at.parse().ok()?;
        let call: Uuid = call_id.parse().ok()?;
        let canonical = ended >= 0 && ended.to_string() == ended_at && call.to_string() == call_id;
        canonical.then_some(Self {
            ended_at: ended,
            call_id: call,
        })
    }

    fn to_proto(self) -> internal::CallHistoryCursor {
        internal::CallHistoryCursor {
            ended_at: self.ended_at,
            call_id: id_bytes(self.call_id),
        }
    }
}

/// One call in a user's history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallEntry {
    pub call_id: Uuid,
    pub peer_user_id: Uuid,
    /// `in` or `out`.
    pub direction: &'static str,
    pub started_at: DateTime<Utc>,
    pub answered_at: Option<DateTime<Utc>>,
    pub ended_at: DateTime<Utc>,
    pub duration_s: u32,
    /// As `CallEnded.reason`: completed, missed, declined, cancelled, busy, failed or
    /// unreachable.
    pub outcome: &'static str,
    pub cursor: CallCursor,
}

impl CallEntry {
    /// The call as the API answers it.
    pub(crate) fn to_json(&self) -> Value {
        let mut answer = json!({
            "call_id": self.call_id,
            "peer_user_id": self.peer_user_id,
            "direction": self.direction,
            "started_at": rfc3339(self.started_at),
            "ended_at": rfc3339(self.ended_at),
            "duration_s": self.duration_s,
            "outcome": self.outcome,
            "cursor": self.cursor.encode(),
        });
        if let (Some(answered_at), Value::Object(fields)) = (self.answered_at, &mut answer) {
            fields.insert(
                "answered_at".to_owned(),
                Value::String(rfc3339(answered_at)),
            );
        }
        answer
    }
}

/// message-svc's call history.
#[async_trait]
pub trait CallLog: Send + Sync + std::fmt::Debug {
    /// `user`'s calls they did not hide, newest first: strictly before `before` and after
    /// `since` when given, at most `limit` (1 to 100).
    async fn history(
        &self,
        user: Uuid,
        before: Option<CallCursor>,
        since: Option<CallCursor>,
        limit: u32,
    ) -> Result<Vec<CallEntry>, AuthError>;

    /// Hides `call` from `user`'s own history; succeeds whether or not there was such a
    /// call.
    async fn hide(&self, user: Uuid, call: Uuid) -> Result<(), AuthError>;
}

/// message-svc's `Calls` API over gRPC.
#[derive(Debug, Clone)]
pub struct GrpcCalls {
    client: CallsClient<MessageSvc>,
}

impl GrpcCalls {
    /// A client for message-svc's Calls API at `url` presenting `token`, auth-svc's
    /// internal token (the Directory's). It connects on first use.
    pub fn new(url: &str, token: &str) -> Result<Self, String> {
        Ok(Self::over(message_svc(url, token)?))
    }

    /// A client over an existing channel to message-svc.
    pub(crate) fn over(channel: MessageSvc) -> Self {
        Self {
            client: CallsClient::new(channel),
        }
    }
}

#[async_trait]
impl CallLog for GrpcCalls {
    async fn history(
        &self,
        user: Uuid,
        before: Option<CallCursor>,
        since: Option<CallCursor>,
        limit: u32,
    ) -> Result<Vec<CallEntry>, AuthError> {
        let request = internal::CallHistoryRequest {
            user_id: id_bytes(user),
            before: before.map(CallCursor::to_proto),
            since: since.map(CallCursor::to_proto),
            limit,
        };
        let response = self
            .client
            .clone()
            .history(request)
            .await
            .map_err(|status| unavailable("History", status.code()))?;
        response.into_inner().calls.iter().map(entry).collect()
    }

    async fn hide(&self, user: Uuid, call: Uuid) -> Result<(), AuthError> {
        self.client
            .clone()
            .hide(internal::HideCallRequest {
                user_id: id_bytes(user),
                call_id: id_bytes(call),
            })
            .await
            .map_err(|status| unavailable("Hide", status.code()))?;
        Ok(())
    }
}

/// Without message-svc: every call is `Unavailable`. For an auth-svc started without
/// `PUSH_MESSAGE_SVC_URL` or `PUSH_DIRECTORY_TOKEN`.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoCalls;

#[async_trait]
impl CallLog for NoCalls {
    async fn history(
        &self,
        _: Uuid,
        _: Option<CallCursor>,
        _: Option<CallCursor>,
        _: u32,
    ) -> Result<Vec<CallEntry>, AuthError> {
        Err(AuthError::Unavailable)
    }

    async fn hide(&self, _: Uuid, _: Uuid) -> Result<(), AuthError> {
        Err(AuthError::Unavailable)
    }
}

/// auth-svc checks every input before it asks, so any refusal of message-svc's is
/// `Unavailable` (logged with its code only).
fn unavailable(rpc: &'static str, code: tonic::Code) -> AuthError {
    tracing::warn!(?code, rpc, "message-svc's Calls API did not answer");
    AuthError::Unavailable
}

/// An answer of message-svc's that breaks the contract, logged; the request is
/// `Unavailable`.
fn malformed(what: &str) -> AuthError {
    tracing::warn!(
        what,
        "message-svc's Calls API answered with a malformed value"
    );
    AuthError::Unavailable
}

fn entry(entry: &internal::CallHistoryEntry) -> Result<CallEntry, AuthError> {
    let time =
        |micros: i64| DateTime::from_timestamp_micros(micros).ok_or_else(|| malformed("a time"));
    let call_id = id(&entry.call_id).map_err(|_| malformed("a call id"))?;
    let direction = match entry.direction() {
        CallDirection::In => "in",
        CallDirection::Out => "out",
        CallDirection::Unspecified => return Err(malformed("a direction")),
    };
    let outcome = match entry.outcome() {
        CallEndReason::Completed => "completed",
        CallEndReason::Missed => "missed",
        CallEndReason::Declined => "declined",
        CallEndReason::Cancelled => "cancelled",
        CallEndReason::Busy => "busy",
        CallEndReason::Failed => "failed",
        CallEndReason::Unreachable => "unreachable",
        CallEndReason::Unspecified => return Err(malformed("an outcome")),
    };
    Ok(CallEntry {
        call_id,
        peer_user_id: id(&entry.peer_user_id).map_err(|_| malformed("a user id"))?,
        direction,
        started_at: time(entry.started_at)?,
        answered_at: entry.answered_at.map(time).transpose()?,
        ended_at: time(entry.ended_at)?,
        duration_s: entry.duration_s,
        outcome,
        cursor: CallCursor {
            ended_at: entry.ended_at,
            call_id,
        },
    })
}

#[cfg(test)]
mod tests {
    use push_proto::internal::v1::calls_server::{self, CallsServer};
    use tokio::net::TcpListener;
    use tokio_stream::wrappers::TcpListenerStream;
    use tonic::{Request, Response, Status};

    use super::*;

    const TOKEN: &str = "directory-token-for-the-calls-tests";

    #[test]
    fn cursors_round_trip_and_have_one_spelling() {
        let cursor = CallCursor {
            ended_at: 1_790_000_300_000_000,
            call_id: Uuid::now_v7(),
        };
        let text = cursor.encode();
        assert!(
            text.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        );
        assert_eq!(CallCursor::decode(&text), Some(cursor));
        let id = cursor.call_id;
        for bad in [
            String::new(),
            "not a cursor".to_owned(),
            format!("{text}="),
            URL_SAFE_NO_PAD.encode(format!("01:{id}")),
            URL_SAFE_NO_PAD.encode(format!("-1:{id}")),
            URL_SAFE_NO_PAD.encode(format!("+1:{id}")),
            URL_SAFE_NO_PAD.encode(format!("1:{}", id.to_string().to_uppercase())),
            URL_SAFE_NO_PAD.encode(format!("1:{}", id.simple())),
            URL_SAFE_NO_PAD.encode(format!("1:{id}:2")),
            URL_SAFE_NO_PAD.encode(format!("1{id}")),
            "a".repeat(129),
        ] {
            assert_eq!(CallCursor::decode(&bad), None, "{bad}");
        }
    }

    /// A message-svc with one call per user, checking the token.
    struct OneCall;

    fn authorised<T>(request: &Request<T>) -> Result<(), Status> {
        let presented = request.metadata().get("authorization").cloned();
        if presented.as_ref().and_then(|value| value.to_str().ok())
            == Some(format!("Bearer {TOKEN}").as_str())
        {
            Ok(())
        } else {
            Err(Status::unauthenticated("wrong token"))
        }
    }

    #[tonic::async_trait]
    impl calls_server::Calls for OneCall {
        async fn history(
            &self,
            request: Request<internal::CallHistoryRequest>,
        ) -> Result<Response<internal::CallHistoryPage>, Status> {
            authorised(&request)?;
            let request = request.into_inner();
            if request.limit == 7 {
                return Ok(Response::new(internal::CallHistoryPage {
                    calls: vec![internal::CallHistoryEntry::default()],
                }));
            }
            let before = request.before.unwrap_or_default();
            Ok(Response::new(internal::CallHistoryPage {
                calls: vec![internal::CallHistoryEntry {
                    call_id: before.call_id,
                    peer_user_id: request.user_id,
                    direction: CallDirection::In.into(),
                    started_at: before.ended_at - 1_000_000,
                    answered_at: None,
                    ended_at: before.ended_at,
                    duration_s: 0,
                    outcome: CallEndReason::Missed.into(),
                }],
            }))
        }

        async fn hide(
            &self,
            request: Request<internal::HideCallRequest>,
        ) -> Result<Response<internal::HideCallResponse>, Status> {
            authorised(&request)?;
            Ok(Response::new(internal::HideCallResponse {}))
        }
    }

    #[tokio::test]
    async fn the_calls_api_is_asked_with_the_token_and_refusals_are_unavailable() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(CallsServer::new(OneCall))
                .serve_with_incoming(TcpListenerStream::new(listener)),
        );
        let (user, call) = (Uuid::now_v7(), Uuid::now_v7());
        let before = CallCursor {
            ended_at: 1_790_000_300_000_000,
            call_id: call,
        };
        let calls = GrpcCalls::new(&url, TOKEN).unwrap();
        let entries = calls.history(user, Some(before), None, 50).await.unwrap();
        assert_eq!(
            entries,
            vec![CallEntry {
                call_id: call,
                peer_user_id: user,
                direction: "in",
                started_at: DateTime::from_timestamp_micros(1_790_000_299_000_000).unwrap(),
                answered_at: None,
                ended_at: DateTime::from_timestamp_micros(1_790_000_300_000_000).unwrap(),
                duration_s: 0,
                outcome: "missed",
                cursor: before,
            }]
        );
        assert_eq!(calls.hide(user, call).await, Ok(()));
        // A malformed answer is unavailable, never shown.
        assert_eq!(
            calls.history(user, None, None, 7).await,
            Err(AuthError::Unavailable)
        );
        assert!(!format!("{calls:?}").contains(TOKEN));
        let wrong = GrpcCalls::new(&url, "another-token-of-the-right-length").unwrap();
        assert_eq!(
            wrong.history(user, None, None, 50).await,
            Err(AuthError::Unavailable)
        );
        assert_eq!(wrong.hide(user, call).await, Err(AuthError::Unavailable));
    }
}
