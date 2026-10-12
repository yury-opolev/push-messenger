mod common;

use common::{World, entries_for, seqs, system_events, text};
use push_domain::{
    Clock, ContentType, DomainError, EntryKind, MAX_GROUP_MEMBERS, Role, SystemEvent, UserId,
    ValidationError,
};

#[tokio::test]
async fn a_new_group_starts_with_a_created_entry_seen_by_every_member() {
    let world = World::new();
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    world.befriend(&alice, &bob).await;
    world.befriend(&alice, &carol).await;
    world.notifier.take();

    let group = world
        .svc
        .create_group(alice.user, "  Trip  ", &[carol.user, bob.user])
        .await
        .unwrap();
    assert_eq!(
        world.notifier.take(),
        [entries_for(&[&alice, &bob, &carol])]
    );
    let mut others = vec![bob.user, carol.user];
    others.sort();
    for person in [&alice, &bob, &carol] {
        let batch = world.svc.sync(person.device, 100).await.unwrap();
        assert_eq!(seqs(&batch.entries), [1]);
        let entry = &batch.entries[0];
        assert_eq!(
            (entry.kind, entry.content_type, entry.sender_id),
            (EntryKind::System, ContentType::SystemEvent, alice.user)
        );
        assert_eq!(entry.sender_device_id, None);
        assert_eq!(
            system_events(&batch.entries),
            [Some(SystemEvent::Created {
                actor: alice.user,
                title: "Trip".into(),
                users: others.clone(),
            })]
        );
    }
    let stored = world.conversation(group).await;
    assert_eq!(stored.title.as_deref(), Some("Trip"));
    assert_eq!((stored.last_seq, stored.updated_at), (1, world.clock.now()));
}

#[tokio::test]
async fn the_creator_is_the_admin_and_everyone_sees_the_group_from_its_start() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let tablet = world.add_device(&bob).await;
    let group = world.group(&alice, &[&bob]).await;

    let admin = world.member(group, &alice).await.unwrap();
    let member = world.member(group, &bob).await.unwrap();
    assert_eq!((admin.role, member.role), (Role::Admin, Role::Member));
    for row in [admin, member] {
        assert_eq!(
            (row.visible_from_seq, row.left_at_seq, row.read_seq),
            (1, None, 0)
        );
    }
    for device in [alice.device, bob.device, tablet] {
        assert_eq!(world.position(device, group).await, Some(0));
    }
}

#[tokio::test]
async fn duplicates_and_the_creator_in_the_member_list_are_ignored() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    world.befriend(&alice, &bob).await;

    let group = world
        .svc
        .create_group(alice.user, "Pair", &[bob.user, alice.user, bob.user])
        .await
        .unwrap();
    let batch = world.svc.sync(bob.device, 100).await.unwrap();
    assert_eq!(
        system_events(&batch.entries),
        [Some(SystemEvent::Created {
            actor: alice.user,
            title: "Pair".into(),
            users: vec![bob.user],
        })]
    );
    assert!(world.member(group, &bob).await.is_some());
}

#[tokio::test]
async fn a_group_title_must_not_be_blank() {
    let world = World::new();
    let alice = world.person().await;
    assert_eq!(
        world.svc.create_group(alice.user, "   ", &[]).await,
        Err(DomainError::Invalid(ValidationError::EmptyTitle))
    );
}

#[tokio::test]
async fn every_member_must_be_an_accepted_contact_of_the_creator() {
    let world = World::new();
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    world.befriend(&alice, &bob).await;
    world
        .svc
        .contact_request(alice.user, carol.user)
        .await
        .unwrap();

    assert_eq!(
        world
            .svc
            .create_group(alice.user, "Trip", &[bob.user, carol.user])
            .await,
        Err(DomainError::NotContact)
    );
    assert!(
        world
            .svc
            .sync(bob.device, 100)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
}

#[tokio::test]
async fn a_group_holds_at_most_a_hundred_members() {
    let world = World::new();
    let alice = world.person().await;
    let mut friends = Vec::new();
    for _ in 1..MAX_GROUP_MEMBERS {
        let friend = world.person().await;
        world.befriend(&alice, &friend).await;
        friends.push(friend.user);
    }

    assert!(
        world
            .svc
            .create_group(alice.user, "Full", &friends)
            .await
            .is_ok()
    );
    friends.push(UserId::new());
    assert_eq!(
        world
            .svc
            .create_group(alice.user, "Too many", &friends)
            .await,
        Err(DomainError::GroupFull)
    );
}

#[tokio::test]
async fn members_chat_without_being_contacts_and_blocks_do_not_matter() {
    let world = World::new();
    let (alice, bob, carol, dave) = (
        world.person().await,
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let group = world.group(&alice, &[&bob, &carol]).await;
    world.svc.contact_block(bob.user, carol.user).await.unwrap();

    world.send(&carol, group, "hi bob").await;
    world.send(&bob, group, "hi carol").await;
    assert_eq!(
        world.svc.send_text(text(&dave, group, "let me in")).await,
        Err(DomainError::NotMember)
    );
    let history = world.svc.history(dave.user, group, None, 10).await;
    assert_eq!(history, Err(DomainError::NotMember));
}
