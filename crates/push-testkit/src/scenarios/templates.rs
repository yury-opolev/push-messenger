use chrono::{SubsecRound, TimeDelta};
use push_domain::templates::{BUILTIN_PUBLISHER, Definition, TemplateRef, builtins};
use push_domain::{
    Catalog, Clock, DomainError, MAX_CATALOG_PAGE, MAX_PENDING_PER_USER, ReviewAction,
    ReviewCommand, Store, StoredTemplate, TemplateAction, TemplateEvent, TemplateStatus, UserId,
    ValidationError,
};

use crate::{Backend, Case, World, case_list};

/// A valid tpl/1 definition of `<publisher>.<name>@<version>`.
fn definition(publisher: &str, name: &str, version: u16) -> Vec<u8> {
    format!(
        r#"{{"format":"tpl/1","publisher":"{publisher}","name":"{name}","version":{version},
        "title":"Deploy check","senders":["agent","human"],
        "params":{{"text":{{"type":"string","max_len":200}}}},
        "layout":[{{"type":"text","param":"text"}}]}}"#
    )
    .into_bytes()
}

fn template_ref(text: &str) -> TemplateRef {
    text.parse().unwrap()
}

fn review(template: &str, action: ReviewAction) -> ReviewCommand {
    ReviewCommand {
        template: template_ref(template),
        action,
        reviewer: "ops".to_owned(),
        note: None,
    }
}

/// Submits `<publisher>.<name>@<version>` as `user` and expects it to succeed.
async fn submit<S: Store + Clone>(
    world: &World<S>,
    user: UserId,
    publisher: &str,
    name: &str,
    version: u16,
) -> StoredTemplate {
    world
        .svc
        .submit_template(user, &definition(publisher, name, version))
        .await
        .unwrap()
}

/// Reviews a version and expects it to succeed.
async fn reviewed<S: Store + Clone>(
    world: &World<S>,
    template: &str,
    action: ReviewAction,
) -> StoredTemplate {
    world
        .svc
        .review_template(review(template, action))
        .await
        .unwrap()
}

/// Every catalog entry after `since`, read in one page.
async fn catalog<S: Store + Clone>(world: &World<S>, since: i64) -> Catalog {
    world.svc.template_catalog(since, 0).await.unwrap()
}

fn refs(entries: &[StoredTemplate]) -> Vec<String> {
    entries
        .iter()
        .map(|stored| stored.template.to_string())
        .collect()
}

async fn a_submission_waits_for_review_outside_the_catalog<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;

    let stored = submit(&world, alice.user, "acme", "deploy", 1).await;

    let canonical = Definition::parse(&definition("acme", "deploy", 1))
        .unwrap()
        .canonical_json();
    assert_eq!(stored.template, template_ref("acme.deploy@1"));
    assert_eq!(stored.definition, canonical);
    assert_eq!(stored.title, "Deploy check");
    assert_eq!(stored.status, TemplateStatus::Pending);
    assert_eq!(stored.submitted_by, Some(alice.user));
    assert_eq!(stored.submitted_at, world.clock.now());
    assert_eq!((stored.reviewed_at, stored.change_seq), (None, None));
    let catalog = catalog(&world, 0).await;
    assert!(catalog.entries.is_empty());
    assert_eq!((catalog.seq, catalog.more), (0, false));
    let pending = world
        .svc
        .list_templates(Some(TemplateStatus::Pending), 0)
        .await
        .unwrap();
    assert_eq!(pending, [stored]);
}

async fn an_approved_version_enters_the_catalog_with_its_definition<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    let submitted = submit(&world, alice.user, "acme", "deploy", 1).await;
    world.clock.advance(TimeDelta::minutes(5));

    let approved = reviewed(&world, "acme.deploy@1", ReviewAction::Approve).await;

    assert_eq!(approved.status, TemplateStatus::Approved);
    assert_eq!(approved.reviewed_at, Some(world.clock.now()));
    assert_eq!(approved.change_seq, Some(1));
    assert_eq!(approved.definition, submitted.definition);
    let catalog = catalog(&world, 0).await;
    assert_eq!(catalog.entries, [approved]);
    assert_eq!((catalog.seq, catalog.more), (1, false));
    assert!(
        world
            .svc
            .template_catalog(1, 0)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
}

async fn a_rejected_version_never_enters_the_catalog<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    submit(&world, alice.user, "acme", "deploy", 1).await;

    let rejected = reviewed(&world, "acme.deploy@1", ReviewAction::Reject).await;

    assert_eq!(rejected.status, TemplateStatus::Rejected);
    assert_eq!(rejected.reviewed_at, Some(world.clock.now()));
    assert_eq!(rejected.change_seq, None);
    let catalog = catalog(&world, 0).await;
    assert!(catalog.entries.is_empty());
    assert_eq!(catalog.seq, 0);
}

async fn a_revoked_version_reappears_in_the_catalog_as_revoked_at_a_higher_seq<B: Backend>(
    backend: &B,
) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    submit(&world, alice.user, "acme", "deploy", 1).await;
    let approved = reviewed(&world, "acme.deploy@1", ReviewAction::Approve).await;
    let seen = catalog(&world, 0).await.seq;

    let revoked = reviewed(&world, "acme.deploy@1", ReviewAction::Revoke).await;

    assert_eq!(revoked.status, TemplateStatus::Revoked);
    assert!(revoked.change_seq > approved.change_seq);
    let catalog = catalog(&world, seen).await;
    assert_eq!(catalog.entries, std::slice::from_ref(&revoked));
    assert_eq!(catalog.seq, revoked.change_seq.unwrap());
}

async fn the_catalog_pages_without_gaps_or_repeats<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    for name in ["a", "b", "c", "d", "e"] {
        submit(&world, alice.user, "acme", name, 1).await;
        reviewed(&world, &format!("acme.{name}@1"), ReviewAction::Approve).await;
    }
    reviewed(&world, "acme.b@1", ReviewAction::Revoke).await;

    let mut since = 0;
    let mut pages = Vec::new();
    loop {
        let page = world.svc.template_catalog(since, 2).await.unwrap();
        assert!(page.entries.len() <= 2);
        pages.push(refs(&page.entries));
        since = page.seq;
        if !page.more {
            break;
        }
    }

    let seen: Vec<String> = pages.concat();
    assert_eq!(
        seen,
        ["acme.a@1", "acme.c@1", "acme.d@1", "acme.e@1", "acme.b@1"]
    );
    assert_eq!(since, 6);
    let after = world.svc.template_catalog(since, 2).await.unwrap();
    assert!(after.entries.is_empty());
    assert_eq!((after.seq, after.more), (6, false));
}

async fn the_catalog_refuses_a_negative_position_and_caps_its_page<B: Backend>(backend: &B) {
    let world = World::new(backend).await;

    assert_eq!(
        world.svc.template_catalog(-1, 10).await,
        Err(DomainError::Invalid(ValidationError::CatalogPosition {
            actual: -1
        }))
    );
    let page = world
        .svc
        .template_catalog(0, MAX_CATALOG_PAGE + 1)
        .await
        .unwrap();
    assert_eq!((page.seq, page.more), (0, false));
    // A client ahead of the server (a restored database) is sent back to the server's
    // position, so it does not miss the changes made from there on.
    let ahead = world.svc.template_catalog(7, 10).await.unwrap();
    assert_eq!((ahead.seq, ahead.more), (0, false));
    let alice = world.person().await;
    submit(&world, alice.user, "acme", "deploy", 1).await;
    reviewed(&world, "acme.deploy@1", ReviewAction::Approve).await;
    let ahead = world.svc.template_catalog(7, 10).await.unwrap();
    assert!(ahead.entries.is_empty());
    assert_eq!((ahead.seq, ahead.more), (1, false));
}

async fn a_publisher_belongs_to_the_account_that_claimed_it<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    submit(&world, alice.user, "acme", "deploy", 1).await;

    assert_eq!(
        world
            .svc
            .submit_template(bob.user, &definition("acme", "other", 1))
            .await,
        Err(DomainError::TemplateConflict(
            "the publisher belongs to another account"
        ))
    );
    submit(&world, alice.user, "acme", "other", 1).await;
    submit(&world, alice.user, "acme", "deploy", 2).await;
    submit(&world, bob.user, "bobco", "deploy", 1).await;
}

async fn reserved_publishers_are_refused<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;

    for publisher in ["agent", "push", "system", "admin", "builtin"] {
        assert_eq!(
            world
                .svc
                .submit_template(alice.user, &definition(publisher, "mine", 1))
                .await,
            Err(DomainError::TemplateConflict("the publisher is reserved")),
            "{publisher}"
        );
    }
}

async fn an_invalid_definition_or_an_unknown_user_is_refused<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;

    assert!(matches!(
        world.svc.submit_template(alice.user, b"{}").await,
        Err(DomainError::Template(_))
    ));
    assert!(matches!(
        world.svc.submit_template(alice.user, b"not json").await,
        Err(DomainError::Template(_))
    ));
    assert_eq!(
        world
            .svc
            .submit_template(UserId::new(), &definition("acme", "deploy", 1))
            .await,
        Err(DomainError::UnknownUser)
    );
    assert!(world.svc.list_templates(None, 0).await.unwrap().is_empty());
}

async fn an_account_may_have_twenty_submissions_waiting<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    for version in 1..=MAX_PENDING_PER_USER as u16 {
        submit(&world, alice.user, "acme", "deploy", version).await;
    }

    let refused = Err(DomainError::TemplateConflict(
        "too many submissions are waiting for review",
    ));
    assert_eq!(
        world
            .svc
            .submit_template(alice.user, &definition("acme", "deploy", 21))
            .await,
        refused
    );
    // The cap counts the account's submissions under every publisher.
    assert_eq!(
        world
            .svc
            .submit_template(alice.user, &definition("other", "deploy", 1))
            .await,
        refused
    );
    reviewed(&world, "acme.deploy@1", ReviewAction::Reject).await;
    submit(&world, alice.user, "acme", "deploy", 21).await;
}

async fn a_stored_version_cannot_be_submitted_again<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    submit(&world, alice.user, "acme", "deploy", 1).await;
    let approved = reviewed(&world, "acme.deploy@1", ReviewAction::Approve).await;

    let changed = String::from_utf8(definition("acme", "deploy", 1))
        .unwrap()
        .replace("Deploy check", "Changed title");
    assert_eq!(
        world
            .svc
            .submit_template(alice.user, changed.as_bytes())
            .await,
        Err(DomainError::TemplateExists)
    );
    let (stored, _) = world
        .svc
        .template_detail(&template_ref("acme.deploy@1"))
        .await
        .unwrap();
    assert_eq!(stored, approved);
}

async fn reviews_follow_the_one_way_status<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    submit(&world, alice.user, "acme", "approved", 1).await;
    submit(&world, alice.user, "acme", "pending", 1).await;
    reviewed(&world, "acme.approved@1", ReviewAction::Approve).await;
    let not_pending = Err(DomainError::TemplateConflict("the template is not pending"));
    let not_approved = Err(DomainError::TemplateConflict(
        "the template is not approved",
    ));

    let svc = &world.svc;
    assert_eq!(
        svc.review_template(review("acme.approved@1", ReviewAction::Approve))
            .await,
        not_pending
    );
    assert_eq!(
        svc.review_template(review("acme.approved@1", ReviewAction::Reject))
            .await,
        not_pending
    );
    assert_eq!(
        svc.review_template(review("acme.pending@1", ReviewAction::Revoke))
            .await,
        not_approved
    );
    assert_eq!(
        svc.review_template(review("acme.missing@1", ReviewAction::Approve))
            .await,
        Err(DomainError::TemplateNotFound)
    );
    reviewed(&world, "acme.approved@1", ReviewAction::Revoke).await;
    assert_eq!(
        svc.review_template(review("acme.approved@1", ReviewAction::Revoke))
            .await,
        not_approved
    );
    assert_eq!(catalog(&world, 0).await.seq, 2);
}

async fn a_review_needs_a_printable_reviewer_and_a_short_note<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    submit(&world, alice.user, "acme", "deploy", 1).await;

    for reviewer in [
        String::new(),
        "   ".to_owned(),
        "a".repeat(65),
        "o\u{7}ps".to_owned(),
    ] {
        let command = ReviewCommand {
            reviewer: reviewer.clone(),
            ..review("acme.deploy@1", ReviewAction::Approve)
        };
        assert_eq!(
            world.svc.review_template(command).await,
            Err(DomainError::Invalid(ValidationError::Reviewer)),
            "{reviewer:?}"
        );
    }
    for note in ["a".repeat(501), "tab\there".to_owned()] {
        let command = ReviewCommand {
            note: Some(note),
            ..review("acme.deploy@1", ReviewAction::Approve)
        };
        assert_eq!(
            world.svc.review_template(command).await,
            Err(DomainError::Invalid(ValidationError::ReviewNote))
        );
    }
    let command = ReviewCommand {
        reviewer: "é".repeat(64),
        note: Some(format!("{}\n", "é".repeat(499))),
        ..review("acme.deploy@1", ReviewAction::Approve)
    };
    let approved = world.svc.review_template(command).await.unwrap();
    assert_eq!(approved.status, TemplateStatus::Approved);
}

async fn a_note_keeps_windows_line_breaks_as_newlines<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    submit(&world, alice.user, "acme", "deploy", 1).await;

    let command = ReviewCommand {
        note: Some("a lone\rreturn".to_owned()),
        ..review("acme.deploy@1", ReviewAction::Approve)
    };
    assert_eq!(
        world.svc.review_template(command).await,
        Err(DomainError::Invalid(ValidationError::ReviewNote))
    );
    // 501 characters as sent, 500 once "\r\n" is read as "\n".
    let note = format!("{}\r\n", "a".repeat(499));
    let command = ReviewCommand {
        note: Some(note),
        ..review("acme.deploy@1", ReviewAction::Approve)
    };
    world.svc.review_template(command).await.unwrap();
    let command = ReviewCommand {
        note: Some("looks good\r\nship it".to_owned()),
        ..review("acme.deploy@1", ReviewAction::Revoke)
    };
    world.svc.review_template(command).await.unwrap();

    let (_, events) = world
        .svc
        .template_detail(&template_ref("acme.deploy@1"))
        .await
        .unwrap();
    let notes: Vec<Option<String>> = events.into_iter().map(|event| event.note).collect();
    assert_eq!(
        notes,
        [
            None,
            Some(format!("{}\n", "a".repeat(499))),
            Some("looks good\nship it".to_owned()),
        ]
    );
}

async fn seeding_stores_the_builtins_once<B: Backend>(backend: &B) {
    let world = World::new(backend).await;

    assert_eq!(world.svc.seed_builtin_templates().await, Ok(4));

    let catalog = catalog(&world, 0).await;
    assert_eq!(
        refs(&catalog.entries),
        [
            "agent.reply@1",
            "agent.approval@1",
            "agent.question@1",
            "agent.plan@1"
        ]
    );
    assert_eq!(catalog.seq, 4);
    for (stored, builtin) in catalog.entries.iter().zip(builtins()) {
        assert_eq!(stored.status, TemplateStatus::Approved);
        assert_eq!(stored.definition, builtin.canonical_json());
        assert_eq!(stored.title, builtin.title());
        assert_eq!(stored.submitted_by, None);
        assert_eq!(stored.reviewed_at, Some(stored.submitted_at));
    }
    world.clock.advance(TimeDelta::hours(1));
    assert_eq!(world.svc.seed_builtin_templates().await, Ok(0));
    let again = world.svc.template_catalog(0, 0).await.unwrap();
    assert_eq!(again.entries, catalog.entries);
    assert_eq!(again.seq, 4);
}

async fn a_revoked_builtin_stays_revoked_after_seeding_again<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    world.svc.seed_builtin_templates().await.unwrap();
    let revoked = reviewed(&world, "agent.plan@1", ReviewAction::Revoke).await;

    assert_eq!(world.svc.seed_builtin_templates().await, Ok(0));

    let (stored, _) = world
        .svc
        .template_detail(&template_ref("agent.plan@1"))
        .await
        .unwrap();
    assert_eq!(stored, revoked);
    assert_eq!(catalog(&world, 0).await.seq, 5);
    let alice = world.person().await;
    assert_eq!(
        world
            .svc
            .submit_template(alice.user, &definition(BUILTIN_PUBLISHER, "plan", 2))
            .await,
        Err(DomainError::TemplateConflict("the publisher is reserved"))
    );
}

async fn the_audit_trail_records_every_step<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    let submitted_at = world.clock.now();
    submit(&world, alice.user, "acme", "deploy", 1).await;
    submit(&world, alice.user, "acme", "draft", 1).await;
    world.clock.advance(TimeDelta::minutes(1));
    let approved_at = world.clock.now();
    world
        .svc
        .review_template(ReviewCommand {
            note: Some("looks good\nship it".to_owned()),
            ..review("acme.deploy@1", ReviewAction::Approve)
        })
        .await
        .unwrap();
    reviewed(&world, "acme.draft@1", ReviewAction::Reject).await;
    world.clock.advance(TimeDelta::minutes(1));
    let revoked_at = world.clock.now();
    world
        .svc
        .review_template(ReviewCommand {
            reviewer: "security".to_owned(),
            note: Some("phishing".to_owned()),
            ..review("acme.deploy@1", ReviewAction::Revoke)
        })
        .await
        .unwrap();

    let event = |template: &str, action, actor, reviewer: Option<&str>, note: Option<&str>, at| {
        TemplateEvent {
            template: template_ref(template),
            action,
            actor,
            reviewer: reviewer.map(str::to_owned),
            note: note.map(str::to_owned),
            at,
        }
    };
    let (_, deploy) = world
        .svc
        .template_detail(&template_ref("acme.deploy@1"))
        .await
        .unwrap();
    assert_eq!(
        deploy,
        [
            event(
                "acme.deploy@1",
                TemplateAction::Submit,
                Some(alice.user),
                None,
                None,
                submitted_at
            ),
            event(
                "acme.deploy@1",
                TemplateAction::Approve,
                None,
                Some("ops"),
                Some("looks good\nship it"),
                approved_at
            ),
            event(
                "acme.deploy@1",
                TemplateAction::Revoke,
                None,
                Some("security"),
                Some("phishing"),
                revoked_at
            ),
        ]
    );
    let (_, draft) = world
        .svc
        .template_detail(&template_ref("acme.draft@1"))
        .await
        .unwrap();
    let actions: Vec<TemplateAction> = draft.iter().map(|event| event.action).collect();
    assert_eq!(actions, [TemplateAction::Submit, TemplateAction::Reject]);

    world.svc.seed_builtin_templates().await.unwrap();
    let (_, reply) = world
        .svc
        .template_detail(&template_ref("agent.reply@1"))
        .await
        .unwrap();
    assert_eq!(
        reply,
        [event(
            "agent.reply@1",
            TemplateAction::Seed,
            None,
            None,
            None,
            revoked_at
        )]
    );
    assert_eq!(
        world
            .svc
            .template_detail(&template_ref("acme.missing@1"))
            .await,
        Err(DomainError::TemplateNotFound)
    );
}

async fn times_are_kept_to_the_microsecond<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    world.clock.advance(TimeDelta::nanoseconds(123_456_789));
    let now = world.clock.now().trunc_subsecs(6);

    let submitted = submit(&world, alice.user, "acme", "deploy", 1).await;
    let approved = reviewed(&world, "acme.deploy@1", ReviewAction::Approve).await;

    assert_eq!(submitted.submitted_at, now);
    assert_eq!(approved.reviewed_at, Some(now));
    let (stored, events) = world
        .svc
        .template_detail(&template_ref("acme.deploy@1"))
        .await
        .unwrap();
    assert_eq!(stored, approved);
    assert!(events.iter().all(|event| event.at == now));
}

async fn templates_are_listed_newest_first_by_status<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    for name in ["a", "b", "c"] {
        submit(&world, alice.user, "acme", name, 1).await;
        world.clock.advance(TimeDelta::seconds(1));
    }
    reviewed(&world, "acme.b@1", ReviewAction::Approve).await;

    let svc = &world.svc;
    let all = svc.list_templates(None, 0).await.unwrap();
    assert_eq!(refs(&all), ["acme.c@1", "acme.b@1", "acme.a@1"]);
    let pending = svc
        .list_templates(Some(TemplateStatus::Pending), 1)
        .await
        .unwrap();
    assert_eq!(refs(&pending), ["acme.c@1"]);
    let approved = svc
        .list_templates(Some(TemplateStatus::Approved), 0)
        .await
        .unwrap();
    assert_eq!(refs(&approved), ["acme.b@1"]);
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
        a_submission_waits_for_review_outside_the_catalog,
        an_approved_version_enters_the_catalog_with_its_definition,
        a_rejected_version_never_enters_the_catalog,
        a_revoked_version_reappears_in_the_catalog_as_revoked_at_a_higher_seq,
        the_catalog_pages_without_gaps_or_repeats,
        the_catalog_refuses_a_negative_position_and_caps_its_page,
        a_publisher_belongs_to_the_account_that_claimed_it,
        reserved_publishers_are_refused,
        an_invalid_definition_or_an_unknown_user_is_refused,
        an_account_may_have_twenty_submissions_waiting,
        a_stored_version_cannot_be_submitted_again,
        reviews_follow_the_one_way_status,
        a_review_needs_a_printable_reviewer_and_a_short_note,
        a_note_keeps_windows_line_breaks_as_newlines,
        seeding_stores_the_builtins_once,
        a_revoked_builtin_stays_revoked_after_seeding_again,
        the_audit_trail_records_every_step,
        times_are_kept_to_the_microsecond,
        templates_are_listed_newest_first_by_status,
    ]
}
