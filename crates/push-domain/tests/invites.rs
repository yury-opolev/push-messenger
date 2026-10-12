mod common;

use chrono::TimeDelta;
use common::World;
use push_domain::{
    Clock, DomainError, MAX_INVITE_LIFETIME_SECS, MAX_INVITE_USES, UserId, ValidationError,
};

fn day() -> TimeDelta {
    TimeDelta::days(1)
}

#[tokio::test]
async fn an_invite_has_a_ten_character_code_and_its_limits() {
    let world = World::new();
    let alice = world.person().await;

    let invite = world.svc.create_invite(alice.user, day(), 3).await.unwrap();
    assert_eq!(invite.code.len(), 10);
    assert_eq!(invite.expires_at, world.clock.now() + day());
    assert_eq!((invite.max_uses, invite.uses), (3, 0));
    let other = world.svc.create_invite(alice.user, day(), 3).await.unwrap();
    assert_ne!(other.code, invite.code);
}

#[tokio::test]
async fn invites_outside_the_limits_are_refused() {
    let world = World::new();
    let alice = world.person().await;

    assert_eq!(
        world
            .svc
            .create_invite(alice.user, day(), MAX_INVITE_USES + 1)
            .await,
        Err(DomainError::Invalid(ValidationError::InviteUses {
            actual: MAX_INVITE_USES + 1,
            limit: MAX_INVITE_USES,
        }))
    );
    let too_long = TimeDelta::seconds(MAX_INVITE_LIFETIME_SECS + 1);
    assert!(matches!(
        world.svc.create_invite(alice.user, too_long, 1).await,
        Err(DomainError::Invalid(ValidationError::InviteLifetime { .. }))
    ));
}

#[tokio::test]
async fn an_unknown_user_cannot_create_an_invite() {
    let world = World::new();
    assert_eq!(
        world.svc.create_invite(UserId::new(), day(), 1).await,
        Err(DomainError::UnknownUser)
    );
}

#[tokio::test]
async fn only_the_creator_can_revoke_and_revoking_twice_is_harmless() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let invite = world.svc.create_invite(alice.user, day(), 1).await.unwrap();

    assert_eq!(
        world.svc.revoke_invite(bob.user, &invite.code).await,
        Err(DomainError::InviteNotFound)
    );
    assert_eq!(
        world.svc.revoke_invite(alice.user, &invite.code).await,
        Ok(())
    );
    assert_eq!(
        world.svc.revoke_invite(alice.user, &invite.code).await,
        Ok(())
    );
    assert_eq!(
        world.svc.revoke_invite(alice.user, "0000000000").await,
        Err(DomainError::InviteNotFound)
    );
}
