//! Who besides the owner reaches the backend (spec 2026-11-09 proactive agents §4, §7.2):
//! the owner's inbound policy, read at start-up and after each reconnect and changed live
//! by `AgentSettingsEvent`, and the profiles of those who write (an agent, and whose),
//! kept as the relationships are.

use push_sdk::{AgentSettings, InboundPolicy, SdkError, UserId};
use tokio::time::Instant;

use super::contacts::CONTACT_FRESH;
use super::{Core, Internal};
use crate::messenger::{Messenger, Peer};

impl<M: Messenger> Core<M> {
    /// Reads the agent's settings at start-up; without them (an older server) the
    /// policy stays `owner_only`.
    pub(super) async fn load_settings(&mut self) {
        let result = self.messenger.agent_settings().await;
        self.settings_read(result);
    }

    /// Reads the agent's settings again in the background (after a reconnect).
    pub(super) fn refresh_settings(&self) {
        let messenger = self.messenger.clone();
        let internal = self.internal.clone();
        tokio::spawn(async move {
            let result = messenger.agent_settings().await;
            let _ = internal.send(Internal::Settings { result });
        });
    }

    /// The agent's settings as read; a failure keeps the policy known so far.
    pub(super) fn settings_read(&mut self, result: Result<AgentSettings, SdkError>) {
        match result {
            Ok(settings) => self.settings_changed(settings),
            Err(error) => tracing::warn!(
                event = "agent_settings_failed",
                code = error.code(),
                inbound = self.inbound.as_str()
            ),
        }
    }

    /// The owner's settings, read or reported: the inbound policy applies at once.
    pub(super) fn settings_changed(&mut self, settings: AgentSettings) {
        if settings.inbound != self.inbound {
            tracing::info!(event = "inbound_policy", policy = settings.inbound.as_str());
        }
        self.inbound = settings.inbound;
    }

    /// Whether the inbound policy lets `peer`, an accepted contact, reach the backend.
    pub(super) fn inbound_allows(&self, peer: Option<&Peer>) -> bool {
        match self.inbound {
            InboundPolicy::OwnerOnly => false,
            InboundPolicy::OwnerAgents => {
                peer.is_some_and(|peer| peer.agent && peer.owner == Some(self.owner))
            }
            InboundPolicy::Contacts => true,
        }
    }

    /// `user`'s profile (`None` when the agent may not see it), kept for a while.
    pub(super) async fn peer(&mut self, user: UserId) -> Option<Peer> {
        let now = Instant::now();
        if let Some((peer, at)) = self.contacts.peers.get(&user) {
            if now.saturating_duration_since(*at) < CONTACT_FRESH {
                return peer.clone();
            }
        }
        let peer = match self.messenger.peers(vec![user]).await {
            Ok(peers) => peers.into_iter().find(|peer| peer.user_id == user),
            Err(error) => {
                tracing::warn!(event = "profile_failed", code = error.code());
                return self.known_peer(user).cloned();
            }
        };
        self.contacts.peers.insert(user, (peer.clone(), now));
        peer
    }

    /// `user`'s profile as last read, however old.
    pub(super) fn known_peer(&self, user: UserId) -> Option<&Peer> {
        self.contacts
            .peers
            .get(&user)
            .and_then(|(peer, _)| peer.as_ref())
    }

    /// `user` is an agent, as last read.
    pub(super) fn is_agent(&self, user: UserId) -> bool {
        self.known_peer(user).is_some_and(|peer| peer.agent)
    }
}
