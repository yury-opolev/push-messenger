mod common;

use chrono::TimeDelta;
use common::{Person, World, entries_for, seqs, system_events};
use push_domain::{Clock, ConversationId, DomainError, Role, SystemEvent, ValidationError};

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
async fn an_admin_renames_the_group() {
    let (world, alice, bob, carol, group) = trio().await;
    world.clock.advance(TimeDelta::minutes(1));

    world
        .svc
        .rename_group(alice.user, group, "  Plans  ")
        .await
        .unwrap();
    assert_eq!(
        world.notifier.take(),
        [entries_for(&[&alice, &bob, &carol])]
    );
    let stored = world.conversation(group).await;
    assert_eq!(stored.title.as_deref(), Some("Plans"));
    assert_eq!((stored.last_seq, stored.updated_at), (2, world.clock.now()));
    let batch = world.svc.sync(bob.device, 100).await.unwrap();
    assert_eq!(
        system_events(&batch.entries[1..]),
        [Some(SystemEvent::Renamed {
            actor: alice.user,
            title: "Plans".into(),
        })]
    );
}

#[tokio::test]
async fn renaming_to_the_same_title_changes_nothing_and_members_cannot_rename() {
    let (world, alice, bob, _carol, group) = trio().await;

    assert_eq!(
        world.svc.rename_group(alice.user, group, "Group").await,
        Ok(())
    );
    assert!(world.notifier.take().is_empty());
    assert_eq!(world.conversation(group).await.last_seq, 1);
    assert_eq!(
        world.svc.rename_group(bob.user, group, "Mine").await,
        Err(DomainError::NotAdmin)
    );
    assert_eq!(
        world.svc.rename_group(alice.user, group, " ").await,
        Err(DomainError::Invalid(ValidationError::EmptyTitle))
    );
}

#[tokio::test]
async fn a_promoted_member_can_act_as_admin_and_demote_others() {
    let (world, alice, bob, carol, group) = trio().await;

    world
        .svc
        .set_role(alice.user, group, bob.user, Role::Admin)
        .await
        .unwrap();
    assert_eq!(
        world.notifier.take(),
        [entries_for(&[&alice, &bob, &carol])]
    );
    world
        .svc
        .set_role(bob.user, group, alice.user, Role::Member)
        .await
        .unwrap();
    assert_eq!(
        world.svc.rename_group(alice.user, group, "Mine").await,
        Err(DomainError::NotAdmin)
    );
    let batch = world.svc.sync(carol.device, 100).await.unwrap();
    assert_eq!(
        system_events(&batch.entries[1..]),
        [
            Some(SystemEvent::RoleChanged {
                actor: alice.user,
                user: bob.user,
                role: Role::Admin,
            }),
            Some(SystemEvent::RoleChanged {
                actor: bob.user,
                user: alice.user,
                role: Role::Member,
            })
        ]
    );
}

#[tokio::test]
async fn the_last_admin_cannot_be_demoted() {
    let (world, alice, bob, _carol, group) = trio().await;

    assert_eq!(
        world
            .svc
            .set_role(alice.user, group, alice.user, Role::Member)
            .await,
        Err(DomainError::LastAdmin)
    );
    assert_eq!(
        world
            .svc
            .set_role(alice.user, group, bob.user, Role::Member)
            .await,
        Ok(())
    );
    assert!(world.notifier.take().is_empty());
    assert_eq!(world.conversation(group).await.last_seq, 1);
}

#[tokio::test]
async fn only_current_members_get_a_role() {
    let (world, alice, bob, _carol, group) = trio().await;
    world.svc.leave_group(bob.user, group).await.unwrap();

    assert_eq!(
        world
            .svc
            .set_role(alice.user, group, bob.user, Role::Admin)
            .await,
        Err(DomainError::NotMember)
    );
}

#[tokio::test]
async fn no_one_is_promoted_when_an_admin_leaves_and_another_remains() {
    let (world, alice, bob, carol, group) = trio().await;
    world
        .svc
        .set_role(alice.user, group, bob.user, Role::Admin)
        .await
        .unwrap();

    world.svc.leave_group(alice.user, group).await.unwrap();
    let batch = world.svc.sync(carol.device, 100).await.unwrap();
    assert_eq!(seqs(&batch.entries), [1, 2, 3]);
    assert_eq!(
        world.member(group, &carol).await.unwrap().role,
        Role::Member
    );
}
