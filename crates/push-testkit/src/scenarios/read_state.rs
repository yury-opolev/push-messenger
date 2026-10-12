use crate::{Backend, Case, World, case_list};
use push_domain::{ConversationState, DomainError, MemberState, Positions, UserId};

fn member(state: &ConversationState, user: UserId) -> &MemberState {
    let Positions::Members(members) = &state.positions else {
        panic!("a direct conversation lists its members' positions");
    };
    members
        .iter()
        .find(|member| member.user_id == user)
        .expect("member present")
}

async fn the_read_position_only_moves_forward<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    for body in ["one", "two", "three"] {
        world.send(&alice, chat, body).await;
    }

    assert_eq!(world.svc.read_up_to(bob.user, chat, 2).await, Ok(true));
    assert_eq!(world.svc.read_up_to(bob.user, chat, 1).await, Ok(false));
    let state = world
        .svc
        .conversation_state(alice.user, chat)
        .await
        .unwrap();
    assert_eq!(member(&state, bob.user).read_seq, 2);
}

async fn reading_beyond_the_last_message_is_rejected<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&alice, chat, "one").await;

    assert_eq!(
        world.svc.read_up_to(bob.user, chat, 9).await,
        Err(DomainError::SeqOutOfRange)
    );
}

async fn sending_marks_received_messages_read<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&bob, chat, "one").await;
    world.svc.ack(alice.device, chat, 1).await.unwrap();
    world.send(&alice, chat, "two").await;

    let state = world
        .svc
        .conversation_state(alice.user, chat)
        .await
        .unwrap();
    assert_eq!(state.last_seq, 2);
    assert_eq!(member(&state, alice.user).read_seq, 1);
    assert_eq!(member(&state, bob.user).read_seq, 0);
}

async fn sending_does_not_mark_unreceived_messages_read<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&bob, chat, "one").await;
    world.send(&alice, chat, "two").await;

    let state = world.svc.conversation_state(bob.user, chat).await.unwrap();
    assert_eq!(member(&state, alice.user).read_seq, 0);
    assert_eq!(member(&state, alice.user).delivered_seq, 0);
}

async fn sending_does_not_mark_anything_delivered<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&bob, chat, "one").await;
    world.send(&alice, chat, "two").await;

    let state = world.svc.conversation_state(bob.user, chat).await.unwrap();
    assert_eq!(member(&state, alice.user).delivered_seq, 0);
    assert_eq!(member(&state, bob.user).delivered_seq, 0);
}

async fn delivered_is_the_furthest_position_across_a_users_devices<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let tablet = world.add_device(&bob).await;
    for body in ["one", "two", "three"] {
        world.send(&alice, chat, body).await;
    }

    world.svc.ack(bob.device, chat, 1).await.unwrap();
    world.svc.ack(tablet, chat, 3).await.unwrap();
    let state = world
        .svc
        .conversation_state(alice.user, chat)
        .await
        .unwrap();
    assert_eq!(member(&state, bob.user).delivered_seq, 3);
}

async fn read_implies_delivered_in_conversation_state<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    for body in ["one", "two"] {
        world.send(&alice, chat, body).await;
    }

    world.svc.read_up_to(bob.user, chat, 2).await.unwrap();
    let state = world
        .svc
        .conversation_state(alice.user, chat)
        .await
        .unwrap();
    assert_eq!(member(&state, bob.user).delivered_seq, 2);
    assert_eq!(member(&state, bob.user).read_seq, 2);
}

async fn delivered_survives_revoking_the_device_that_received_it<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let tablet = world.add_device(&bob).await;
    for body in ["one", "two", "three"] {
        world.send(&alice, chat, body).await;
    }

    world.svc.ack(tablet, chat, 3).await.unwrap();
    world.svc.revoke_device(tablet).await.unwrap();
    let state = world
        .svc
        .conversation_state(alice.user, chat)
        .await
        .unwrap();
    assert_eq!(member(&state, bob.user).delivered_seq, 3);
}

async fn only_members_can_read_or_see_state<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&alice, chat, "one").await;

    assert_eq!(
        world.svc.conversation_state(carol.user, chat).await,
        Err(DomainError::NotMember)
    );
    assert_eq!(
        world.svc.read_up_to(carol.user, chat, 1).await,
        Err(DomainError::NotMember)
    );
}

async fn a_negative_read_position_is_rejected<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&alice, chat, "one").await;

    assert_eq!(
        world.svc.read_up_to(bob.user, chat, -1).await,
        Err(DomainError::SeqOutOfRange)
    );
}

async fn sending_never_lowers_the_senders_read_position<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let alice_read = || async {
        let state = world
            .svc
            .conversation_state(alice.user, chat)
            .await
            .unwrap();
        member(&state, alice.user).read_seq
    };
    for body in ["one", "two", "three"] {
        world.send(&bob, chat, body).await;
    }

    world.svc.read_up_to(alice.user, chat, 3).await.unwrap();
    assert_eq!(alice_read().await, 3);
    world.send(&alice, chat, "four").await;
    assert_eq!(alice_read().await, 3);
    for body in ["five", "six", "seven", "eight", "nine"] {
        world.send(&bob, chat, body).await;
    }
    assert_eq!(alice_read().await, 3);
}

async fn a_groups_state_holds_only_group_wide_positions<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let group = world.group(&alice, &[&bob, &carol]).await;
    world.send(&alice, group, "one").await;
    world.svc.ack(bob.device, group, 2).await.unwrap();
    world.svc.ack(carol.device, group, 2).await.unwrap();
    world.svc.read_up_to(carol.user, group, 2).await.unwrap();

    let state = world.svc.conversation_state(bob.user, group).await.unwrap();
    assert_eq!(state.last_seq, 2);
    assert_eq!(
        state.positions,
        Positions::Group {
            all_delivered_seq: 2,
            all_read_seq: 1,
        }
    );
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
        the_read_position_only_moves_forward,
        reading_beyond_the_last_message_is_rejected,
        sending_marks_received_messages_read,
        sending_does_not_mark_unreceived_messages_read,
        sending_does_not_mark_anything_delivered,
        delivered_is_the_furthest_position_across_a_users_devices,
        read_implies_delivered_in_conversation_state,
        delivered_survives_revoking_the_device_that_received_it,
        only_members_can_read_or_see_state,
        a_negative_read_position_is_rejected,
        sending_never_lowers_the_senders_read_position,
        a_groups_state_holds_only_group_wide_positions,
    ]
}
