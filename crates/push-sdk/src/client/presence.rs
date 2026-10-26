//! Live activity in a conversation and the presence of contacts
//! (docs/superpowers/specs/2026-10-26-activity-presence-design.md).

use push_domain::{ConversationId, MAX_PRESENCE_BATCH, UserId, validate_label};
use push_proto::v1::frame::Body;
use push_proto::v1::{self, GetPresence, SetPresenceSharing, request, response};

use super::Client;
use super::requests::unexpected;
use crate::SdkError;
use crate::connection::Command;
use crate::model::{ActivityState, ConnectionState, Presence};
use crate::store::bytes;

impl Client {
    /// Tells the other members of `conversation` what the user is doing there, with an
    /// optional short label ("running tests"; 1 to 60 characters, plain text, no line
    /// breaks; empty means none). Fire and forget: nothing answers it, and a refusal is
    /// only logged. Others show it for 30 s, so send it again every 10 to 20 s while it
    /// lasts, and [`ActivityState::Idle`] when it ends (idle carries no label: the label is
    /// ignored). Needs a live connection: fails at once with [`SdkError::Offline`] while
    /// connecting or offline (activity is stale by the time a connection comes up).
    pub async fn send_activity(
        &self,
        conversation: ConversationId,
        state: ActivityState,
        label: Option<&str>,
    ) -> Result<(), SdkError> {
        let label = match state {
            ActivityState::Idle => None,
            _ => validate_label(label).map_err(|error| SdkError::Invalid(error.to_string()))?,
        };
        match self.connection_state() {
            ConnectionState::Connected => {}
            ConnectionState::SignedOut => return Err(SdkError::NotSignedIn),
            ConnectionState::DeviceRemoved => return Err(SdkError::DeviceRemoved),
            ConnectionState::OpenedElsewhere => return Err(SdkError::OpenedElsewhere),
            _ => return Err(SdkError::Offline),
        }
        let frame = Body::Activity(v1::Activity {
            conversation_id: bytes(conversation.0),
            state: state.to_wire().into(),
            label,
        });
        self.inner
            .commands
            .send(Command::Frame(frame))
            .map_err(|_| SdkError::Closed)
    }

    /// The presence of `users` (1 to 100), one per distinct user in the order asked.
    /// Anyone whose presence the user may not see (not an accepted contact, blocked,
    /// sharing off on either side, no such user) is [`PresenceState::Unknown`]. Changes
    /// arrive as [`Event::PresenceChanged`] while connected.
    ///
    /// [`PresenceState::Unknown`]: crate::PresenceState::Unknown
    /// [`Event::PresenceChanged`]: crate::Event::PresenceChanged
    pub async fn get_presence(&self, users: &[UserId]) -> Result<Vec<Presence>, SdkError> {
        if users.is_empty() || users.len() > MAX_PRESENCE_BATCH {
            return Err(SdkError::Invalid(format!(
                "ask for the presence of 1 to {MAX_PRESENCE_BATCH} users"
            )));
        }
        let body = request::Body::GetPresence(GetPresence {
            user_ids: users.iter().map(|user| bytes(user.0)).collect(),
        });
        let response::Body::Presences(list) = self.request(body).await? else {
            return Err(unexpected());
        };
        Ok(list
            .presences
            .iter()
            .filter_map(Presence::from_wire)
            .collect())
    }

    /// Switches sharing the user's presence on or off; the setting now. While sharing is
    /// off, contacts see the user as [`PresenceState::Unknown`], and every other user
    /// reads as `Unknown` too (the opt-out is reciprocal). Nothing else is reported: an
    /// app that keeps presence should clear what it holds when it switches sharing off.
    ///
    /// [`PresenceState::Unknown`]: crate::PresenceState::Unknown
    pub async fn set_presence_sharing(&self, share: bool) -> Result<bool, SdkError> {
        self.presence_sharing_request(Some(share)).await
    }

    /// Whether the user shares their presence (on unless switched off).
    pub async fn presence_sharing(&self) -> Result<bool, SdkError> {
        self.presence_sharing_request(None).await
    }

    async fn presence_sharing_request(&self, share: Option<bool>) -> Result<bool, SdkError> {
        let body = request::Body::SetPresenceSharing(SetPresenceSharing { share });
        let response::Body::PresenceSharing(sharing) = self.request(body).await? else {
            return Err(unexpected());
        };
        Ok(sharing.share)
    }
}

#[cfg(test)]
mod tests {
    use push_proto::v1::{self, frame::Body, request, response};

    use super::*;
    use crate::client::tests::Fixture;
    use crate::model::PresenceState;
    use crate::testing::{Server, recv_until, send};

    /// Reads the next request of the app (not the engine's listings).
    async fn next_request(socket: &mut Server) -> v1::Request {
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

    async fn answer(socket: &mut Server, request_id: u64, body: response::Body) {
        let answer = v1::Response {
            request_id,
            body: Some(body),
        };
        send(socket, Body::Response(answer)).await;
    }

    #[tokio::test]
    async fn send_activity_sends_a_frame() {
        let mut f = Fixture::new().await;
        let conversation = ConversationId::new();
        let working = f
            .client
            .send_activity(conversation, ActivityState::Working, Some("running tests"))
            .await;
        assert_eq!(working, Err(SdkError::Offline));

        f.client.connect().unwrap();
        let mut socket = f.ready().await;
        f.client
            .wait_connected(std::time::Duration::from_secs(10))
            .await
            .unwrap();
        f.client
            .send_activity(conversation, ActivityState::Working, Some("running tests"))
            .await
            .unwrap();
        f.client
            .send_activity(conversation, ActivityState::Idle, Some("ignored"))
            .await
            .unwrap();
        let mut sent = Vec::new();
        while sent.len() < 2 {
            if let Body::Activity(activity) =
                recv_until(&mut socket, |body| matches!(body, Body::Activity(_))).await
            {
                sent.push(activity);
            }
        }
        assert_eq!(
            sent,
            [
                v1::Activity {
                    conversation_id: bytes(conversation.0),
                    state: v1::ActivityState::Working.into(),
                    label: Some("running tests".to_owned()),
                },
                v1::Activity {
                    conversation_id: bytes(conversation.0),
                    state: v1::ActivityState::Idle.into(),
                    label: None,
                },
            ]
        );
    }

    #[tokio::test]
    async fn send_activity_refuses_a_bad_label() {
        let f = Fixture::new().await;
        let conversation = ConversationId::new();
        let too_long = "a".repeat(push_domain::MAX_ACTIVITY_LABEL_CHARS + 1);
        for label in ["line\nbreak", "   ", too_long.as_str(), "bidi\u{202e}"] {
            let sent = f
                .client
                .send_activity(conversation, ActivityState::Typing, Some(label))
                .await;
            assert!(matches!(sent, Err(SdkError::Invalid(_))), "{label:?}");
        }
    }

    #[tokio::test]
    async fn get_presence_round_trips() {
        let mut f = Fixture::new().await;
        f.client.connect().unwrap();
        let mut socket = f.ready().await;
        let (online, offline, hidden) = (UserId::new(), UserId::new(), UserId::new());
        let client = f.client.clone();
        let asking =
            tokio::spawn(async move { client.get_presence(&[online, offline, hidden]).await });
        let request = next_request(&mut socket).await;
        assert_eq!(
            request.body,
            Some(request::Body::GetPresence(v1::GetPresence {
                user_ids: vec![bytes(online.0), bytes(offline.0), bytes(hidden.0)],
            }))
        );
        let wire = |user: UserId, state: v1::PresenceState, last_seen| v1::Presence {
            user_id: bytes(user.0),
            state: state.into(),
            last_seen,
        };
        let list = v1::PresenceList {
            presences: vec![
                wire(online, v1::PresenceState::Online, None),
                wire(
                    offline,
                    v1::PresenceState::Offline,
                    Some(1_700_000_040_000_000),
                ),
                wire(hidden, v1::PresenceState::Unknown, None),
            ],
        };
        answer(
            &mut socket,
            request.request_id,
            response::Body::Presences(list),
        )
        .await;
        assert_eq!(
            asking.await.unwrap().unwrap(),
            [
                Presence {
                    user_id: online,
                    state: PresenceState::Online,
                    last_seen_ms: None,
                },
                Presence {
                    user_id: offline,
                    state: PresenceState::Offline,
                    last_seen_ms: Some(1_700_000_040_000),
                },
                Presence {
                    user_id: hidden,
                    state: PresenceState::Unknown,
                    last_seen_ms: None,
                },
            ]
        );
    }

    #[tokio::test]
    async fn get_presence_refuses_more_than_100_users() {
        let f = Fixture::new().await;
        let many: Vec<UserId> = (0..=MAX_PRESENCE_BATCH).map(|_| UserId::new()).collect();
        let invalid =
            |result: Result<Vec<Presence>, SdkError>| matches!(result, Err(SdkError::Invalid(_)));
        assert!(invalid(f.client.get_presence(&many).await));
        assert!(invalid(f.client.get_presence(&[]).await));
        // Within the limit it gets as far as the connection.
        let within = f.client.get_presence(&many[..MAX_PRESENCE_BATCH]).await;
        assert_eq!(within, Err(SdkError::Offline));
    }

    #[tokio::test]
    async fn set_presence_sharing_round_trips() {
        let mut f = Fixture::new().await;
        f.client.connect().unwrap();
        let mut socket = f.ready().await;

        let client = f.client.clone();
        let switching = tokio::spawn(async move { client.set_presence_sharing(false).await });
        let request = next_request(&mut socket).await;
        assert_eq!(
            request.body,
            Some(request::Body::SetPresenceSharing(v1::SetPresenceSharing {
                share: Some(false),
            }))
        );
        let off = response::Body::PresenceSharing(v1::PresenceSharing { share: false });
        answer(&mut socket, request.request_id, off).await;
        assert_eq!(switching.await.unwrap(), Ok(false));

        let client = f.client.clone();
        let reading = tokio::spawn(async move { client.presence_sharing().await });
        let request = next_request(&mut socket).await;
        assert_eq!(
            request.body,
            Some(request::Body::SetPresenceSharing(v1::SetPresenceSharing {
                share: None
            }))
        );
        let on = response::Body::PresenceSharing(v1::PresenceSharing { share: true });
        answer(&mut socket, request.request_id, on).await;
        assert_eq!(reading.await.unwrap(), Ok(true));
    }
}
