use crate::{Backend, Case, World, case_list, entries_for, seqs, system_events, text};
use push_domain::{
    Clock, ContentType, ConversationId, DomainError, EntryKind, MAX_GROUP_MEMBERS, Positions, Role,
    SystemEvent, UserId, ValidationError,
};

async fn a_new_group_starts_with_a_created_entry_seen_by_every_member<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
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

async fn the_creator_is_the_admin_and_everyone_sees_the_group_from_its_start<B: Backend>(
    backend: &B,
) {
    let world = World::new(backend).await;
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

async fn duplicates_and_the_creator_in_the_member_list_are_ignored<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
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

async fn a_group_title_must_not_be_blank<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    assert_eq!(
        world.svc.create_group(alice.user, "   ", &[]).await,
        Err(DomainError::Invalid(ValidationError::EmptyTitle))
    );
}

async fn every_member_must_be_an_accepted_contact_of_the_creator<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
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

async fn a_group_holds_at_most_a_hundred_members<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
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

async fn members_chat_without_being_contacts_and_blocks_do_not_matter<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
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

async fn an_admin_adds_a_contact_who_sees_the_group_from_the_added_entry<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob, dave) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let group = world.group(&alice, &[&bob]).await;
    world.send(&alice, group, "before").await;
    world.befriend(&alice, &dave).await;
    world.notifier.take();

    world
        .svc
        .add_members(alice.user, group, &[dave.user])
        .await
        .unwrap();
    assert_eq!(world.notifier.take(), [entries_for(&[&alice, &bob, &dave])]);
    let batch = world.svc.sync(dave.device, 100).await.unwrap();
    assert_eq!(seqs(&batch.entries), [3]);
    assert_eq!(
        system_events(&batch.entries),
        [Some(SystemEvent::MembersAdded {
            actor: alice.user,
            users: vec![dave.user],
        })]
    );
    let history = world.svc.history(dave.user, group, None, 10).await.unwrap();
    assert_eq!(seqs(&history.entries), [3]);
    let row = world.member(group, &dave).await.unwrap();
    assert_eq!(
        (row.role, row.visible_from_seq, row.read_seq),
        (Role::Member, 3, 2)
    );
    assert_eq!(world.position(dave.device, group).await, Some(2));
}

async fn messages_from_before_joining_are_neither_unread_nor_changed_for_the_new_member<
    B: Backend,
>(
    backend: &B,
) {
    let world = World::new(backend).await;
    let (alice, bob, dave) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let group = world.group(&alice, &[&bob]).await;
    let before = world.send(&alice, group, "before").await;
    world.befriend(&alice, &dave).await;
    world
        .svc
        .add_members(alice.user, group, &[dave.user])
        .await
        .unwrap();
    world.edit(&alice, group, &before, "before, edited").await;
    world.send(&alice, group, "after").await;

    let listed = world.svc.list_conversations(dave.user, None).await.unwrap();
    assert_eq!((listed[0].last_seq, listed[0].unread), (5, 1));
    let batch = world.svc.sync(dave.device, 100).await.unwrap();
    assert_eq!(seqs(&batch.entries), [3, 5]);
}

async fn only_a_current_admin_of_a_group_can_add<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let group = world.group(&alice, &[&bob]).await;
    let chat = world.direct_chat(&alice, &carol).await;
    world.befriend(&bob, &carol).await;

    let add = |by: UserId, conversation| {
        let svc = &world.svc;
        async move { svc.add_members(by, conversation, &[carol.user]).await }
    };
    assert_eq!(add(bob.user, group).await, Err(DomainError::NotAdmin));
    assert_eq!(add(carol.user, group).await, Err(DomainError::NotMember));
    assert_eq!(add(alice.user, chat).await, Err(DomainError::NotAGroup));
    assert_eq!(
        add(alice.user, ConversationId::new()).await,
        Err(DomainError::ConversationNotFound)
    );
}

async fn added_users_must_be_contacts_of_the_admin_and_current_members_are_skipped<B: Backend>(
    backend: &B,
) {
    let world = World::new(backend).await;
    let (alice, bob, erin) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let group = world.group(&alice, &[&bob]).await;
    world.notifier.take();

    assert_eq!(
        world.svc.add_members(alice.user, group, &[erin.user]).await,
        Err(DomainError::NotContact)
    );
    assert_eq!(
        world
            .svc
            .add_members(alice.user, group, &[bob.user, alice.user])
            .await,
        Ok(())
    );
    assert!(world.notifier.take().is_empty());
    assert_eq!(world.conversation(group).await.last_seq, 1);
}

async fn adding_respects_the_hundred_member_cap<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    let mut friends = Vec::new();
    for _ in 0..MAX_GROUP_MEMBERS {
        let friend = world.person().await;
        world.befriend(&alice, &friend).await;
        friends.push(friend.user);
    }
    let (last, first) = friends.split_last().unwrap();
    let group = world
        .svc
        .create_group(alice.user, "Almost full", &first[1..])
        .await
        .unwrap();

    assert_eq!(
        world
            .svc
            .add_members(alice.user, group, &[first[0], *last])
            .await,
        Err(DomainError::GroupFull)
    );
    assert_eq!(
        world.svc.add_members(alice.user, group, &[*last]).await,
        Ok(())
    );
}

async fn a_device_registered_later_starts_at_the_end_of_the_group<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    let group = world.group(&alice, &[&bob]).await;
    world.send(&alice, group, "one").await;
    world.send(&alice, group, "two").await;

    let tablet = world.add_device(&bob).await;
    assert_eq!(world.position(tablet, group).await, Some(3));
    assert!(
        world
            .svc
            .sync(tablet, 100)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
}

async fn adding_members_does_not_lower_all_read<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob, dave) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let group = world.group(&alice, &[&bob]).await;
    world.send(&alice, group, "one").await;
    world.send(&alice, group, "two").await;
    world.svc.read_up_to(bob.user, group, 2).await.unwrap();
    let before = world
        .svc
        .conversation_state(alice.user, group)
        .await
        .unwrap();
    assert_eq!(
        before.positions,
        Positions::Group {
            all_delivered_seq: 2,
            all_read_seq: 2,
        }
    );

    world.befriend(&alice, &dave).await;
    world
        .svc
        .add_members(alice.user, group, &[dave.user])
        .await
        .unwrap();
    let after = world
        .svc
        .conversation_state(alice.user, group)
        .await
        .unwrap();
    assert_eq!(after.positions, before.positions);
}

async fn a_full_group_rejection_consumes_no_seq<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    let mut friends = Vec::new();
    for _ in 0..MAX_GROUP_MEMBERS {
        let friend = world.person().await;
        world.befriend(&alice, &friend).await;
        friends.push(friend.user);
    }
    let (last, first) = friends.split_last().unwrap();
    let group = world
        .svc
        .create_group(alice.user, "Almost full", &first[1..])
        .await
        .unwrap();
    let last_seq = world.conversation(group).await.last_seq;

    assert_eq!(
        world
            .svc
            .add_members(alice.user, group, &[first[0], *last])
            .await,
        Err(DomainError::GroupFull)
    );
    assert_eq!(world.conversation(group).await.last_seq, last_seq);
}

async fn duplicate_ids_in_add_are_added_once<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob, dave) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let group = world.group(&alice, &[&bob]).await;
    world.befriend(&alice, &dave).await;

    world
        .svc
        .add_members(alice.user, group, &[dave.user, dave.user])
        .await
        .unwrap();
    assert_eq!(world.conversation(group).await.last_seq, 2);
    let batch = world.svc.sync(dave.device, 100).await.unwrap();
    assert_eq!(
        system_events(&batch.entries),
        [Some(SystemEvent::MembersAdded {
            actor: alice.user,
            users: vec![dave.user],
        })]
    );
}

async fn read_up_to_below_the_floor_changes_nothing<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob, dave) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let group = world.group(&alice, &[&bob]).await;
    world.send(&alice, group, "one").await;
    world.send(&alice, group, "two").await;
    world.befriend(&alice, &dave).await;
    world
        .svc
        .add_members(alice.user, group, &[dave.user])
        .await
        .unwrap();
    assert_eq!(world.member(group, &dave).await.unwrap().read_seq, 3);
    world.notifier.take();

    assert_eq!(world.svc.read_up_to(dave.user, group, 1).await, Ok(false));
    assert!(world.notifier.take().is_empty());
    assert_eq!(world.member(group, &dave).await.unwrap().read_seq, 3);
}

async fn a_removed_member_can_rejoin_and_sees_only_new_history<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob, dave) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let group = world.group(&alice, &[&bob, &dave]).await;
    world.send(&alice, group, "one").await;
    world.svc.read_up_to(dave.user, group, 2).await.unwrap();
    world
        .svc
        .remove_member(alice.user, group, dave.user)
        .await
        .unwrap();
    world.send(&alice, group, "while away").await;
    world
        .svc
        .add_members(alice.user, group, &[dave.user])
        .await
        .unwrap();
    world.send(&alice, group, "back").await;

    let batch = world.svc.sync(dave.device, 100).await.unwrap();
    assert_eq!(seqs(&batch.entries), [5, 6]);
    assert_eq!(
        system_events(&batch.entries[..1]),
        [Some(SystemEvent::MembersAdded {
            actor: alice.user,
            users: vec![dave.user],
        })]
    );
    let history = world.svc.history(dave.user, group, None, 10).await.unwrap();
    assert_eq!(seqs(&history.entries), [5, 6]);
    let row = world.member(group, &dave).await.unwrap();
    assert_eq!((row.visible_from_seq, row.left_at_seq), (5, None));
    assert!(row.read_seq >= 2);
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
        a_new_group_starts_with_a_created_entry_seen_by_every_member,
        the_creator_is_the_admin_and_everyone_sees_the_group_from_its_start,
        duplicates_and_the_creator_in_the_member_list_are_ignored,
        a_group_title_must_not_be_blank,
        every_member_must_be_an_accepted_contact_of_the_creator,
        a_group_holds_at_most_a_hundred_members,
        members_chat_without_being_contacts_and_blocks_do_not_matter,
        an_admin_adds_a_contact_who_sees_the_group_from_the_added_entry,
        messages_from_before_joining_are_neither_unread_nor_changed_for_the_new_member,
        only_a_current_admin_of_a_group_can_add,
        added_users_must_be_contacts_of_the_admin_and_current_members_are_skipped,
        adding_respects_the_hundred_member_cap,
        a_device_registered_later_starts_at_the_end_of_the_group,
        adding_members_does_not_lower_all_read,
        a_full_group_rejection_consumes_no_seq,
        duplicate_ids_in_add_are_added_once,
        read_up_to_below_the_floor_changes_nothing,
        a_removed_member_can_rejoin_and_sees_only_new_history,
    ]
}
