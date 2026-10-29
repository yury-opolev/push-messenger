//! The cards an app sent and still waits on, and the checks on their answers.
//!
//! The server checks an answer's form and that its values are options of the card, but
//! not who the card was meant for: anyone in the conversation may answer it. The card's
//! sender therefore checks every answer itself: it must come from the card's owner, name
//! the card, hold only values the card offers, and arrive before the card's deadline.

use std::collections::HashMap;

use push_domain::{ClientMsgId, ConversationId, MessageId, UserId};
use serde::Deserialize;
use uuid::Uuid;

use crate::model::{Message, MessageKind};

/// A card this client sent and still waits on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenCard {
    pub conversation: ConversationId,
    pub client_msg_id: ClientMsgId,
    /// The server's id, once the card was stored (`Event::MessageSent`).
    pub message_id: Option<MessageId>,
    /// The only user whose answer counts.
    pub owner: UserId,
    /// The values the card offers (`RichBody::offered`).
    pub offered: Vec<String>,
    /// At most this many values in one answer (1 for buttons/single choice).
    pub max_values: usize,
    /// Unix ms after which answers no longer count.
    pub deadline_ms: i64,
}

/// Why an answer does not count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CardRefusal {
    /// The message is not an ANSWER with a `card` and `values`.
    NotACardAnswer,
    /// It answers no card in the book, or one whose server id is not known yet.
    UnknownCard,
    /// It comes from someone other than the card's owner.
    NotOwner,
    /// Its values are not 1 to `max_values` distinct options of the card.
    NotOffered,
    /// It arrived after the card's deadline.
    Late,
}

/// The result of [`CardBook::check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CardCheck {
    /// The owner's answer: the card is closed and removed from the book.
    Accepted { card: OpenCard, values: Vec<String> },
    /// The answer does not count; the card stays open.
    Refused(CardRefusal),
}

/// The open cards of one client, keyed by their `ClientMsgId`.
#[derive(Debug, Default)]
pub struct CardBook {
    cards: HashMap<ClientMsgId, OpenCard>,
}

/// The parts of an ANSWER body the checks need; the server has checked its form.
#[derive(Deserialize)]
struct Answer {
    card: Uuid,
    values: Vec<String>,
}

impl CardBook {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a card, replacing one with the same `client_msg_id`.
    pub fn open(&mut self, card: OpenCard) {
        self.cards.insert(card.client_msg_id, card);
    }

    /// Records the server id from `Event::MessageSent` for a card in the book (no-op otherwise).
    pub fn sent(&mut self, message: &Message) {
        let Some(id) = message.message_id else {
            return;
        };
        if let Some(card) = self.cards.get_mut(&message.client_msg_id) {
            if card.conversation == message.conversation_id {
                card.message_id = Some(id);
            }
        }
    }

    /// Checks a delivered message that may answer one of the cards. Accepted only when it is an
    /// ANSWER in the card's conversation, from the owner, naming the card's server id, with
    /// 1..=max_values distinct values all offered, received at or before the deadline (`now_ms`).
    /// A refusal leaves the card open.
    pub fn check(&mut self, message: &Message, now_ms: i64) -> CardCheck {
        let answer = match read_answer(message) {
            Some(answer) => answer,
            None => return CardCheck::Refused(CardRefusal::NotACardAnswer),
        };
        let target = Some(MessageId(answer.card));
        let Some(card) = self
            .cards
            .values()
            .find(|card| card.message_id == target && card.conversation == message.conversation_id)
        else {
            return CardCheck::Refused(CardRefusal::UnknownCard);
        };
        if message.sender_id != card.owner {
            return CardCheck::Refused(CardRefusal::NotOwner);
        }
        let values = &answer.values;
        let distinct = values
            .iter()
            .enumerate()
            .all(|(i, value)| !values[..i].contains(value));
        if values.is_empty()
            || values.len() > card.max_values
            || !distinct
            || !values.iter().all(|value| card.offered.contains(value))
        {
            return CardCheck::Refused(CardRefusal::NotOffered);
        }
        if now_ms > card.deadline_ms {
            return CardCheck::Refused(CardRefusal::Late);
        }
        let id = card.client_msg_id;
        match self.cards.remove(&id) {
            Some(card) => CardCheck::Accepted {
                card,
                values: answer.values,
            },
            None => CardCheck::Refused(CardRefusal::UnknownCard),
        }
    }

    /// Removes a card (answered another way, declined, or refused by the server).
    pub fn close(&mut self, client_msg_id: ClientMsgId) -> Option<OpenCard> {
        self.cards.remove(&client_msg_id)
    }

    /// Removes and returns every card past its deadline, the earliest first.
    pub fn expire(&mut self, now_ms: i64) -> Vec<OpenCard> {
        let late: Vec<ClientMsgId> = self
            .cards
            .values()
            .filter(|card| now_ms > card.deadline_ms)
            .map(|card| card.client_msg_id)
            .collect();
        let mut expired: Vec<OpenCard> =
            late.iter().filter_map(|id| self.cards.remove(id)).collect();
        expired.sort_by_key(|card| (card.deadline_ms, card.client_msg_id));
        expired
    }

    pub fn get(&self, client_msg_id: ClientMsgId) -> Option<&OpenCard> {
        self.cards.get(&client_msg_id)
    }

    pub fn len(&self) -> usize {
        self.cards.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cards.is_empty()
    }
}

fn read_answer(message: &Message) -> Option<Answer> {
    if message.kind != MessageKind::Answer || message.deleted {
        return None;
    }
    serde_json::from_str(message.body.as_deref()?).ok()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::model::MessageStatus;

    const DEADLINE: i64 = 1_000;

    fn message(
        conversation: ConversationId,
        sender: UserId,
        kind: MessageKind,
        body: &str,
    ) -> Message {
        Message {
            conversation_id: conversation,
            client_msg_id: ClientMsgId::new(),
            message_id: Some(MessageId::new()),
            seq: Some(2),
            sender_id: sender,
            kind,
            body: Some(body.to_owned()),
            client_ts: None,
            server_ts: Some(500),
            edited_at: None,
            deleted: false,
            status: MessageStatus::Received,
            answer_seq: None,
            card_seq: Some(1),
        }
    }

    fn answer(card: &OpenCard, sender: UserId, values: &[&str]) -> Message {
        let card_id = card.message_id.unwrap_or_default();
        let body = json!({"card": card_id.to_string(), "values": values, "fallback": "x"});
        message(
            card.conversation,
            sender,
            MessageKind::Answer,
            &body.to_string(),
        )
    }

    fn open_card(max_values: usize, deadline_ms: i64) -> OpenCard {
        OpenCard {
            conversation: ConversationId::new(),
            client_msg_id: ClientMsgId::new(),
            message_id: None,
            owner: UserId::new(),
            offered: vec!["approve".to_owned(), "reject".to_owned()],
            max_values,
            deadline_ms,
        }
    }

    /// A book holding one sent card (its server id recorded through `sent`).
    fn book_with_card(max_values: usize) -> (CardBook, OpenCard) {
        let mut book = CardBook::new();
        let card = open_card(max_values, DEADLINE);
        book.open(card.clone());
        let mut stored = message(card.conversation, UserId::new(), MessageKind::Rich, "{}");
        stored.client_msg_id = card.client_msg_id;
        book.sent(&stored);
        let card = book.get(card.client_msg_id).unwrap().clone();
        assert_eq!(card.message_id, stored.message_id);
        (book, card)
    }

    fn refused(book: &mut CardBook, message: &Message, refusal: CardRefusal) {
        assert_eq!(book.check(message, DEADLINE), CardCheck::Refused(refusal));
        assert_eq!(book.len(), 1, "a refusal leaves the card open");
    }

    #[test]
    fn the_owner_s_answer_is_accepted_and_closes_the_card() {
        let (mut book, card) = book_with_card(1);
        let check = book.check(&answer(&card, card.owner, &["approve"]), DEADLINE);
        assert_eq!(
            check,
            CardCheck::Accepted {
                card: card.clone(),
                values: vec!["approve".to_owned()],
            }
        );
        assert!(book.is_empty());
        assert_eq!(book.get(card.client_msg_id), None);
    }

    #[test]
    fn a_stranger_s_answer_is_refused_and_the_card_stays_open() {
        let (mut book, card) = book_with_card(1);
        refused(
            &mut book,
            &answer(&card, UserId::new(), &["approve"]),
            CardRefusal::NotOwner,
        );
        assert!(matches!(
            book.check(&answer(&card, card.owner, &["reject"]), DEADLINE),
            CardCheck::Accepted { .. }
        ));
    }

    #[test]
    fn answers_to_unknown_cards_are_refused() {
        let (mut book, card) = book_with_card(1);
        let mut other = card.clone();
        other.message_id = Some(MessageId::new());
        refused(
            &mut book,
            &answer(&other, card.owner, &["approve"]),
            CardRefusal::UnknownCard,
        );
        let mut elsewhere = card.clone();
        elsewhere.conversation = ConversationId::new();
        refused(
            &mut book,
            &answer(&elsewhere, card.owner, &["approve"]),
            CardRefusal::UnknownCard,
        );
    }

    #[test]
    fn values_must_be_distinct_offered_and_few_enough() {
        let (mut book, card) = book_with_card(1);
        for values in [
            &["maybe"][..],
            &["approve", "reject"],
            &["approve", "approve"],
            &[],
        ] {
            refused(
                &mut book,
                &answer(&card, card.owner, values),
                CardRefusal::NotOffered,
            );
        }
        let (mut book, card) = book_with_card(2);
        refused(
            &mut book,
            &answer(&card, card.owner, &["approve", "approve"]),
            CardRefusal::NotOffered,
        );
        assert!(matches!(
            book.check(&answer(&card, card.owner, &["reject", "approve"]), DEADLINE),
            CardCheck::Accepted { .. }
        ));
    }

    #[test]
    fn answers_after_the_deadline_are_late() {
        let (mut book, card) = book_with_card(1);
        let late = answer(&card, card.owner, &["approve"]);
        assert_eq!(
            book.check(&late, DEADLINE + 1),
            CardCheck::Refused(CardRefusal::Late)
        );
        assert_eq!(book.len(), 1, "a late card stays until it expires");
    }

    #[test]
    fn other_messages_are_not_card_answers() {
        let (mut book, card) = book_with_card(1);
        let body = answer(&card, card.owner, &["approve"]).body.unwrap();
        refused(
            &mut book,
            &message(card.conversation, card.owner, MessageKind::Text, &body),
            CardRefusal::NotACardAnswer,
        );
        refused(
            &mut book,
            &message(
                card.conversation,
                card.owner,
                MessageKind::Answer,
                "not json",
            ),
            CardRefusal::NotACardAnswer,
        );
        let mut deleted = answer(&card, card.owner, &["approve"]);
        deleted.body = None;
        deleted.deleted = true;
        refused(&mut book, &deleted, CardRefusal::NotACardAnswer);
    }

    #[test]
    fn an_answer_before_the_card_was_stored_is_unknown() {
        let mut book = CardBook::new();
        let card = open_card(1, DEADLINE);
        book.open(card.clone());
        let mut guess = card.clone();
        guess.message_id = Some(MessageId::new());
        refused(
            &mut book,
            &answer(&guess, card.owner, &["approve"]),
            CardRefusal::UnknownCard,
        );
        assert_eq!(book.get(card.client_msg_id), Some(&card));
    }

    #[test]
    fn sent_ignores_other_messages() {
        let (mut book, card) = book_with_card(1);
        let mut unrelated = message(card.conversation, card.owner, MessageKind::Rich, "{}");
        book.sent(&unrelated);
        unrelated.client_msg_id = card.client_msg_id;
        unrelated.conversation_id = ConversationId::new();
        book.sent(&unrelated);
        assert_eq!(book.get(card.client_msg_id), Some(&card));
    }

    #[test]
    fn expire_removes_only_cards_past_their_deadline() {
        let mut book = CardBook::new();
        let first = open_card(1, 100);
        let second = open_card(1, 200);
        let current = open_card(1, 300);
        for card in [&second, &current, &first] {
            book.open(card.clone());
        }
        assert_eq!(book.expire(300), [first, second]);
        assert_eq!(book.len(), 1);
        assert!(book.get(current.client_msg_id).is_some());
        assert_eq!(book.close(current.client_msg_id), Some(current));
        assert!(book.is_empty());
    }
}
