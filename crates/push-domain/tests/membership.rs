mod common;

use chrono::TimeDelta;
use common::{Person, World, entries_for, seqs, system_events, text};
use push_domain::{
    Clock, ConversationId, DomainError, Event, ReceiptUpdate, Role, SystemEvent, ValidationError,
};

/// A group of alice (admin), bob and carol; `seq` 1 is its `Created` entry.
async fn trio() -> (World, Person, Person, Person, ConversationId) {
    let world = World::new();
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let group = world.group(&alice, &[&bob, &carol]).await;
    world.notifier.take();
    (world, alice, bob, carol, group)
}

#[tokio::test]
async fn a_removed_member_receives_nothing_after_the_removal_entry() {
    let (world, alice, bob, carol, group) = trio().await;
    world.send(&alice, group, "one").await;
    world.notifier.take();

    world
        .svc
        .remove_member(alice.user, group, bob.user)
        .await
        .unwrap();
    assert_eq!(
        world.notifier.take(),
        [entries_for(&[&alice, &bob, &carol])]
    );
    world.send(&alice, group, "after").await;
    assert_eq!(world.notifier.take(), [entries_for(&[&alice, &carol])]);

    let batch = world.svc.sync(bob.device, 100).await.unwrap();
    assert_eq!(seqs(&batch.entries), [1, 2, 3]);
    assert_eq!(
        system_events(&batch.entries[2..]),
        [Some(SystemEvent::MemberRemoved {
            actor: alice.user,
            user: bob.user,
        })]
    );
    let history = world.svc.history(bob.user, group, None, 10).await.unwrap();
    assert_eq!(seqs(&history.entries), [1, 2, 3]);
    assert_eq!(
        world.svc.send_text(text(&bob, group, "hello?")).await,
        Err(DomainError::NotMember)
    );
    assert_eq!(
        world.svc.ack(bob.device, group, 4).await,
        Err(DomainError::SeqOutOfRange)
    );
    assert_eq!(world.svc.read_up_to(bob.user, group, 3).await, Ok(true));
}

#[tokio::test]
async fn removing_the_member_who_held_the_group_back_completes_the_read() {
    let (world, alice, bob, carol, group) = trio().await;
    world.send(&alice, group, "one").await;
    world.svc.read_up_to(bob.user, group, 2).await.unwrap();
    world.notifier.take();

    world
        .svc
        .remove_member(alice.user, group, carol.user)
        .await
        .unwrap();
    let mut to = vec![alice.user, bob.user];
    to.sort();
    assert_eq!(
        world.notifier.take(),
        [
            entries_for(&[&alice, &bob, &carol]),
            Event::Receipt {
                conversation: group,
                to,
                receipt: ReceiptUpdate::Group {
                    all_delivered_seq: 3,
                    all_read_seq: 3,
                },
            }
        ]
    );
}

#[tokio::test]
async fn only_an_admin_removes_others_and_removing_twice_changes_nothing() {
    let (world, alice, bob, carol, group) = trio().await;
    let dave = world.person().await;

    assert_eq!(
        world.svc.remove_member(bob.user, group, carol.user).await,
        Err(DomainError::NotAdmin)
    );
    assert_eq!(
        world.svc.remove_member(alice.user, group, alice.user).await,
        Err(DomainError::Invalid(ValidationError::SelfTarget))
    );
    assert_eq!(
        world.svc.remove_member(alice.user, group, dave.user).await,
        Err(DomainError::NotMember)
    );
    world
        .svc
        .remove_member(alice.user, group, carol.user)
        .await
        .unwrap();
    world.notifier.take();

    assert_eq!(
        world.svc.remove_member(alice.user, group, carol.user).await,
        Ok(())
    );
    assert!(world.notifier.take().is_empty());
    assert_eq!(world.conversation(group).await.last_seq, 2);
}

#[tokio::test]
async fn a_member_leaves_and_leaving_twice_changes_nothing() {
    let (world, alice, bob, carol, group) = trio().await;

    world.svc.leave_group(bob.user, group).await.unwrap();
    assert_eq!(
        world.notifier.take(),
        [entries_for(&[&alice, &bob, &carol])]
    );
    let batch = world.svc.sync(bob.device, 100).await.unwrap();
    assert_eq!(
        system_events(&batch.entries),
        [
            Some(SystemEvent::Created {
                actor: alice.user,
                title: "Group".into(),
                users: {
                    let mut users = vec![bob.user, carol.user];
                    users.sort();
                    users
                },
            }),
            Some(SystemEvent::MemberLeft { actor: bob.user })
        ]
    );
    assert_eq!(
        world.member(group, &bob).await.unwrap().left_at_seq,
        Some(2)
    );

    assert_eq!(world.svc.leave_group(bob.user, group).await, Ok(()));
    assert!(world.notifier.take().is_empty());
    let stranger = world.person().await;
    assert_eq!(
        world.svc.leave_group(stranger.user, group).await,
        Err(DomainError::NotMember)
    );
}

#[tokio::test]
async fn when_the_last_admin_leaves_the_longest_standing_member_is_promoted() {
    let world = World::new();
    let alice = world.person().await;
    // Dave is created first, so he has the lower user id but joins later than bob.
    let dave = world.person().await;
    let bob = world.person().await;
    assert!(dave.user < bob.user);
    let group = world.group(&alice, &[&bob]).await;
    world.befriend(&alice, &dave).await;
    world
        .svc
        .add_members(alice.user, group, &[dave.user])
        .await
        .unwrap();

    world.svc.leave_group(alice.user, group).await.unwrap();
    let batch = world.svc.sync(dave.device, 100).await.unwrap();
    assert_eq!(
        system_events(&batch.entries),
        [
            Some(SystemEvent::MembersAdded {
                actor: alice.user,
                users: vec![dave.user],
            }),
            Some(SystemEvent::MemberLeft { actor: alice.user }),
            Some(SystemEvent::AdminPromoted {
                actor: alice.user,
                user: bob.user,
            })
        ]
    );
    assert_eq!(batch.entries[2].sender_device_id, None);
    assert_eq!(world.member(group, &bob).await.unwrap().role, Role::Admin);
    let alices = world.svc.sync(alice.device, 100).await.unwrap();
    assert_eq!(seqs(&alices.entries), [1, 2, 3]);
}

#[tokio::test]
async fn among_members_who_joined_together_the_lowest_user_id_is_promoted() {
    let (world, alice, bob, carol, group) = trio().await;

    world.svc.leave_group(alice.user, group).await.unwrap();
    let heir = if bob.user < carol.user { &bob } else { &carol };
    assert_eq!(world.member(group, heir).await.unwrap().role, Role::Admin);
}

#[tokio::test]
async fn an_admin_leaving_while_another_admin_remains_promotes_nobody() {
    let (world, alice, bob, carol, group) = trio().await;
    world
        .svc
        .set_role(alice.user, group, bob.user, Role::Admin)
        .await
        .unwrap();

    world.svc.leave_group(alice.user, group).await.unwrap();
    let batch = world.svc.sync(carol.device, 100).await.unwrap();
    let events = system_events(&batch.entries);
    assert_eq!(events.len(), 3);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Some(SystemEvent::AdminPromoted { .. })))
    );
    assert_eq!(world.member(group, &bob).await.unwrap().role, Role::Admin);
    assert_eq!(
        world.member(group, &carol).await.unwrap().role,
        Role::Member
    );
}

#[tokio::test]
async fn leaving_a_direct_conversation_is_refused() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let direct = world.direct_chat(&alice, &bob).await;

    assert_eq!(
        world.svc.leave_group(alice.user, direct).await,
        Err(DomainError::NotAGroup)
    );
}

#[tokio::test]
async fn removing_from_a_direct_conversation_is_refused() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let direct = world.direct_chat(&alice, &bob).await;

    assert_eq!(
        world.svc.remove_member(alice.user, direct, bob.user).await,
        Err(DomainError::NotAGroup)
    );
}

#[tokio::test]
async fn a_group_whose_last_member_left_is_closed() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let group = world.group(&alice, &[&bob]).await;
    world.svc.leave_group(bob.user, group).await.unwrap();

    world.svc.leave_group(alice.user, group).await.unwrap();
    assert_eq!(world.conversation(group).await.last_seq, 3);
    assert_eq!(
        world.svc.send_text(text(&alice, group, "anyone?")).await,
        Err(DomainError::NotMember),
        "a group whose last member left is closed"
    );
}

#[tokio::test]
async fn a_rejoining_member_sees_only_the_new_range_and_old_acks_change_nothing() {
    let (world, alice, bob, _carol, group) = trio().await;
    world.send(&alice, group, "one").await;
    world.send(&alice, group, "two").await;
    world.svc.ack(bob.device, group, 3).await.unwrap();
    world
        .svc
        .remove_member(alice.user, group, bob.user)
        .await
        .unwrap();
    world.send(&alice, group, "while away").await;
    world.send(&alice, group, "still away").await;
    world
        .svc
        .add_members(alice.user, group, &[bob.user])
        .await
        .unwrap();

    let row = world.member(group, &bob).await.unwrap();
    assert_eq!(
        (row.visible_from_seq, row.left_at_seq, row.read_seq),
        (7, None, 6)
    );
    assert_eq!(world.position(bob.device, group).await, Some(3));
    assert_eq!(world.svc.ack(bob.device, group, 5).await, Ok(false));
    assert_eq!(world.position(bob.device, group).await, Some(3));
    let batch = world.svc.sync(bob.device, 100).await.unwrap();
    assert_eq!(seqs(&batch.entries), [7]);
    let history = world.svc.history(bob.user, group, None, 10).await.unwrap();
    assert_eq!(seqs(&history.entries), [7]);
}

#[tokio::test]
async fn a_former_member_lists_the_group_as_it_was_when_they_left() {
    let (world, alice, bob, _carol, group) = trio().await;
    world.clock.advance(TimeDelta::minutes(1));
    world.svc.leave_group(bob.user, group).await.unwrap();
    let left_at = world.clock.now();
    world.clock.advance(TimeDelta::minutes(1));
    world.send(&alice, group, "after bob left").await;

    let listed = world.svc.list_conversations(bob.user, None).await.unwrap();
    assert_eq!(listed.len(), 1);
    let bobs = &listed[0];
    assert_eq!((bobs.last_seq, bobs.updated_at), (2, left_at));
    assert!(bobs.members.is_empty() && bobs.admins.is_empty());
    assert!(
        world
            .svc
            .list_conversations(bob.user, Some(left_at))
            .await
            .unwrap()
            .is_empty()
    );
}
