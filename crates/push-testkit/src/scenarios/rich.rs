//! RICH cards and ANSWERs: template enforcement on send and edit, and card state (spec
//! 2026-10-26, rich message templates, D9–D11 and "Enforcement").

use push_domain::templates::TemplateError;
use push_domain::{
    ContentType, ConversationId, DomainError, EntryKind, MAX_TEXT_BYTES, MessageRef, ReviewAction,
    ReviewCommand, SendMessage, SendReceipt, Store, ValidationError,
};

use crate::{Backend, Case, Person, World, case_list, delete, edit, message, text};

const APPROVAL: &str = r#"{"tpl":"agent.approval@1","params":{"title":"Deploy v2","details":"Ship it to production?"},"fallback":"Deploy v2 to production?"}"#;

/// A person and their agent in a direct chat, with the built-in templates seeded.
async fn agent_chat<B: Backend>(backend: &B) -> (World<B::Store>, Person, Person, ConversationId) {
    let world = World::new(backend).await;
    world.svc.seed_builtin_templates().await.unwrap();
    let person = world.person().await;
    let agent = world.agent(&person).await;
    let chat = world
        .svc
        .open_direct(person.user, agent.user)
        .await
        .unwrap();
    (world, person, agent, chat)
}

/// Sends a body of `content_type` and expects it to succeed.
async fn sent<S: Store + Clone>(
    world: &World<S>,
    from: &Person,
    chat: ConversationId,
    content_type: ContentType,
    body: &str,
) -> SendReceipt {
    world
        .svc
        .send_message(message(from, chat, content_type, body))
        .await
        .unwrap()
}

/// An ANSWER body choosing `values` on `card`.
fn answer(card: &SendReceipt, values: &[&str]) -> String {
    let values: Vec<String> = values.iter().map(|value| format!("\"{value}\"")).collect();
    format!(
        r#"{{"card":"{}","values":[{}],"fallback":"Answered"}}"#,
        card.message_id,
        values.join(",")
    )
}

/// A RICH body of `template` with these params (a JSON object's content).
fn rich(template: &str, params: &str) -> String {
    format!(r#"{{"tpl":"{template}","params":{{{params}}},"fallback":"A card"}}"#)
}

/// A tpl/1 definition open to people and agents, with one string param `text`.
fn note_definition(reference: &str) -> Vec<u8> {
    let (id, version) = reference.split_once('@').unwrap();
    let (publisher, name) = id.split_once('.').unwrap();
    format!(
        r#"{{"format":"tpl/1","publisher":"{publisher}","name":"{name}","version":{version},
        "title":"Note","senders":["human","agent"],
        "params":{{"text":{{"type":"string","max_len":200}}}},
        "layout":[{{"type":"text","param":"text"}}]}}"#
    )
    .into_bytes()
}

async fn review<S: Store + Clone>(world: &World<S>, template: &str, action: ReviewAction) {
    world
        .svc
        .review_template(ReviewCommand {
            template: template.parse().unwrap(),
            action,
            reviewer: "ops".to_owned(),
            note: None,
        })
        .await
        .unwrap();
}

fn refusal(message: &str) -> DomainError {
    DomainError::Template(TemplateError(message.to_owned()))
}

fn by_id(target: &SendReceipt) -> MessageRef {
    MessageRef::Id(target.message_id)
}

async fn an_agent_sends_an_approval_card<B: Backend>(backend: &B) {
    let (world, _person, agent, chat) = agent_chat(backend).await;

    let card = sent(&world, &agent, chat, ContentType::Rich, APPROVAL).await;

    let stored = world.entry(chat, card.message_id).await;
    assert_eq!(stored.kind, EntryKind::Message);
    assert_eq!(stored.content_type, ContentType::Rich);
    assert_eq!(stored.body.as_deref(), Some(APPROVAL.as_bytes()));
    assert_eq!((stored.target_seq, stored.answer_seq), (None, None));
}

async fn a_person_cannot_send_an_agent_only_template<B: Backend>(backend: &B) {
    let (world, person, _agent, chat) = agent_chat(backend).await;

    assert_eq!(
        world
            .svc
            .send_message(message(&person, chat, ContentType::Rich, APPROVAL))
            .await,
        Err(refusal(
            "template agent.approval@1 cannot be sent by a human"
        ))
    );
    assert_eq!(world.send(&person, chat, "gapless").await.seq, 1);
}

async fn templates_that_are_not_approved_are_refused_alike<B: Backend>(backend: &B) {
    let (world, person, agent, chat) = agent_chat(backend).await;
    for reference in ["acme.pending@1", "acme.rejected@1", "acme.revoked@1"] {
        world
            .svc
            .submit_template(person.user, &note_definition(reference))
            .await
            .unwrap();
    }
    review(&world, "acme.rejected@1", ReviewAction::Reject).await;
    review(&world, "acme.revoked@1", ReviewAction::Approve).await;
    review(&world, "acme.revoked@1", ReviewAction::Revoke).await;

    for reference in [
        "acme.pending@1",
        "acme.rejected@1",
        "acme.revoked@1",
        "acme.unknown@1",
    ] {
        for from in [&person, &agent] {
            let body = rich(reference, r#""text":"hi""#);
            assert_eq!(
                world
                    .svc
                    .send_message(message(from, chat, ContentType::Rich, &body))
                    .await,
                Err(refusal(&format!("template {reference} is not approved"))),
                "{reference}"
            );
        }
    }
    assert_eq!(world.send(&person, chat, "gapless").await.seq, 1);
}

async fn params_that_break_the_schema_are_refused<B: Backend>(backend: &B) {
    let (world, _person, agent, chat) = agent_chat(backend).await;

    for params in [
        r#""title":"Deploy v2""#,
        r#""title":"Deploy v2","details":"Ship it?","extra":"x""#,
        r#""title":"Deploy v2","details":7"#,
    ] {
        let body = rich("agent.approval@1", params);
        assert!(
            matches!(
                world
                    .svc
                    .send_message(message(&agent, chat, ContentType::Rich, &body))
                    .await,
                Err(DomainError::Template(_))
            ),
            "{params}"
        );
    }
    for body in ["", "not json", r#"{"tpl":"agent.approval@1"}"#] {
        assert!(matches!(
            world
                .svc
                .send_message(message(&agent, chat, ContentType::Rich, body))
                .await,
            Err(DomainError::Template(_))
        ));
    }
}

async fn a_system_event_cannot_be_sent<B: Backend>(backend: &B) {
    let (world, person, _agent, chat) = agent_chat(backend).await;

    assert_eq!(
        world
            .svc
            .send_message(message(&person, chat, ContentType::SystemEvent, "{}"))
            .await,
        Err(DomainError::Invalid(ValidationError::ContentType))
    );
}

async fn a_person_sends_an_approved_template_open_to_people<B: Backend>(backend: &B) {
    let (world, person, agent, chat) = agent_chat(backend).await;
    let publisher = world.person().await;
    world
        .svc
        .submit_template(publisher.user, &note_definition("acme.note@1"))
        .await
        .unwrap();
    review(&world, "acme.note@1", ReviewAction::Approve).await;

    let body = rich("acme.note@1", r#""text":"from a person""#);
    let card = sent(&world, &person, chat, ContentType::Rich, &body).await;
    assert_eq!(
        world.entry(chat, card.message_id).await.content_type,
        ContentType::Rich
    );
    sent(&world, &agent, chat, ContentType::Rich, &body).await;
}

async fn a_person_answers_an_approval_card_once<B: Backend>(backend: &B) {
    let (world, person, agent, chat) = agent_chat(backend).await;
    let card = sent(&world, &agent, chat, ContentType::Rich, APPROVAL).await;

    let body = answer(&card, &["approve"]);
    let reply = sent(&world, &person, chat, ContentType::Answer, &body).await;

    let stored = world.entry(chat, reply.message_id).await;
    assert_eq!(stored.kind, EntryKind::Message);
    assert_eq!(stored.content_type, ContentType::Answer);
    assert_eq!(stored.target_seq, Some(card.seq));
    assert_eq!(stored.body.as_deref(), Some(body.as_bytes()));
    let card_now = world.entry(chat, card.message_id).await;
    assert_eq!(card_now.answer_seq, Some(reply.seq));
    assert_eq!(card_now.body.as_deref(), Some(APPROVAL.as_bytes()));
    let page = world
        .svc
        .history(person.user, chat, None, 10)
        .await
        .unwrap();
    let answered: Vec<Option<i64>> = page.entries.iter().map(|e| e.answer_seq).collect();
    assert_eq!(answered, [Some(reply.seq), None]);

    assert_eq!(
        world
            .svc
            .send_message(message(
                &person,
                chat,
                ContentType::Answer,
                &answer(&card, &["reject"])
            ))
            .await,
        Err(DomainError::CardAnswered)
    );
    assert_eq!(
        world.send(&person, chat, "gapless").await.seq,
        reply.seq + 1
    );
}

async fn the_sender_cannot_answer_their_own_card<B: Backend>(backend: &B) {
    let (world, _person, agent, chat) = agent_chat(backend).await;
    let card = sent(&world, &agent, chat, ContentType::Rich, APPROVAL).await;

    assert_eq!(
        world
            .svc
            .send_message(message(
                &agent,
                chat,
                ContentType::Answer,
                &answer(&card, &["approve"])
            ))
            .await,
        Err(refusal("you cannot answer your own card"))
    );
}

async fn an_answer_needs_a_card_of_its_conversation<B: Backend>(backend: &B) {
    let (world, person, agent, chat) = agent_chat(backend).await;
    let friend = world.person().await;
    let other = world.direct_chat(&person, &friend).await;
    let card = sent(&world, &agent, chat, ContentType::Rich, APPROVAL).await;

    assert_eq!(
        world
            .svc
            .send_message(message(
                &person,
                other,
                ContentType::Answer,
                &answer(&card, &["approve"])
            ))
            .await,
        Err(DomainError::TargetNotFound)
    );
    let text = world.send(&agent, chat, "plain text").await;
    assert_eq!(
        world
            .svc
            .send_message(message(
                &person,
                chat,
                ContentType::Answer,
                &answer(&text, &["approve"])
            ))
            .await,
        Err(refusal("the target is not a card"))
    );
    assert_eq!(world.entry(chat, card.message_id).await.answer_seq, None);
}

async fn an_answer_must_choose_what_the_card_offers<B: Backend>(backend: &B) {
    let (world, person, agent, chat) = agent_chat(backend).await;
    let card = sent(&world, &agent, chat, ContentType::Rich, APPROVAL).await;

    for values in [&["maybe"][..], &["approve", "reject"]] {
        assert!(
            matches!(
                world
                    .svc
                    .send_message(message(
                        &person,
                        chat,
                        ContentType::Answer,
                        &answer(&card, values)
                    ))
                    .await,
                Err(DomainError::Template(_))
            ),
            "{values:?}"
        );
    }
    assert_eq!(world.entry(chat, card.message_id).await.answer_seq, None);
}

async fn a_card_without_a_choice_cannot_be_answered<B: Backend>(backend: &B) {
    let (world, person, agent, chat) = agent_chat(backend).await;
    let body = rich("agent.reply@1", r#""text":"Done.""#);
    let card = sent(&world, &agent, chat, ContentType::Rich, &body).await;

    assert!(matches!(
        world
            .svc
            .send_message(message(
                &person,
                chat,
                ContentType::Answer,
                &answer(&card, &["ok"])
            ))
            .await,
        Err(DomainError::Template(_))
    ));
}

async fn a_card_of_a_revoked_template_cannot_be_answered<B: Backend>(backend: &B) {
    let (world, person, agent, chat) = agent_chat(backend).await;
    let card = sent(&world, &agent, chat, ContentType::Rich, APPROVAL).await;
    review(&world, "agent.approval@1", ReviewAction::Revoke).await;

    assert_eq!(
        world
            .svc
            .send_message(message(
                &person,
                chat,
                ContentType::Answer,
                &answer(&card, &["approve"])
            ))
            .await,
        Err(refusal("template agent.approval@1 is not approved"))
    );
}

async fn a_deleted_card_cannot_be_answered<B: Backend>(backend: &B) {
    let (world, person, agent, chat) = agent_chat(backend).await;
    let card = sent(&world, &agent, chat, ContentType::Rich, APPROVAL).await;
    world
        .svc
        .delete_message(delete(&agent, chat, by_id(&card)))
        .await
        .unwrap();

    assert_eq!(
        world
            .svc
            .send_message(message(
                &person,
                chat,
                ContentType::Answer,
                &answer(&card, &["approve"])
            ))
            .await,
        Err(DomainError::TargetNotFound)
    );
}

async fn a_question_is_answered_with_one_of_its_own_options<B: Backend>(backend: &B) {
    let (world, person, agent, chat) = agent_chat(backend).await;
    let body = rich(
        "agent.question@1",
        r#""question":"Which colour?","options":[{"id":"red","label":"Red"},{"id":"blue","label":"Blue"}]"#,
    );
    let card = sent(&world, &agent, chat, ContentType::Rich, &body).await;

    assert!(matches!(
        world
            .svc
            .send_message(message(
                &person,
                chat,
                ContentType::Answer,
                &answer(&card, &["green"])
            ))
            .await,
        Err(DomainError::Template(_))
    ));
    let reply = sent(
        &world,
        &person,
        chat,
        ContentType::Answer,
        &answer(&card, &["blue"]),
    )
    .await;
    assert_eq!(
        world.entry(chat, card.message_id).await.answer_seq,
        Some(reply.seq)
    );
}

async fn a_card_is_edited_with_a_full_rich_body<B: Backend>(backend: &B) {
    let (world, _person, agent, chat) = agent_chat(backend).await;
    let card = sent(&world, &agent, chat, ContentType::Rich, APPROVAL).await;
    let changed = rich(
        "agent.approval@1",
        r#""title":"Deploy v3","details":"Ship v3 instead?""#,
    );

    let receipt = world
        .svc
        .edit_text(edit(&agent, chat, by_id(&card), &changed))
        .await
        .unwrap();

    let stored = world.entry(chat, card.message_id).await;
    assert_eq!(stored.content_type, ContentType::Rich);
    assert_eq!(stored.body.as_deref(), Some(changed.as_bytes()));
    assert_eq!(stored.edit_seq, Some(receipt.seq));
    let change = world.entry(chat, receipt.message_id).await;
    assert_eq!(change.kind, EntryKind::Edit);
    assert_eq!(change.content_type, ContentType::Rich);

    for bad in [
        rich("agent.approval@1", r#""title":"Deploy v3""#),
        "plain text".to_owned(),
        rich("agent.unknown@1", r#""title":"Deploy v3""#),
    ] {
        assert!(
            matches!(
                world
                    .svc
                    .edit_text(edit(&agent, chat, by_id(&card), &bad))
                    .await,
                Err(DomainError::Template(_))
            ),
            "{bad}"
        );
    }
    let too_large = "a".repeat(push_domain::templates::MAX_RICH_BYTES + 1);
    assert_eq!(
        world
            .svc
            .edit_text(edit(&agent, chat, by_id(&card), &too_large))
            .await,
        Err(DomainError::Invalid(ValidationError::BodyTooLarge {
            actual: push_domain::templates::MAX_RICH_BYTES + 1,
            limit: push_domain::templates::MAX_RICH_BYTES,
        }))
    );
    assert_eq!(
        world.entry(chat, card.message_id).await.body.as_deref(),
        Some(changed.as_bytes())
    );
}

async fn answered_cards_and_answers_cannot_be_edited<B: Backend>(backend: &B) {
    let (world, person, agent, chat) = agent_chat(backend).await;
    let card = sent(&world, &agent, chat, ContentType::Rich, APPROVAL).await;
    let reply = sent(
        &world,
        &person,
        chat,
        ContentType::Answer,
        &answer(&card, &["approve"]),
    )
    .await;

    assert_eq!(
        world
            .svc
            .edit_text(edit(&agent, chat, by_id(&card), APPROVAL))
            .await,
        Err(DomainError::CardAnswered)
    );
    assert_eq!(
        world
            .svc
            .edit_text(edit(
                &person,
                chat,
                by_id(&reply),
                &answer(&card, &["reject"])
            ))
            .await,
        Err(refusal("an answer cannot be edited"))
    );
}

async fn text_edits_are_still_checked_as_text<B: Backend>(backend: &B) {
    let (world, person, _agent, chat) = agent_chat(backend).await;
    let hello = world.send(&person, chat, "hello").await;

    world.edit(&person, chat, &hello, "hello again").await;
    let stored = world.entry(chat, hello.message_id).await;
    assert_eq!(stored.content_type, ContentType::Text);
    assert_eq!(stored.body.as_deref(), Some(&b"hello again"[..]));
    let longest = "a".repeat(MAX_TEXT_BYTES);
    world.edit(&person, chat, &hello, &longest).await;
    let too_long = "a".repeat(MAX_TEXT_BYTES + 1);
    assert_eq!(
        world
            .svc
            .edit_text(edit(&person, chat, by_id(&hello), &too_long))
            .await,
        Err(DomainError::Invalid(ValidationError::BodyTooLarge {
            actual: MAX_TEXT_BYTES + 1,
            limit: MAX_TEXT_BYTES,
        }))
    );
    let mut not_utf8 = edit(&person, chat, by_id(&hello), "x");
    not_utf8.body = vec![0xff, 0xfe];
    assert_eq!(
        world.svc.edit_text(not_utf8).await,
        Err(DomainError::Invalid(ValidationError::NotUtf8))
    );
}

async fn a_retry_of_a_card_returns_its_receipt_after_a_revocation<B: Backend>(backend: &B) {
    let (world, _person, agent, chat) = agent_chat(backend).await;
    let command = message(&agent, chat, ContentType::Rich, APPROVAL);
    let first = world.svc.send_message(command.clone()).await.unwrap();
    review(&world, "agent.approval@1", ReviewAction::Revoke).await;

    assert_eq!(world.svc.send_message(command).await, Ok(first));
    assert_eq!(
        world
            .svc
            .send_message(message(&agent, chat, ContentType::Rich, APPROVAL))
            .await,
        Err(refusal("template agent.approval@1 is not approved"))
    );
}

async fn a_client_msg_id_reused_for_another_content_type_conflicts<B: Backend>(backend: &B) {
    let (world, person, agent, chat) = agent_chat(backend).await;
    let card = sent(&world, &agent, chat, ContentType::Rich, APPROVAL).await;
    let text = text(&person, chat, "approve");
    world.svc.send_text(text.clone()).await.unwrap();

    let reused = SendMessage {
        client_msg_id: text.client_msg_id,
        ..message(
            &person,
            chat,
            ContentType::Answer,
            &answer(&card, &["approve"]),
        )
    };
    assert_eq!(
        world.svc.send_message(reused).await,
        Err(DomainError::ClientIdConflict)
    );
    assert_eq!(world.entry(chat, card.message_id).await.answer_seq, None);
}

async fn a_member_who_joined_after_a_card_cannot_answer_it_but_receives_its_answer<B: Backend>(
    backend: &B,
) {
    let world = World::new(backend).await;
    world.svc.seed_builtin_templates().await.unwrap();
    let alice = world.person().await;
    let agent = world.agent(&alice).await;
    let dave = world.person().await;
    // The agent is already its owner's contact.
    let group = world
        .svc
        .create_group(alice.user, "Group", &[agent.user])
        .await
        .unwrap();
    let card = sent(&world, &agent, group, ContentType::Rich, APPROVAL).await;
    world.befriend(&alice, &dave).await;
    world
        .svc
        .add_members(alice.user, group, &[dave.user])
        .await
        .unwrap();

    assert_eq!(
        world
            .svc
            .send_message(message(
                &dave,
                group,
                ContentType::Answer,
                &answer(&card, &["approve"])
            ))
            .await,
        Err(DomainError::TargetNotFound)
    );
    let reply = sent(
        &world,
        &alice,
        group,
        ContentType::Answer,
        &answer(&card, &["approve"]),
    )
    .await;

    // The answer is a new message: Dave, who never saw the card, still receives it.
    let batch = world.svc.sync(dave.device, 100).await.unwrap();
    let last = batch.entries.last().unwrap();
    assert_eq!(
        (last.seq, last.content_type, last.target_seq),
        (reply.seq, ContentType::Answer, Some(card.seq))
    );
    assert!(world.svc.ack(dave.device, group, last.seq).await.unwrap());
    assert_eq!(world.position(dave.device, group).await, Some(reply.seq));
}

async fn deleting_an_answer_leaves_the_card_answered<B: Backend>(backend: &B) {
    let (world, person, agent, chat) = agent_chat(backend).await;
    let card = sent(&world, &agent, chat, ContentType::Rich, APPROVAL).await;
    let reply = sent(
        &world,
        &person,
        chat,
        ContentType::Answer,
        &answer(&card, &["approve"]),
    )
    .await;

    world
        .svc
        .delete_message(delete(&person, chat, by_id(&reply)))
        .await
        .unwrap();

    assert!(
        world
            .entry(chat, reply.message_id)
            .await
            .deleted_at
            .is_some()
    );
    assert_eq!(
        world.entry(chat, card.message_id).await.answer_seq,
        Some(reply.seq)
    );
    assert_eq!(
        world
            .svc
            .send_message(message(
                &person,
                chat,
                ContentType::Answer,
                &answer(&card, &["reject"])
            ))
            .await,
        Err(DomainError::CardAnswered)
    );
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
        an_agent_sends_an_approval_card,
        a_person_cannot_send_an_agent_only_template,
        templates_that_are_not_approved_are_refused_alike,
        params_that_break_the_schema_are_refused,
        a_system_event_cannot_be_sent,
        a_person_sends_an_approved_template_open_to_people,
        a_person_answers_an_approval_card_once,
        the_sender_cannot_answer_their_own_card,
        an_answer_needs_a_card_of_its_conversation,
        an_answer_must_choose_what_the_card_offers,
        a_card_without_a_choice_cannot_be_answered,
        a_card_of_a_revoked_template_cannot_be_answered,
        a_deleted_card_cannot_be_answered,
        a_question_is_answered_with_one_of_its_own_options,
        a_card_is_edited_with_a_full_rich_body,
        answered_cards_and_answers_cannot_be_edited,
        text_edits_are_still_checked_as_text,
        a_retry_of_a_card_returns_its_receipt_after_a_revocation,
        a_client_msg_id_reused_for_another_content_type_conflicts,
        a_member_who_joined_after_a_card_cannot_answer_it_but_receives_its_answer,
        deleting_an_answer_leaves_the_card_answered,
    ]
}
