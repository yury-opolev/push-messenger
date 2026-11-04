//! The agent's requests as cards: posting them, settling them with the owner's answer,
//! and their deadlines.

use push_bridge_control::{CardState, PeekKind};
use push_sdk::OpenCard;
use tokio::time::Instant;

use super::{Core, OpenRequest};
use crate::backend::{
    Answer, BackendCommand, BackendRequest, PermissionMode, RequestKind, Responder,
};
use crate::cards::{self, CARD_TIMEOUT, CardKind, CardPlan};
use crate::messenger::Messenger;

impl<M: Messenger> Core<M> {
    pub(super) async fn open_request(&mut self, request: BackendRequest) {
        let BackendRequest { kind, responder } = request;
        let plan = match &kind {
            RequestKind::Permission { tool, preview } => {
                cards::permission_card(&self.name, tool, preview, &self.folder)
            }
            RequestKind::Question {
                question,
                options,
                multi_select,
                allow_free_text,
            } => cards::question_card(question, options, *multi_select, *allow_free_text),
            RequestKind::Plan { plan } => cards::plan_card(plan),
        };
        tracing::info!(event = "request_opened", kind = ?plan.kind);
        self.post_card(plan, Some(responder)).await;
    }

    /// Posts the card and its follow-ups and waits for the owner; `responder` is `None`
    /// for `/mode bypass`.
    pub(super) async fn post_card(&mut self, plan: CardPlan, responder: Option<Responder>) {
        // The reply so far gets its final state first, so the card follows what led to
        // it and what comes after the owner's answer goes into a new reply below it.
        self.seal_turn();
        let card = match &plan.card {
            Some(body) => match self.messenger.send_rich(self.conversation, body) {
                Ok(id) => {
                    let time = self.clock.now_ms();
                    self.control
                        .record(|ring| ring.sent(time, Some(id), body, PeekKind::Reply));
                    Some(id)
                }
                Err(error) => {
                    tracing::warn!(event = "card_refused", code = error.code());
                    if let Some(responder) = responder {
                        responder.answer(Answer::Decline);
                    }
                    self.reply(&format!(
                        "Could not show the card ({}); the request was declined.",
                        error.code()
                    ));
                    return;
                }
            },
            None => None,
        };
        for body in &plan.follow_ups {
            self.send_rich(body);
        }
        if let Some(id) = card {
            let timeout_ms = i64::try_from(CARD_TIMEOUT.as_millis()).unwrap_or(i64::MAX);
            self.cards.open(OpenCard {
                conversation: self.conversation,
                client_msg_id: id,
                message_id: None,
                owner: self.owner,
                offered: plan.offered.clone(),
                max_values: plan.max_values,
                deadline_ms: self.clock.now_ms().saturating_add(timeout_ms),
            });
        }
        self.open.push(OpenRequest {
            kind: plan.kind,
            responder,
            card,
            deadline: Instant::now() + CARD_TIMEOUT,
            opened: Instant::now(),
            waiting_label: plan.waiting_label,
            title: plan.title,
            spoken: plan.spoken,
            notified: false,
        });
        if let Some(request) = self.open.last() {
            self.request_opened(&request.kind);
        }
        // A call hears the oldest open card only: a voice answer goes to it.
        if let [request] = self.open.as_slice() {
            let spoken = request.spoken.clone();
            self.speak_card(&spoken);
        }
        self.update_activity(false).await;
    }

    /// The owner answered `request` (removed from the open list already).
    pub(super) async fn settle(&mut self, request: OpenRequest, answer: Answer) {
        if let Some(card) = request.card {
            self.cards.close(card);
        }
        self.peek_card(request.card, CardState::Answered);
        self.request_closed(&request.kind, "answered");
        match request.responder {
            Some(responder) => responder.answer(answer),
            None if answer == Answer::Permission(true) => {
                self.command_in_background(BackendCommand::SetPermissionMode(
                    PermissionMode::BypassPermissions,
                ));
            }
            None => self.reply("Approvals stay on."),
        }
        self.update_activity(false).await;
    }

    /// Requests past their deadline get their timeout answer and "No answer; skipped".
    pub(super) async fn expire_requests(&mut self) {
        let now = Instant::now();
        if self.open.iter().all(|request| request.deadline > now) {
            return;
        }
        let (expired, open): (Vec<_>, Vec<_>) = std::mem::take(&mut self.open)
            .into_iter()
            .partition(|request| request.deadline <= now);
        self.open = open;
        for request in expired {
            tracing::info!(event = "card_expired", kind = ?request.kind);
            if let Some(card) = request.card {
                self.cards.close(card);
            }
            self.peek_card(request.card, CardState::Expired);
            self.request_closed(&request.kind, "expired");
            let answer = cards::timeout_answer(&request.kind);
            if let (Some(responder), Some(answer)) = (request.responder, answer) {
                responder.answer(answer);
            }
            self.reply("No answer; skipped");
        }
        self.update_activity(false).await;
    }

    /// Closes the backend's open requests locally (the backend declined them already)
    /// and says why once.
    pub(super) fn close_backend_requests(&mut self, why: &str) {
        let (closed, open): (Vec<_>, Vec<_>) = std::mem::take(&mut self.open)
            .into_iter()
            .partition(|request| request.kind != CardKind::Bypass);
        self.open = open;
        if closed.is_empty() {
            return;
        }
        for request in closed {
            if let Some(card) = request.card {
                self.cards.close(card);
            }
            self.peek_card(request.card, CardState::Declined);
            self.request_closed(&request.kind, "declined");
        }
        self.reply(why);
    }

    /// Closes an open `/mode bypass` card without turning approvals off: it was for the
    /// session that just ended.
    pub(super) fn cancel_bypass_request(&mut self) {
        let (closed, open): (Vec<_>, Vec<_>) = std::mem::take(&mut self.open)
            .into_iter()
            .partition(|request| request.kind == CardKind::Bypass);
        self.open = open;
        if closed.is_empty() {
            return;
        }
        for request in closed {
            if let Some(card) = request.card {
                self.cards.close(card);
            }
            self.peek_card(request.card, CardState::Declined);
            self.request_closed(&request.kind, "declined");
        }
        self.reply("Turning approvals off was cancelled: the session changed.");
    }
}
