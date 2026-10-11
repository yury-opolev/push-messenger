mod common;

use common::World;
use push_domain::{ConversationState, DomainError, MemberState, UserId};

fn member(state: &ConversationState, user: UserId) -> &MemberState {
    state
        .members
        .iter()
        .find(|member| member.user_id == user)
        .expect("member present")
}

#[tokio::test]
async fn the_read_position_only_moves_forward() {
    let world = World::new();
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

#[tokio::test]
async fn reading_beyond_the_last_message_is_rejected() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&alice, chat, "one").await;

    assert_eq!(
        world.svc.read_up_to(bob.user, chat, 9).await,
        Err(DomainError::SeqOutOfRange)
    );
}

#[tokio::test]
async fn sending_marks_the_conversation_read_for_the_sender() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&bob, chat, "one").await;
    world.send(&alice, chat, "two").await;

    let state = world
        .svc
        .conversation_state(alice.user, chat)
        .await
        .unwrap();
    assert_eq!(state.last_seq, 2);
    assert_eq!(member(&state, alice.user).read_seq, 2);
    assert_eq!(member(&state, bob.user).read_seq, 1);
}

#[tokio::test]
async fn sending_does_not_mark_anything_delivered() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&bob, chat, "one").await;
    world.send(&alice, chat, "two").await;

    let state = world.svc.conversation_state(bob.user, chat).await.unwrap();
    assert_eq!(member(&state, alice.user).delivered_seq, 0);
    assert_eq!(member(&state, bob.user).delivered_seq, 0);
}

#[tokio::test]
async fn delivered_is_the_furthest_position_across_a_users_devices() {
    let world = World::new();
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

#[tokio::test]
async fn only_members_can_read_or_see_state() {
    let world = World::new();
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
