use crate::{Backend, Case, Person, World, case_list};
use chrono::TimeDelta;
use push_domain::{
    Clock, ContactState, DomainError, Event, MAX_INVITE_LIFETIME_SECS, MAX_INVITE_USES, Store,
    UserId, ValidationError,
};

fn day() -> TimeDelta {
    TimeDelta::days(1)
}

fn accepted(user: &Person, peer: &Person) -> Event {
    Event::ContactChanged {
        user: user.user,
        peer: peer.user,
        state: Some(ContactState::Accepted),
    }
}

/// A new invite from `person`, valid for a day.
async fn invite<S: Store>(world: &World<S>, person: &Person, max_uses: u32) -> String {
    world
        .svc
        .create_invite(person.user, day(), max_uses)
        .await
        .unwrap()
        .code
}

async fn an_invite_has_a_ten_character_code_and_its_limits<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;

    let invite = world.svc.create_invite(alice.user, day(), 3).await.unwrap();
    assert_eq!(invite.code.len(), 10);
    assert_eq!(invite.expires_at, world.clock.now() + day());
    assert_eq!((invite.max_uses, invite.uses), (3, 0));
    let other = world.svc.create_invite(alice.user, day(), 3).await.unwrap();
    assert_ne!(other.code, invite.code);
}

async fn invites_outside_the_limits_are_refused<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
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

async fn an_unknown_user_cannot_create_an_invite<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    assert_eq!(
        world.svc.create_invite(UserId::new(), day(), 1).await,
        Err(DomainError::UnknownUser)
    );
}

async fn only_the_creator_can_revoke_and_revoking_twice_is_harmless<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
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

async fn redeeming_makes_both_users_contacts_and_tells_both<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    let code = invite(&world, &alice, 1).await;
    world.notifier.take();

    assert_eq!(
        world.svc.redeem_invite(bob.user, &code).await,
        Ok(alice.user)
    );
    assert_eq!(
        world.notifier.take(),
        [accepted(&alice, &bob), accepted(&bob, &alice)]
    );
    assert!(world.svc.open_direct(bob.user, alice.user).await.is_ok());
}

async fn a_typed_code_may_use_lower_case_dashes_and_look_alikes<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    let code = invite(&world, &alice, 1).await;
    let typed = format!("{}-{}", &code[..5], &code[5..])
        .to_lowercase()
        .replace('0', "o")
        .replace('1', "l");

    assert_eq!(
        world.svc.redeem_invite(bob.user, &typed).await,
        Ok(alice.user)
    );
}

async fn an_invite_can_be_used_only_max_uses_times<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob, carol, dave) = (
        world.person().await,
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let code = invite(&world, &alice, 2).await;

    assert_eq!(
        world.svc.redeem_invite(bob.user, &code).await,
        Ok(alice.user)
    );
    assert_eq!(
        world.svc.redeem_invite(carol.user, &code).await,
        Ok(alice.user)
    );
    assert_eq!(
        world.svc.redeem_invite(dave.user, &code).await,
        Err(DomainError::InviteNotFound)
    );
}

async fn an_invite_expires_at_its_expiry_time<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    let code = invite(&world, &alice, 1).await;

    world.clock.advance(day());
    assert_eq!(
        world.svc.redeem_invite(bob.user, &code).await,
        Err(DomainError::InviteNotFound)
    );
}

async fn a_revoked_or_unknown_invite_is_not_found<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    let code = invite(&world, &alice, 1).await;
    world.svc.revoke_invite(alice.user, &code).await.unwrap();

    for code in [code.as_str(), "0000000000", "not a code"] {
        assert_eq!(
            world.svc.redeem_invite(bob.user, code).await,
            Err(DomainError::InviteNotFound)
        );
    }
}

async fn redeeming_your_own_invite_is_invalid<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    let code = invite(&world, &alice, 1).await;

    assert_eq!(
        world.svc.redeem_invite(alice.user, &code).await,
        Err(DomainError::Invalid(ValidationError::SelfTarget))
    );
}

async fn a_block_by_the_creator_looks_like_an_unknown_invite_and_uses_nothing<B: Backend>(
    backend: &B,
) {
    let world = World::new(backend).await;
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let code = invite(&world, &alice, 1).await;
    world.svc.contact_block(alice.user, bob.user).await.unwrap();
    world.notifier.take();

    assert_eq!(
        world.svc.redeem_invite(bob.user, &code).await,
        Err(DomainError::InviteNotFound)
    );
    assert!(world.notifier.take().is_empty());
    assert_eq!(
        world.svc.redeem_invite(carol.user, &code).await,
        Ok(alice.user)
    );
}

async fn a_redeemer_who_blocked_the_creator_is_told_so<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    let code = invite(&world, &alice, 1).await;
    world.svc.contact_block(bob.user, alice.user).await.unwrap();

    assert_eq!(
        world.svc.redeem_invite(bob.user, &code).await,
        Err(DomainError::Blocked)
    );
}

async fn existing_contacts_redeem_without_using_the_invite<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    world.befriend(&alice, &bob).await;
    let code = invite(&world, &alice, 1).await;
    world.notifier.take();

    assert_eq!(
        world.svc.redeem_invite(bob.user, &code).await,
        Ok(alice.user)
    );
    assert!(world.notifier.take().is_empty());
    assert_eq!(
        world.svc.redeem_invite(carol.user, &code).await,
        Ok(alice.user)
    );
}

async fn redeeming_replaces_a_pending_request<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    world
        .svc
        .contact_request(bob.user, alice.user)
        .await
        .unwrap();
    let code = invite(&world, &alice, 1).await;
    world.notifier.take();

    world.svc.redeem_invite(bob.user, &code).await.unwrap();
    assert_eq!(
        world.notifier.take(),
        [accepted(&alice, &bob), accepted(&bob, &alice)]
    );
    for (me, peer) in [(&alice, &bob), (&bob, &alice)] {
        assert_eq!(
            world.svc.contact_state(me.user, peer.user).await,
            Ok(Some(ContactState::Accepted))
        );
    }
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
        an_invite_has_a_ten_character_code_and_its_limits,
        invites_outside_the_limits_are_refused,
        an_unknown_user_cannot_create_an_invite,
        only_the_creator_can_revoke_and_revoking_twice_is_harmless,
        redeeming_makes_both_users_contacts_and_tells_both,
        a_typed_code_may_use_lower_case_dashes_and_look_alikes,
        an_invite_can_be_used_only_max_uses_times,
        an_invite_expires_at_its_expiry_time,
        a_revoked_or_unknown_invite_is_not_found,
        redeeming_your_own_invite_is_invalid,
        a_block_by_the_creator_looks_like_an_unknown_invite_and_uses_nothing,
        a_redeemer_who_blocked_the_creator_is_told_so,
        existing_contacts_redeem_without_using_the_invite,
        redeeming_replaces_a_pending_request,
    ]
}
