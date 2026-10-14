//! Whose profiles a caller may see (spec §5.5). Only message-svc knows who shares a
//! contact or a conversation, so auth-svc asks its internal `Directory` API, presenting
//! auth-svc's own internal token. Each service keeps its own database.

use std::time::Duration;

use async_trait::async_trait;
use push_proto::internal::v1::VisibleUsersRequest;
use push_proto::internal::v1::directory_client::DirectoryClient;
use push_proto::{id, id_bytes};
use tonic::codegen::InterceptedService;
use tonic::metadata::{Ascii, MetadataValue};
use tonic::service::Interceptor;
use tonic::transport::{Channel, Endpoint};
use tonic::{Request, Status};
use uuid::Uuid;

use crate::error::AuthError;

/// How long connecting to message-svc may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// How long one call may take; a profile request waits for it.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Answers whose profiles a user may see.
#[async_trait]
pub trait Directory: Send + Sync + std::fmt::Debug {
    /// The users among `candidates` whose profiles `caller` may see. `Unavailable` when
    /// there is no answer: no profile is shown without one.
    async fn visible_users(
        &self,
        caller: Uuid,
        candidates: &[Uuid],
    ) -> Result<Vec<Uuid>, AuthError>;
}

/// message-svc's `Directory` API over gRPC.
#[derive(Debug, Clone)]
pub struct GrpcDirectory {
    client: DirectoryClient<InterceptedService<Channel, Bearer>>,
}

impl GrpcDirectory {
    /// A client for message-svc at `url` (`http://host:50051`) that presents `token`,
    /// auth-svc's internal token for this API. It connects on first use and reconnects by
    /// itself, so auth-svc starts without waiting for message-svc.
    pub fn new(url: &str, token: &str) -> Result<Self, String> {
        let mut bearer: MetadataValue<Ascii> = format!("Bearer {token}")
            .parse()
            .map_err(|_| "the Directory token cannot be sent as metadata".to_owned())?;
        // Never indexed by HTTP/2 or shown in header dumps.
        bearer.set_sensitive(true);
        let channel = Endpoint::from_shared(url.to_owned())
            .map_err(|_| "not a valid message-svc URL".to_owned())?
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(CALL_TIMEOUT)
            .connect_lazy();
        Ok(Self {
            client: DirectoryClient::with_interceptor(channel, Bearer(bearer)),
        })
    }
}

#[async_trait]
impl Directory for GrpcDirectory {
    async fn visible_users(
        &self,
        caller: Uuid,
        candidates: &[Uuid],
    ) -> Result<Vec<Uuid>, AuthError> {
        let request = VisibleUsersRequest {
            user_id: id_bytes(caller),
            user_ids: candidates.iter().map(|user| id_bytes(*user)).collect(),
        };
        let unavailable = |code| {
            tracing::warn!(?code, "message-svc's Directory API did not answer");
            AuthError::Unavailable
        };
        let response = self
            .client
            .clone()
            .visible_users(request)
            .await
            .map_err(|status| unavailable(status.code()))?;
        response
            .into_inner()
            .user_ids
            .iter()
            .map(|user| id(user).map_err(|_| unavailable(tonic::Code::Internal)))
            .collect()
    }
}

/// Without message-svc: every answer is `Unavailable`, so no profile is shown. For an
/// auth-svc started without `PUSH_MESSAGE_SVC_URL`.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoDirectory;

#[async_trait]
impl Directory for NoDirectory {
    async fn visible_users(
        &self,
        _caller: Uuid,
        _candidates: &[Uuid],
    ) -> Result<Vec<Uuid>, AuthError> {
        Err(AuthError::Unavailable)
    }
}

/// Adds auth-svc's internal token, as `authorization: Bearer <token>`, to every call.
#[derive(Clone)]
struct Bearer(MetadataValue<Ascii>);

impl std::fmt::Debug for Bearer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Bearer(<redacted>)")
    }
}

impl Interceptor for Bearer {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        request
            .metadata_mut()
            .insert("authorization", self.0.clone());
        Ok(request)
    }
}

#[cfg(test)]
mod tests {
    use push_proto::internal::v1::VisibleUsersResponse;
    use push_proto::internal::v1::directory_server::{self, DirectoryServer};
    use tokio::net::TcpListener;
    use tokio_stream::wrappers::TcpListenerStream;
    use tonic::Response;

    use super::*;

    const TOKEN: &str = "directory-token-for-the-auth-tests";

    /// A message-svc that lets everyone see the first candidate, and checks the token.
    struct FirstOnly;

    #[tonic::async_trait]
    impl directory_server::Directory for FirstOnly {
        async fn visible_users(
            &self,
            request: Request<VisibleUsersRequest>,
        ) -> Result<Response<VisibleUsersResponse>, Status> {
            let presented = request.metadata().get("authorization").cloned();
            if presented.as_ref().and_then(|value| value.to_str().ok())
                != Some(format!("Bearer {TOKEN}").as_str())
            {
                return Err(Status::unauthenticated("wrong token"));
            }
            let request = request.into_inner();
            Ok(Response::new(VisibleUsersResponse {
                user_ids: request.user_ids.into_iter().take(1).collect(),
            }))
        }
    }

    async fn message_svc() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(DirectoryServer::new(FirstOnly))
                .serve_with_incoming(TcpListenerStream::new(listener)),
        );
        url
    }

    #[tokio::test]
    async fn the_directory_is_asked_with_the_token_and_refusals_are_unavailable() {
        let url = message_svc().await;
        let (caller, first, second) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let directory = GrpcDirectory::new(&url, TOKEN).unwrap();
        assert_eq!(
            directory.visible_users(caller, &[first, second]).await,
            Ok(vec![first])
        );
        let wrong = GrpcDirectory::new(&url, "another-token-of-the-right-length").unwrap();
        assert_eq!(
            wrong.visible_users(caller, &[first]).await,
            Err(AuthError::Unavailable)
        );
        let unreachable = GrpcDirectory::new("http://127.0.0.1:1", TOKEN).unwrap();
        assert_eq!(
            unreachable.visible_users(caller, &[first]).await,
            Err(AuthError::Unavailable)
        );
        assert!(!format!("{directory:?}").contains(TOKEN));
        assert_eq!(
            NoDirectory.visible_users(caller, &[first]).await,
            Err(AuthError::Unavailable)
        );
    }
}
