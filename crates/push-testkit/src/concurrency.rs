//! Scenarios that run operations at the same time. They need a store whose transactions
//! really run concurrently: the in-memory store serialises every transaction, and its
//! store-wide lock would deadlock the scenarios that hold two transactions open. Only
//! `push-store` runs them.

use std::sync::Arc;
use std::time::Duration;

use chrono::TimeDelta;
use push_domain::templates::TemplateRef;
use push_domain::{
    ContactState, ContentType, DeviceId, DomainError, Event, MAX_PENDING_PER_USER, ReceiptUpdate,
    ReviewAction, ReviewCommand, SendReceipt, Store, TemplateStatus, Tx, UserId,
};
use tokio::task::JoinSet;

use crate::{Backend, Case, World, case_list, message, seqs, text};

/// Runs every future at the same time, each on its own task, and returns their results
/// in the order given.
async fn all<T: Send + 'static>(futures: Vec<impl Future<Output = T> + Send + 'static>) -> Vec<T> {
    let mut set = JoinSet::new();
    for (index, future) in futures.into_iter().enumerate() {
        set.spawn(async move { (index, future.await) });
    }
    let mut results: Vec<(usize, T)> = set.join_all().await;
    results.sort_by_key(|(index, _)| *index);
    results.into_iter().map(|(_, result)| result).collect()
}

async fn concurrent_sends_stay_gapless_and_ordered<B: Backend>(backend: &B) {
    let world = Arc::new(World::new(backend).await);
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;

    let sends = (0..20)
        .map(|index| {
            let world = Arc::clone(&world);
            let from = if index % 2 == 0 { alice } else { bob };
            async move {
                world
                    .svc
                    .send_text(text(&from, chat, &index.to_string()))
                    .await
            }
        })
        .collect();
    let receipts: Vec<SendReceipt> = all(sends).await.into_iter().map(Result::unwrap).collect();

    let mut stored: Vec<i64> = receipts.iter().map(|receipt| receipt.seq).collect();
    stored.sort();
    assert_eq!(stored, (1..=20).collect::<Vec<_>>());
    let synced = world.svc.sync(bob.device, 100).await.unwrap().entries;
    assert_eq!(seqs(&synced), (1..=20).collect::<Vec<_>>());
    for entry in synced {
        let receipt = receipts
            .iter()
            .find(|receipt| receipt.message_id == entry.message_id)
            .unwrap();
        assert_eq!(receipt.seq, entry.seq);
    }
}

async fn concurrent_retries_store_one_entry<B: Backend>(backend: &B) {
    let world = Arc::new(World::new(backend).await);
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let command = text(&alice, chat, "once");

    let retries = (0..8)
        .map(|_| {
            let (world, command) = (Arc::clone(&world), command.clone());
            async move { world.svc.send_text(command).await }
        })
        .collect();
    let receipts = all(retries).await;

    let first = receipts[0].clone().unwrap();
    assert!(receipts.iter().all(|receipt| *receipt == Ok(first.clone())));
    assert_eq!(first.seq, 1);
    let synced = world.svc.sync(bob.device, 100).await.unwrap().entries;
    assert_eq!(seqs(&synced), [1]);
    assert_eq!(world.send(&bob, chat, "next").await.seq, 2);
}

async fn concurrent_open_direct_calls_create_one_conversation<B: Backend>(backend: &B) {
    let world = Arc::new(World::new(backend).await);
    let (alice, bob) = (world.person().await, world.person().await);
    world.befriend(&alice, &bob).await;

    let opens = (0..8)
        .map(|index| {
            let world = Arc::clone(&world);
            let (me, peer) = if index % 2 == 0 {
                (alice.user, bob.user)
            } else {
                (bob.user, alice.user)
            };
            async move { world.svc.open_direct(me, peer).await }
        })
        .collect();
    let ids = all(opens).await;

    let first = ids[0].clone().unwrap();
    assert!(ids.iter().all(|id| *id == Ok(first)));
    let listed = world
        .svc
        .list_conversations(alice.user, None)
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
}

async fn a_concurrent_accept_never_undoes_a_block<B: Backend>(backend: &B) {
    let world = Arc::new(World::new(backend).await);
    for _ in 0..10 {
        let (alice, bob) = (world.person().await, world.person().await);
        world
            .svc
            .contact_request(alice.user, bob.user)
            .await
            .unwrap();

        let accept = {
            let world = Arc::clone(&world);
            async move { world.svc.contact_accept(bob.user, alice.user).await }
        };
        let block = {
            let world = Arc::clone(&world);
            async move { world.svc.contact_block(alice.user, bob.user).await }
        };
        let (accepted, blocked) = tokio::join!(tokio::spawn(accept), tokio::spawn(block));
        assert!(matches!(
            accepted.unwrap(),
            Ok(()) | Err(DomainError::NoPendingRequest)
        ));
        blocked.unwrap().unwrap();

        assert_eq!(
            world.svc.contact_state(alice.user, bob.user).await,
            Ok(Some(ContactState::Blocked))
        );
        assert_eq!(
            world.svc.contact_state(bob.user, alice.user).await,
            Ok(None)
        );
    }
}

async fn concurrent_redeems_never_exceed_the_use_limit<B: Backend>(backend: &B) {
    let world = Arc::new(World::new(backend).await);
    let creator = world.person().await;
    let invite = world
        .svc
        .create_invite(creator.user, TimeDelta::days(1), 3)
        .await
        .unwrap();
    let mut redeemers = Vec::new();
    for _ in 0..8 {
        redeemers.push(world.person().await);
    }

    let redeems = redeemers
        .iter()
        .map(|redeemer| {
            let (world, code, user) = (Arc::clone(&world), invite.code.clone(), redeemer.user);
            async move { world.svc.redeem_invite(user, &code).await }
        })
        .collect();
    let results = all(redeems).await;

    let redeemed = results.iter().filter(|result| result.is_ok()).count();
    assert_eq!(redeemed, 3);
    assert!(
        results
            .iter()
            .all(|result| matches!(result, Ok(_) | Err(DomainError::InviteNotFound)))
    );
    let contacts = world.svc.list_contacts(creator.user).await.unwrap();
    assert_eq!(contacts.len(), 3);
}

async fn concurrent_acks_by_different_members_produce_the_group_receipt<B: Backend>(backend: &B) {
    let world = Arc::new(World::new(backend).await);
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let group = world.group(&alice, &[&bob, &carol]).await;

    for _ in 0..10 {
        let seq = world.send(&alice, group, "news").await.seq;
        world.notifier.take();

        let acks = [bob, carol]
            .into_iter()
            .map(|member| {
                let world = Arc::clone(&world);
                async move { world.svc.ack(member.device, group, seq).await }
            })
            .collect();
        assert!(all(acks).await.into_iter().all(|moved| moved == Ok(true)));

        let delivered = world.notifier.take().into_iter().any(|event| {
            matches!(
                event,
                Event::Receipt {
                    receipt: ReceiptUpdate::Group {
                        all_delivered_seq,
                        ..
                    },
                    ..
                } if all_delivered_seq == seq
            )
        });
        assert!(delivered, "no group receipt reached seq {seq}");
    }
}

async fn concurrent_registrations_of_one_device_give_it_one_owner<B: Backend>(backend: &B) {
    let world = Arc::new(World::new(backend).await);
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let users = [alice.user, bob.user];
    let chats = [
        world.direct_chat(&alice, &carol).await,
        world.direct_chat(&bob, &carol).await,
    ];

    for _ in 0..10 {
        let device = DeviceId::new();
        let registrations = users
            .into_iter()
            .map(|user| {
                let world = Arc::clone(&world);
                async move { world.svc.register_device(device, user).await }
            })
            .collect();
        let results = all(registrations).await;

        let winner = results
            .iter()
            .position(Result::is_ok)
            .expect("one registration succeeds");
        let loser = 1 - winner;
        assert_eq!(results[loser], Err(DomainError::UnknownDevice));
        assert_eq!(world.identify(device, users[winner]).await, Ok(()));
        assert_eq!(
            world.identify(device, users[loser]).await,
            Err(DomainError::UnknownDevice)
        );
        assert!(world.position(device, chats[winner]).await.is_some());
        assert_eq!(world.position(device, chats[loser]).await, None);
    }
}

/// A device relayed while its user joins a conversation must still receive everything
/// from the join on, even when the relay repeats `device.registered` (at least once).
/// Without a lock on the user, each transaction misses the other's uncommitted row: the
/// join finds no new device, the registration no new membership, and the repeated
/// registration then starts the device at the end of the log, past entries it never
/// received.
async fn a_device_registered_during_a_join_misses_nothing<B: Backend>(backend: &B) {
    let world = Arc::new(World::new(backend).await);
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let group = world.group(&alice, &[&bob]).await;
    world.befriend(&alice, &carol).await;
    let phone = DeviceId::new();

    // An uncommitted position of Carol's first device makes the join wait in the middle:
    // after it has written Carol's member row and listed her devices, before it commits.
    let mut holder = world.store.begin().await.unwrap();
    holder
        .create_cursor_if_absent(carol.device, group, 0)
        .await
        .unwrap();
    let join = {
        let world = Arc::clone(&world);
        tokio::spawn(async move {
            world
                .svc
                .add_members(alice.user, group, &[carol.user])
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let registration = {
        let world = Arc::clone(&world);
        tokio::spawn(async move { world.svc.device_registered(phone, carol.user).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    holder.rollback().await.unwrap();
    join.await.unwrap().unwrap();
    registration.await.unwrap().unwrap();

    let sent = world.send(&alice, group, "after the join").await;
    // The worker relays at least once: the registration arrives again.
    world
        .svc
        .device_registered(phone, carol.user)
        .await
        .unwrap();
    let synced = world.svc.sync(phone, 100).await.unwrap().entries;
    assert!(
        synced.iter().any(|entry| entry.seq == sent.seq),
        "the new device skipped seq {}: it got {:?}",
        sent.seq,
        seqs(&synced)
    );
}

async fn a_deadlock_is_reported_as_a_retryable_conflict<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let [a, b, c, d] = [(); 4].map(|()| UserId::new());
    let mut first = store.begin().await.unwrap();
    let mut second = store.begin().await.unwrap();
    first.lock_contact_pair(a, b).await.unwrap();
    second.lock_contact_pair(c, d).await.unwrap();

    // Each waits for the other's lock. The database aborts one of them, which gives its
    // locks back when it rolls back, so the other then gets its lock.
    let take = |mut tx: <B::Store as Store>::Tx, x: UserId, y: UserId| async move {
        match tx.lock_contact_pair(x, y).await {
            Ok(()) => Ok(tx),
            Err(error) => {
                tx.rollback().await.unwrap();
                Err(error)
            }
        }
    };
    let (first, second) = tokio::join!(take(first, c, d), take(second, a, b));

    let errors: Vec<DomainError> = [&first, &second]
        .into_iter()
        .filter_map(|result| result.as_ref().err().cloned())
        .collect();
    assert_eq!(errors, [DomainError::Conflict]);
    assert!(errors[0].is_retryable());
    for tx in [first, second].into_iter().flatten() {
        tx.rollback().await.unwrap();
    }
}

/// A valid tpl/1 definition of `<publisher>.deploy@1`.
fn definition(publisher: &str) -> Vec<u8> {
    format!(
        r#"{{"format":"tpl/1","publisher":"{publisher}","name":"deploy","version":1,
        "title":"Deploy check","senders":["human"],
        "params":{{"text":{{"type":"string","max_len":200}}}},
        "layout":[{{"type":"text","param":"text"}}]}}"#
    )
    .into_bytes()
}

async fn concurrent_submissions_never_exceed_the_pending_cap<B: Backend>(backend: &B) {
    let world = Arc::new(World::new(backend).await);
    let alice = world.person().await;
    for index in 1..MAX_PENDING_PER_USER {
        world
            .svc
            .submit_template(alice.user, &definition(&format!("p{index}")))
            .await
            .unwrap();
    }

    let submissions = (0..8)
        .map(|index| {
            let world = Arc::clone(&world);
            async move {
                world
                    .svc
                    .submit_template(alice.user, &definition(&format!("q{index}")))
                    .await
            }
        })
        .collect();
    let results = all(submissions).await;

    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    for result in results.iter().filter(|result| result.is_err()) {
        assert_eq!(
            *result,
            Err(DomainError::TemplateConflict(
                "too many submissions are waiting for review"
            ))
        );
    }
    let pending = world
        .svc
        .list_templates(Some(TemplateStatus::Pending), 0)
        .await
        .unwrap();
    assert_eq!(pending.len(), MAX_PENDING_PER_USER as usize);
}

async fn concurrent_seeders_store_each_builtin_once<B: Backend>(backend: &B) {
    let world = Arc::new(World::new(backend).await);

    let seeders = (0..4)
        .map(|_| {
            let world = Arc::clone(&world);
            async move { world.svc.seed_builtin_templates().await }
        })
        .collect();
    let inserted: usize = all(seeders).await.into_iter().map(Result::unwrap).sum();

    assert_eq!(inserted, 4);
    let catalog = world.svc.template_catalog(0, 0).await.unwrap();
    assert_eq!(catalog.entries.len(), 4);
    assert_eq!(catalog.seq, 4);
}

const APPROVAL: &str = r#"{"tpl":"agent.approval@1","params":{"title":"Deploy v2","details":"Ship it to production?"},"fallback":"Deploy v2 to production?"}"#;

async fn concurrent_answers_to_a_card_answer_it_once<B: Backend>(backend: &B) {
    let world = Arc::new(World::new(backend).await);
    world.svc.seed_builtin_templates().await.unwrap();
    let (alice, bob) = (world.person().await, world.person().await);
    let agent = world.agent(&alice).await;
    world.befriend(&alice, &bob).await;
    let group = world
        .svc
        .create_group(alice.user, "Group", &[agent.user, bob.user])
        .await
        .unwrap();

    for _ in 0..5 {
        let card = world
            .svc
            .send_message(message(&agent, group, ContentType::Rich, APPROVAL))
            .await
            .unwrap();
        let body = format!(
            r#"{{"card":"{}","values":["approve"],"fallback":"Approved"}}"#,
            card.message_id
        );
        let answers = [alice, bob]
            .into_iter()
            .map(|member| {
                let (world, body) = (Arc::clone(&world), body.clone());
                async move {
                    world
                        .svc
                        .send_message(message(&member, group, ContentType::Answer, &body))
                        .await
                }
            })
            .collect();
        let results = all(answers).await;

        let answered: Vec<&SendReceipt> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
        assert_eq!(answered.len(), 1, "{results:?}");
        assert!(
            results.contains(&Err(DomainError::CardAnswered)),
            "{results:?}"
        );
        assert_eq!(
            world.entry(group, card.message_id).await.answer_seq,
            Some(answered[0].seq)
        );
    }
}

/// A send that has read a template holds it: a revocation waits until the send ends, so
/// it can never commit between the send's check and its message.
async fn a_revocation_waits_for_a_send_that_read_the_template<B: Backend>(backend: &B) {
    let world = Arc::new(World::new(backend).await);
    world.svc.seed_builtin_templates().await.unwrap();
    let template: TemplateRef = "agent.approval@1".parse().unwrap();

    let mut send = world.store.begin().await.unwrap();
    let read = send.template_for_send(&template).await.unwrap().unwrap();
    assert_eq!(read.status, TemplateStatus::Approved);
    let revoke = {
        let (world, template) = (Arc::clone(&world), template.clone());
        tokio::spawn(async move {
            world
                .svc
                .review_template(ReviewCommand {
                    template,
                    action: ReviewAction::Revoke,
                    reviewer: "ops".to_owned(),
                    note: None,
                })
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!revoke.is_finished(), "the revocation did not wait");
    send.commit().await.unwrap();
    revoke.await.unwrap().unwrap();

    let mut tx = world.store.begin().await.unwrap();
    let after = tx.template_for_send(&template).await.unwrap().unwrap();
    assert_eq!(after.status, TemplateStatus::Revoked);
    tx.rollback().await.unwrap();
}

/// The scenarios that need a store with concurrent transactions.
pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
        concurrent_sends_stay_gapless_and_ordered,
        concurrent_retries_store_one_entry,
        concurrent_open_direct_calls_create_one_conversation,
        a_concurrent_accept_never_undoes_a_block,
        concurrent_redeems_never_exceed_the_use_limit,
        concurrent_acks_by_different_members_produce_the_group_receipt,
        concurrent_registrations_of_one_device_give_it_one_owner,
        a_device_registered_during_a_join_misses_nothing,
        a_deadlock_is_reported_as_a_retryable_conflict,
        concurrent_submissions_never_exceed_the_pending_cap,
        concurrent_seeders_store_each_builtin_once,
        concurrent_answers_to_a_card_answer_it_once,
        a_revocation_waits_for_a_send_that_read_the_template,
    ]
    .into_iter()
    .map(|case| case.within("concurrency"))
    .collect()
}
