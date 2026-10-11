mod common;

use common::{World, seqs};
use push_domain::DomainError;

async fn chat_with_five_messages() -> (World, common::Person, push_domain::ConversationId) {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    for body in ["1", "2", "3", "4", "5"] {
        world.send(&alice, chat, body).await;
    }
    (world, bob, chat)
}

#[tokio::test]
async fn the_first_page_is_the_newest_messages_oldest_first() {
    let (world, bob, chat) = chat_with_five_messages().await;
    let page = world.svc.history(bob.user, chat, None, 2).await.unwrap();
    assert_eq!(seqs(&page), [4, 5]);
}

#[tokio::test]
async fn pages_go_backwards_from_before_seq() {
    let (world, bob, chat) = chat_with_five_messages().await;
    let history = |before| world.svc.history(bob.user, chat, Some(before), 2);

    assert_eq!(seqs(&history(4).await.unwrap()), [2, 3]);
    assert_eq!(seqs(&history(2).await.unwrap()), [1]);
    assert!(history(1).await.unwrap().is_empty());
}

#[tokio::test]
async fn the_page_size_is_at_least_one() {
    let (world, bob, chat) = chat_with_five_messages().await;
    let page = world.svc.history(bob.user, chat, None, 0).await.unwrap();
    assert_eq!(seqs(&page), [5]);
}

#[tokio::test]
async fn only_members_can_read_history() {
    let (world, _bob, chat) = chat_with_five_messages().await;
    let carol = world.person().await;
    assert_eq!(
        world.svc.history(carol.user, chat, None, 10).await,
        Err(DomainError::NotMember)
    );
}
