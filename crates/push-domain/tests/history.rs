mod common;

use common::{Person, World, bodies, delete, seqs};
use push_domain::{ConversationId, DomainError, MAX_HISTORY_PAGE, MessageRef};

async fn chat_with_five_messages() -> (World, Person, Person, ConversationId) {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    for body in ["1", "2", "3", "4", "5"] {
        world.send(&alice, chat, body).await;
    }
    (world, alice, bob, chat)
}

#[tokio::test]
async fn the_first_page_is_the_newest_messages_oldest_first() {
    let (world, _alice, bob, chat) = chat_with_five_messages().await;
    let page = world.svc.history(bob.user, chat, None, 2).await.unwrap();
    assert_eq!(seqs(&page.entries), [4, 5]);
    assert!(page.has_more);
}

#[tokio::test]
async fn pages_go_backwards_from_before_seq() {
    let (world, _alice, bob, chat) = chat_with_five_messages().await;
    let history = |before| world.svc.history(bob.user, chat, Some(before), 2);

    let page = history(4).await.unwrap();
    assert_eq!(seqs(&page.entries), [2, 3]);
    assert!(page.has_more);
    let page = history(2).await.unwrap();
    assert_eq!(seqs(&page.entries), [1]);
    assert!(!page.has_more);
    assert!(history(1).await.unwrap().entries.is_empty());
}

#[tokio::test]
async fn a_page_that_reaches_the_start_has_no_more() {
    let (world, _alice, bob, chat) = chat_with_five_messages().await;
    let page = world.svc.history(bob.user, chat, Some(3), 2).await.unwrap();
    assert_eq!(seqs(&page.entries), [1, 2]);
    assert!(!page.has_more);
}

#[tokio::test]
async fn the_page_size_is_at_least_one() {
    let (world, _alice, bob, chat) = chat_with_five_messages().await;
    let page = world.svc.history(bob.user, chat, None, 0).await.unwrap();
    assert_eq!(seqs(&page.entries), [5]);
}

#[tokio::test]
async fn only_members_can_read_history() {
    let (world, _alice, _bob, chat) = chat_with_five_messages().await;
    let carol = world.person().await;
    assert_eq!(
        world.svc.history(carol.user, chat, None, 10).await,
        Err(DomainError::NotMember)
    );
}

#[tokio::test]
async fn a_before_seq_beyond_the_end_returns_the_newest_page() {
    let (world, _alice, bob, chat) = chat_with_five_messages().await;
    let page = world
        .svc
        .history(bob.user, chat, Some(99), 2)
        .await
        .unwrap();
    assert_eq!(seqs(&page.entries), [4, 5]);
}

#[tokio::test]
async fn a_before_seq_at_or_below_one_returns_nothing() {
    let (world, _alice, bob, chat) = chat_with_five_messages().await;
    for before in [1, 0, -3] {
        let page = world
            .svc
            .history(bob.user, chat, Some(before), 10)
            .await
            .unwrap();
        assert!(page.entries.is_empty());
        assert!(!page.has_more);
    }
}

#[tokio::test]
async fn the_page_size_is_capped() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    for _ in 0..=MAX_HISTORY_PAGE {
        world.send(&alice, chat, "x").await;
    }

    let page = world.svc.history(bob.user, chat, None, 1000).await.unwrap();
    assert_eq!(page.entries.len(), MAX_HISTORY_PAGE as usize);
    assert!(page.has_more);
}

#[tokio::test]
async fn history_holds_messages_in_their_current_state_and_no_changes() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let first = world.send(&alice, chat, "one").await;
    let second = world.send(&alice, chat, "two").await;
    world.edit(&alice, chat, &first, "one, edited").await;
    world
        .svc
        .delete_message(delete(&alice, chat, MessageRef::Id(second.message_id)))
        .await
        .unwrap();

    let page = world.svc.history(bob.user, chat, None, 10).await.unwrap();
    assert_eq!(seqs(&page.entries), [1, 2]);
    assert_eq!(bodies(&page.entries), ["one, edited", ""]);
    assert!(page.entries[1].deleted_at.is_some());
}

#[tokio::test]
async fn edits_do_not_take_room_in_a_page() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let first = world.send(&alice, chat, "one").await;
    let second = world.send(&alice, chat, "two").await;
    for body in ["a", "b", "c"] {
        world.edit(&alice, chat, &second, body).await;
    }

    let page = world.svc.history(bob.user, chat, None, 2).await.unwrap();
    assert_eq!(seqs(&page.entries), [first.seq, second.seq]);
    assert!(!page.has_more);
}
