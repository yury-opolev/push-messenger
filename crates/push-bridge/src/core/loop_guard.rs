//! The loop guard (spec 2026-11-09 proactive agents §7.4): in each direct chat with
//! another agent the core counts the agent turns, the plain texts by either agent, since
//! the last message by a person there. The peer's texts count when they arrive (passed on
//! or not), the agent's own when they are sent (a `ContactReply` or `send_message`; the
//! agent's echoed messages are not counted again). At `loop_guard_turns` the chat is
//! paused: the peer's texts no longer reach the backend, the backend's replies there are
//! dropped and its `send_message` answers `paused`. A rich note in the chat (never a turn
//! for the other bridge) and one in the owner's chat say so, once. A person's message in
//! the chat, or the owner's `/loop resume`, starts the count again. The counts live in
//! memory: a restart resets them.

use std::collections::HashMap;

use push_sdk::{ConversationId, UserId};

use super::Core;
use crate::actions::{ActionCode, ActionResult};
use crate::messenger::Messenger;

/// Why the backend may not send into a paused chat.
pub(super) const PAUSED: &str =
    "the loop guard paused this chat; the owner can resume it with /loop resume";

/// The chats with other agents and their turns.
pub(super) struct LoopGuard {
    /// Agent turns without a person that pause a chat.
    limit: u32,
    chats: HashMap<ConversationId, Chat>,
}

/// A direct chat with another agent.
struct Chat {
    /// The other agent's handle (without the `@`).
    handle: String,
    /// Agent turns since the last person's message.
    turns: u32,
    paused: bool,
}

impl LoopGuard {
    pub(super) fn new(limit: u32) -> Self {
        Self {
            limit,
            chats: HashMap::new(),
        }
    }

    /// The paused chats' handles, sorted.
    fn paused(&self) -> Vec<String> {
        let mut handles: Vec<String> = self
            .chats
            .values()
            .filter(|chat| chat.paused)
            .map(|chat| chat.handle.clone())
            .collect();
        handles.sort();
        handles
    }
}

/// "@handle", or "another agent" for a handle not known.
fn named(handle: &str) -> String {
    if handle.is_empty() {
        "another agent".to_owned()
    } else {
        format!("@{handle}")
    }
}

impl<M: Messenger> Core<M> {
    /// Counts an agent's text in `conversation`, the direct chat with the agent `handle`;
    /// the turn that reaches the limit pauses the chat and says so.
    pub(super) fn agent_turn(&mut self, conversation: ConversationId, handle: &str) {
        let limit = self.loops.limit;
        let chat = self
            .loops
            .chats
            .entry(conversation)
            .or_insert_with(|| Chat {
                handle: String::new(),
                turns: 0,
                paused: false,
            });
        if !handle.is_empty() {
            handle.clone_into(&mut chat.handle);
        }
        chat.turns = chat.turns.saturating_add(1);
        if chat.paused || chat.turns < limit {
            return;
        }
        chat.paused = true;
        let peer = named(&chat.handle);
        tracing::info!(event = "loop_guard_paused", turns = limit);
        self.contact_reply(
            conversation,
            &format!(
                "Paused after {limit} turns between agents without a person. An owner can resume."
            ),
        );
        self.reply(&format!(
            "Paused the chat with {peer} after {limit} agent turns without a person. Send /loop resume to let it continue."
        ));
    }

    /// The loop guard paused `conversation`.
    pub(super) fn loop_paused(&self, conversation: ConversationId) -> bool {
        self.loops
            .chats
            .get(&conversation)
            .is_some_and(|chat| chat.paused)
    }

    /// `paused` for a chat the loop guard paused (see [`Core::guard_delivery`]).
    pub(super) fn loop_refusal(&self, conversation: ConversationId) -> Option<ActionResult> {
        self.loop_paused(conversation)
            .then(|| ActionResult::new(ActionCode::Paused, PAUSED))
    }

    /// A message by `sender` (not the agent) in `conversation`: a person's starts a chat
    /// with an agent's count again.
    pub(super) async fn loop_message(&mut self, conversation: ConversationId, sender: UserId) {
        if !self.loops.chats.contains_key(&conversation) {
            return;
        }
        let person =
            sender == self.owner || self.peer(sender).await.is_some_and(|peer| !peer.agent);
        if !person {
            return;
        }
        if let Some(chat) = self.loops.chats.get_mut(&conversation) {
            if chat.turns > 0 || chat.paused {
                tracing::info!(event = "loop_guard_reset", paused = chat.paused);
            }
            chat.turns = 0;
            chat.paused = false;
        }
    }

    /// `/loop`: the paused chats.
    pub(super) fn loop_list(&self) {
        let paused = self.loops.paused();
        if paused.is_empty() {
            return self.reply("No chat is paused.");
        }
        let lines: Vec<String> = paused.iter().map(|handle| named(handle)).collect();
        self.reply(&format!(
            "Paused chats with other agents:\n{}",
            lines.join("\n")
        ));
    }

    /// `/loop resume [@handle]`: that chat, or every paused one, goes on.
    pub(super) fn loop_resume(&mut self, handle: Option<&str>) {
        let mut resumed = Vec::new();
        for chat in self.loops.chats.values_mut() {
            let picked = handle.is_none_or(|handle| chat.handle.eq_ignore_ascii_case(handle));
            if picked && chat.paused {
                resumed.push(named(&chat.handle));
            }
            if picked {
                chat.turns = 0;
                chat.paused = false;
            }
        }
        resumed.sort();
        tracing::info!(event = "loop_guard_resumed", count = resumed.len());
        match (handle, resumed.as_slice()) {
            (Some(handle), []) => self.reply(&format!("No paused chat with @{handle}.")),
            (None, []) => self.reply("No chat is paused."),
            (_, [one]) => self.reply(&format!("Resumed the chat with {one}.")),
            (_, many) => self.reply(&format!("Resumed the chats with {}.", many.join(", "))),
        }
    }
}
