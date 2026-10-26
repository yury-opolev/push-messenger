use chrono::{DateTime, TimeDelta, Utc};
use push_domain::templates::{Definition, TemplateRef, builtins};
use push_domain::{
    DomainError, InsertOutcome, PublisherOwner, Store, StoredTemplate, TemplateAction,
    TemplateEvent, TemplateStatus, Tx, UserId,
};

use super::{add_users, start};
use crate::{Backend, Case, case_list};

/// A pending version of a small valid definition, submitted by `by` (a built-in for
/// `None`) at `at`.
fn template(reference: &str, by: Option<UserId>, at: DateTime<Utc>) -> StoredTemplate {
    let template: TemplateRef = reference.parse().unwrap();
    let json = format!(
        r#"{{"format": "tpl/1", "publisher": "{}", "name": "{}", "version": {},
            "title": "Note", "senders": ["human", "agent"],
            "params": {{"text": {{"type": "string", "max_len": 100}}}},
            "layout": [{{"type": "text", "param": "text"}}]}}"#,
        template.publisher, template.name, template.version
    );
    let definition = Definition::parse(json.as_bytes()).unwrap();
    StoredTemplate {
        template,
        title: definition.title().to_owned(),
        definition: definition.canonical_json(),
        status: TemplateStatus::Pending,
        submitted_by: by,
        submitted_at: at,
        reviewed_at: None,
        change_seq: None,
    }
}

/// A user's pending version under a publisher the user has claimed.
async fn submit<T: Tx>(tx: &mut T, template: StoredTemplate) {
    let owner = template
        .submitted_by
        .map_or(PublisherOwner::Server, PublisherOwner::User);
    tx.claim_publisher(&template.template.publisher, owner)
        .await
        .unwrap();
    assert_eq!(
        tx.insert_template(template).await.unwrap(),
        InsertOutcome::Inserted
    );
}

fn refs(templates: &[StoredTemplate]) -> Vec<String> {
    templates
        .iter()
        .map(|stored| stored.template.to_string())
        .collect()
}

async fn a_stored_template_is_read_back_whole<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let user = UserId::new();
    add_users(&store, &[user]).await;
    let definition = &builtins()[0];
    let builtin = StoredTemplate {
        template: definition.template_ref(),
        title: definition.title().to_owned(),
        definition: definition.canonical_json(),
        status: TemplateStatus::Approved,
        submitted_by: None,
        submitted_at: start(),
        reviewed_at: Some(start()),
        change_seq: Some(1),
    };
    let submitted = template("acme.deploy@3", Some(user), start());
    let mut tx = store.begin().await.unwrap();
    submit(&mut tx, builtin.clone()).await;
    submit(&mut tx, submitted.clone()).await;
    tx.commit().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    assert_eq!(
        tx.template(&builtin.template).await,
        Ok(Some(builtin.clone()))
    );
    assert_eq!(
        tx.lock_template(&submitted.template).await,
        Ok(Some(submitted))
    );
    assert_eq!(
        tx.template(&"acme.deploy@4".parse().unwrap()).await,
        Ok(None)
    );
    assert_eq!(
        tx.lock_template(&"acme.other@3".parse().unwrap()).await,
        Ok(None)
    );
}

async fn a_stored_version_is_never_replaced<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let user = UserId::new();
    add_users(&store, &[user]).await;
    let original = template("acme.deploy@1", Some(user), start());
    let mut tx = store.begin().await.unwrap();
    submit(&mut tx, original.clone()).await;
    let other = StoredTemplate {
        title: "Other".into(),
        status: TemplateStatus::Approved,
        submitted_at: start() + TimeDelta::hours(1),
        ..template("acme.deploy@1", None, start())
    };
    assert_eq!(
        tx.insert_template(other).await.unwrap(),
        InsertOutcome::Duplicate
    );

    assert_eq!(tx.template(&original.template).await, Ok(Some(original)));
}

async fn a_template_needs_a_claimed_publisher<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let mut tx = store.begin().await.unwrap();
    assert!(matches!(
        tx.insert_template(template("acme.deploy@1", None, start()))
            .await,
        Err(DomainError::Store(_))
    ));
}

async fn reviews_set_the_status_the_time_and_the_change<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let user = UserId::new();
    add_users(&store, &[user]).await;
    let (approved, rejected) = (
        template("acme.deploy@1", Some(user), start()),
        template("acme.deploy@2", Some(user), start()),
    );
    let (first, second) = (start() + TimeDelta::hours(1), start() + TimeDelta::hours(2));
    let mut tx = store.begin().await.unwrap();
    submit(&mut tx, approved.clone()).await;
    submit(&mut tx, rejected.clone()).await;
    tx.set_template_status(&approved.template, TemplateStatus::Approved, first, Some(1))
        .await
        .unwrap();
    tx.set_template_status(&rejected.template, TemplateStatus::Rejected, first, None)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    assert_eq!(
        tx.template(&approved.template).await.unwrap(),
        Some(StoredTemplate {
            status: TemplateStatus::Approved,
            reviewed_at: Some(first),
            change_seq: Some(1),
            ..approved.clone()
        })
    );
    assert_eq!(
        tx.template(&rejected.template).await.unwrap(),
        Some(StoredTemplate {
            status: TemplateStatus::Rejected,
            reviewed_at: Some(first),
            ..rejected
        })
    );
    tx.set_template_status(&approved.template, TemplateStatus::Revoked, second, Some(2))
        .await
        .unwrap();
    assert_eq!(
        tx.template(&approved.template).await.unwrap(),
        Some(StoredTemplate {
            status: TemplateStatus::Revoked,
            reviewed_at: Some(second),
            change_seq: Some(2),
            ..approved
        })
    );
}

async fn only_the_one_way_status_changes_are_allowed<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let user = UserId::new();
    add_users(&store, &[user]).await;
    let (pending, approved, rejected, revoked) = (
        template("acme.pending@1", Some(user), start()),
        template("acme.approved@1", Some(user), start()),
        template("acme.rejected@1", Some(user), start()),
        template("acme.revoked@1", Some(user), start()),
    );
    let at = start() + TimeDelta::hours(1);
    let mut tx = store.begin().await.unwrap();
    for stored in [&pending, &approved, &rejected, &revoked] {
        submit(&mut tx, stored.clone()).await;
    }
    tx.set_template_status(&approved.template, TemplateStatus::Approved, at, Some(1))
        .await
        .unwrap();
    tx.set_template_status(&rejected.template, TemplateStatus::Rejected, at, None)
        .await
        .unwrap();
    tx.set_template_status(&revoked.template, TemplateStatus::Approved, at, Some(2))
        .await
        .unwrap();
    tx.set_template_status(&revoked.template, TemplateStatus::Revoked, at, Some(3))
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let unknown: TemplateRef = "acme.unknown@1".parse().unwrap();
    let refused = [
        (&approved.template, TemplateStatus::Pending),
        (&approved.template, TemplateStatus::Approved),
        (&approved.template, TemplateStatus::Rejected),
        (&rejected.template, TemplateStatus::Approved),
        (&rejected.template, TemplateStatus::Revoked),
        (&pending.template, TemplateStatus::Revoked),
        (&pending.template, TemplateStatus::Pending),
        (&revoked.template, TemplateStatus::Approved),
        (&unknown, TemplateStatus::Approved),
    ];
    for (template, status) in refused {
        // A failed call may end its transaction: one transaction per attempt.
        let mut tx = store.begin().await.unwrap();
        assert!(
            matches!(
                tx.set_template_status(template, status, at, Some(9)).await,
                Err(DomainError::Store(_))
            ),
            "{template} -> {status:?}"
        );
    }

    let mut tx = store.begin().await.unwrap();
    let status = |stored: Option<StoredTemplate>| stored.map(|stored| stored.status);
    assert_eq!(
        status(tx.template(&pending.template).await.unwrap()),
        Some(TemplateStatus::Pending)
    );
    assert_eq!(
        status(tx.template(&approved.template).await.unwrap()),
        Some(TemplateStatus::Approved)
    );
    assert_eq!(
        status(tx.template(&rejected.template).await.unwrap()),
        Some(TemplateStatus::Rejected)
    );
    assert_eq!(
        status(tx.template(&revoked.template).await.unwrap()),
        Some(TemplateStatus::Revoked)
    );
}

async fn change_numbers_count_up_from_one<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let mut tx = store.begin().await.unwrap();
    assert_eq!(tx.last_template_change().await, Ok(0));
    assert_eq!(tx.next_template_change().await, Ok(1));
    assert_eq!(tx.next_template_change().await, Ok(2));
    assert_eq!(tx.last_template_change().await, Ok(2));
    tx.commit().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    assert_eq!(tx.next_template_change().await, Ok(3));
    tx.rollback().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    assert_eq!(tx.last_template_change().await, Ok(2));
    assert_eq!(tx.next_template_change().await, Ok(3));
    assert_eq!(tx.last_template_change().await, Ok(3));
}

async fn the_catalog_lists_approved_and_revoked_changes_in_order<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let user = UserId::new();
    add_users(&store, &[user]).await;
    let at = start() + TimeDelta::hours(1);
    let mut tx = store.begin().await.unwrap();
    // Approved as change 3, 1 and 4; one of them revoked later as change 5.
    for (reference, change) in [("acme.c@1", 3), ("acme.a@1", 1), ("acme.d@1", 4)] {
        let stored = template(reference, Some(user), start());
        submit(&mut tx, stored.clone()).await;
        tx.set_template_status(&stored.template, TemplateStatus::Approved, at, Some(change))
            .await
            .unwrap();
    }
    tx.set_template_status(
        &"acme.c@1".parse().unwrap(),
        TemplateStatus::Revoked,
        at,
        Some(5),
    )
    .await
    .unwrap();
    // Pending and rejected versions are never in the catalog.
    submit(&mut tx, template("acme.pending@1", Some(user), start())).await;
    let rejected = template("acme.rejected@1", Some(user), start());
    submit(&mut tx, rejected.clone()).await;
    tx.set_template_status(&rejected.template, TemplateStatus::Rejected, at, None)
        .await
        .unwrap();

    let changed = tx.templates_changed_since(0, 10).await.unwrap();
    assert_eq!(refs(&changed), ["acme.a@1", "acme.d@1", "acme.c@1"]);
    assert_eq!(changed[2].status, TemplateStatus::Revoked);
    assert_eq!(
        refs(&tx.templates_changed_since(1, 10).await.unwrap()),
        ["acme.d@1", "acme.c@1"]
    );
    assert_eq!(
        refs(&tx.templates_changed_since(0, 2).await.unwrap()),
        ["acme.a@1", "acme.d@1"]
    );
    assert!(tx.templates_changed_since(5, 10).await.unwrap().is_empty());
}

async fn versions_are_listed_by_status_newest_first<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let user = UserId::new();
    add_users(&store, &[user]).await;
    let mut tx = store.begin().await.unwrap();
    for (reference, minutes) in [("acme.old@1", 0), ("acme.new@1", 2), ("acme.mid@1", 1)] {
        let at = start() + TimeDelta::minutes(minutes);
        submit(&mut tx, template(reference, Some(user), at)).await;
    }
    tx.set_template_status(
        &"acme.mid@1".parse().unwrap(),
        TemplateStatus::Approved,
        start() + TimeDelta::hours(1),
        Some(1),
    )
    .await
    .unwrap();

    assert_eq!(
        refs(&tx.templates_with_status(None, 10).await.unwrap()),
        ["acme.new@1", "acme.mid@1", "acme.old@1"]
    );
    assert_eq!(
        refs(&tx.templates_with_status(None, 2).await.unwrap()),
        ["acme.new@1", "acme.mid@1"]
    );
    assert_eq!(
        refs(
            &tx.templates_with_status(Some(TemplateStatus::Pending), 10)
                .await
                .unwrap()
        ),
        ["acme.new@1", "acme.old@1"]
    );
    assert_eq!(
        refs(
            &tx.templates_with_status(Some(TemplateStatus::Approved), 10)
                .await
                .unwrap()
        ),
        ["acme.mid@1"]
    );
    assert!(
        tx.templates_with_status(Some(TemplateStatus::Revoked), 10)
            .await
            .unwrap()
            .is_empty()
    );
}

async fn only_pending_submissions_of_the_user_are_counted<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let (user, other) = (UserId::new(), UserId::new());
    add_users(&store, &[user, other]).await;
    let mut tx = store.begin().await.unwrap();
    submit(&mut tx, template("acme.a@1", Some(user), start())).await;
    submit(&mut tx, template("acme.a@2", Some(user), start())).await;
    submit(&mut tx, template("acme.a@3", Some(user), start())).await;
    submit(&mut tx, template("other.a@1", Some(other), start())).await;
    tx.set_template_status(
        &"acme.a@3".parse().unwrap(),
        TemplateStatus::Rejected,
        start(),
        None,
    )
    .await
    .unwrap();

    assert_eq!(tx.pending_templates_of(user).await, Ok(2));
    assert_eq!(tx.pending_templates_of(other).await, Ok(1));
    assert_eq!(tx.pending_templates_of(UserId::new()).await, Ok(0));
}

async fn the_first_claim_of_a_publisher_wins<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let (first, second) = (UserId::new(), UserId::new());
    add_users(&store, &[first, second]).await;
    let mut tx = store.begin().await.unwrap();
    assert_eq!(
        tx.claim_publisher("acme", PublisherOwner::User(first))
            .await,
        Ok(PublisherOwner::User(first))
    );
    assert_eq!(
        tx.claim_publisher("agent", PublisherOwner::Server).await,
        Ok(PublisherOwner::Server)
    );
    tx.commit().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    assert_eq!(
        tx.claim_publisher("acme", PublisherOwner::User(second))
            .await,
        Ok(PublisherOwner::User(first))
    );
    assert_eq!(
        tx.claim_publisher("acme", PublisherOwner::Server).await,
        Ok(PublisherOwner::User(first))
    );
    assert_eq!(
        tx.claim_publisher("agent", PublisherOwner::User(second))
            .await,
        Ok(PublisherOwner::Server)
    );
}

async fn a_publisher_claimed_by_an_unknown_user_is_refused<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let mut tx = store.begin().await.unwrap();
    assert!(matches!(
        tx.claim_publisher("acme", PublisherOwner::User(UserId::new()))
            .await,
        Err(DomainError::Store(_))
    ));
}

async fn the_audit_trail_is_kept_in_order<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let user = UserId::new();
    add_users(&store, &[user]).await;
    let stored = template("acme.deploy@1", Some(user), start());
    let other = template("acme.deploy@2", Some(user), start());
    let event = |template: &StoredTemplate, action, minutes| TemplateEvent {
        template: template.template.clone(),
        action,
        actor: None,
        reviewer: None,
        note: None,
        at: start() + TimeDelta::minutes(minutes),
    };
    let events = [
        TemplateEvent {
            actor: Some(user),
            ..event(&stored, TemplateAction::Submit, 0)
        },
        TemplateEvent {
            reviewer: Some("alice".into()),
            note: Some("fine".into()),
            ..event(&stored, TemplateAction::Approve, 1)
        },
        // Recorded at an earlier time, but later: the trail keeps the order of recording.
        TemplateEvent {
            reviewer: Some("bob".into()),
            ..event(&stored, TemplateAction::Revoke, -5)
        },
    ];
    let mut tx = store.begin().await.unwrap();
    submit(&mut tx, stored.clone()).await;
    submit(&mut tx, other.clone()).await;
    tx.insert_template_event(events[0].clone()).await.unwrap();
    tx.insert_template_event(event(&other, TemplateAction::Seed, 0))
        .await
        .unwrap();
    for later in &events[1..] {
        tx.insert_template_event(later.clone()).await.unwrap();
    }
    tx.commit().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    assert_eq!(tx.template_events(&stored.template).await.unwrap(), events);
    assert_eq!(
        tx.template_events(&other.template).await.unwrap(),
        [event(&other, TemplateAction::Seed, 0)]
    );
    assert!(
        tx.template_events(&"acme.deploy@3".parse().unwrap())
            .await
            .unwrap()
            .is_empty()
    );
}

async fn an_event_of_an_unknown_template_is_refused<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let mut tx = store.begin().await.unwrap();
    let event = TemplateEvent {
        template: "acme.deploy@1".parse().unwrap(),
        action: TemplateAction::Submit,
        actor: None,
        reviewer: None,
        note: None,
        at: start(),
    };
    assert!(matches!(
        tx.insert_template_event(event).await,
        Err(DomainError::Store(_))
    ));
}

async fn only_owned_agents_are_agents<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let (agent, owner) = (UserId::new(), UserId::new());
    add_users(&store, &[agent, owner]).await;
    let mut tx = store.begin().await.unwrap();
    assert_eq!(tx.is_agent(agent).await, Ok(false));
    tx.record_agent_owner(agent, owner).await.unwrap();
    tx.commit().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    assert_eq!(tx.is_agent(agent).await, Ok(true));
    assert_eq!(tx.is_agent(owner).await, Ok(false));
    assert_eq!(tx.is_agent(UserId::new()).await, Ok(false));
}

async fn template_writes_are_kept_only_on_commit<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let user = UserId::new();
    add_users(&store, &[user]).await;
    let stored = template("acme.deploy@1", Some(user), start());
    let submit_event = TemplateEvent {
        template: stored.template.clone(),
        action: TemplateAction::Submit,
        actor: Some(user),
        reviewer: None,
        note: None,
        at: start(),
    };

    let mut tx = store.begin().await.unwrap();
    submit(&mut tx, stored.clone()).await;
    tx.insert_template_event(submit_event.clone())
        .await
        .unwrap();
    tx.next_template_change().await.unwrap();
    // Seen by its own transaction only.
    assert_eq!(
        tx.template(&stored.template).await,
        Ok(Some(stored.clone()))
    );
    tx.rollback().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    assert_eq!(tx.template(&stored.template).await, Ok(None));
    assert!(
        tx.template_events(&stored.template)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(tx.last_template_change().await, Ok(0));
    // The rolled-back claim is gone too: another account can take the publisher.
    let other = UserId::new();
    tx.insert_user(other).await.unwrap();
    assert_eq!(
        tx.claim_publisher("acme", PublisherOwner::User(other))
            .await,
        Ok(PublisherOwner::User(other))
    );
    tx.rollback().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    submit(&mut tx, stored.clone()).await;
    tx.insert_template_event(submit_event.clone())
        .await
        .unwrap();
    tx.set_template_status(&stored.template, TemplateStatus::Approved, start(), Some(1))
        .await
        .unwrap();
    assert_eq!(tx.next_template_change().await, Ok(1));
    tx.commit().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    assert_eq!(
        tx.template(&stored.template)
            .await
            .unwrap()
            .map(|stored| stored.status),
        Some(TemplateStatus::Approved)
    );
    assert_eq!(
        tx.template_events(&stored.template).await.unwrap(),
        [submit_event]
    );
    assert_eq!(tx.last_template_change().await, Ok(1));
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
        a_stored_template_is_read_back_whole,
        a_stored_version_is_never_replaced,
        a_template_needs_a_claimed_publisher,
        reviews_set_the_status_the_time_and_the_change,
        only_the_one_way_status_changes_are_allowed,
        change_numbers_count_up_from_one,
        the_catalog_lists_approved_and_revoked_changes_in_order,
        versions_are_listed_by_status_newest_first,
        only_pending_submissions_of_the_user_are_counted,
        the_first_claim_of_a_publisher_wins,
        a_publisher_claimed_by_an_unknown_user_is_refused,
        the_audit_trail_is_kept_in_order,
        an_event_of_an_unknown_template_is_refused,
        only_owned_agents_are_agents,
        template_writes_are_kept_only_on_commit,
    ]
}
