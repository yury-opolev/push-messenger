use chrono::TimeDelta;
use push_domain::{Clock, DeviceId, DomainError, Event, KickReason, UserId};

use crate::{Backend, Case, World, bodies, case_list};

async fn registering_a_device_for_an_unknown_user_fails<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let result = world
        .svc
        .register_device(DeviceId::new(), UserId::new())
        .await;
    assert_eq!(result, Err(DomainError::UnknownUser));
}

async fn identify_accepts_the_users_own_active_device<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    assert_eq!(world.identify(alice.device, alice.user).await, Ok(()));
}

async fn identify_rejects_a_device_of_another_user<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    let bob = world.person().await;
    assert_eq!(
        world.identify(bob.device, alice.user).await,
        Err(DomainError::UnknownDevice)
    );
}

async fn identify_rejects_a_revoked_device<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    world.svc.revoke_device(alice.device).await.unwrap();
    assert_eq!(
        world.identify(alice.device, alice.user).await,
        Err(DomainError::UnknownDevice)
    );
}

async fn identify_asks_to_retry_a_device_not_relayed_yet<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (user, device) = (UserId::new(), DeviceId::new());
    let refused = world.identify(device, user).await.unwrap_err();
    assert_eq!(refused, DomainError::DeviceNotKnownYet);
    assert!(refused.is_retryable());
    world.svc.device_registered(device, user).await.unwrap();
    assert_eq!(world.identify(device, user).await, Ok(()));
}

async fn a_device_id_of_another_user_cannot_be_registered<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    let bob = world.person().await;
    assert_eq!(
        world.svc.register_device(alice.device, bob.user).await,
        Err(DomainError::UnknownDevice)
    );
}

async fn a_revoked_device_cannot_be_registered_again<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    world.svc.revoke_device(alice.device).await.unwrap();
    assert_eq!(
        world.svc.register_device(alice.device, alice.user).await,
        Err(DomainError::UnknownDevice)
    );
}

async fn registering_the_same_user_and_device_twice_is_harmless<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    world.svc.register_user(alice.user).await.unwrap();
    world
        .svc
        .register_device(alice.device, alice.user)
        .await
        .unwrap();
    assert_eq!(world.identify(alice.device, alice.user).await, Ok(()));
}

async fn a_relayed_registration_records_the_user_too<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (user, device) = (UserId::new(), DeviceId::new());
    world.svc.device_registered(device, user).await.unwrap();
    world.svc.device_registered(device, user).await.unwrap();
    world.svc.register_user(user).await.unwrap();
    assert_eq!(world.identify(device, user).await, Ok(()));
}

async fn a_relayed_registration_starts_up_to_date<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&alice, chat, "one").await;

    let tablet = DeviceId::new();
    world.svc.device_registered(tablet, bob.user).await.unwrap();
    assert!(
        world
            .svc
            .sync(tablet, 100)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    world.send(&alice, chat, "two").await;
    let batch = world.svc.sync(tablet, 100).await.unwrap();
    assert_eq!(bodies(&batch.entries), ["two"]);
}

async fn a_revocation_relayed_before_the_registration_wins<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (user, device) = (UserId::new(), DeviceId::new());
    world.svc.device_revoked(device, user).await.unwrap();
    world.svc.device_registered(device, user).await.unwrap();
    world.svc.device_revoked(device, user).await.unwrap();
    assert_eq!(
        world.identify(device, user).await,
        Err(DomainError::UnknownDevice)
    );
}

async fn a_relayed_revocation_closes_an_active_device<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    let tablet = world.add_device(&alice).await;
    world.svc.device_revoked(tablet, alice.user).await.unwrap();
    assert_eq!(
        world.identify(tablet, alice.user).await,
        Err(DomainError::UnknownDevice)
    );
    assert_eq!(world.identify(alice.device, alice.user).await, Ok(()));
}

async fn relayed_events_for_another_users_device_are_refused<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    assert_eq!(
        world.svc.device_registered(alice.device, bob.user).await,
        Err(DomainError::UnknownDevice)
    );
    assert_eq!(
        world.svc.device_revoked(alice.device, bob.user).await,
        Err(DomainError::UnknownDevice)
    );
    assert_eq!(world.identify(alice.device, alice.user).await, Ok(()));
}

async fn a_relayed_revocation_kicks_the_device<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    let tablet = world.add_device(&alice).await;
    world.notifier.take();
    world.svc.device_revoked(tablet, alice.user).await.unwrap();
    // Every copy of the event kicks again: harmless, and nothing is lost if the first
    // kick was.
    world.svc.device_revoked(tablet, alice.user).await.unwrap();
    let kick = Event::Kick {
        devices: vec![tablet],
        reason: KickReason::DeviceRevoked,
    };
    assert_eq!(world.notifier.take(), vec![kick.clone(), kick]);

    // A refused revocation kicks nobody.
    let bob = world.person().await;
    world.notifier.take();
    assert!(
        world
            .svc
            .device_revoked(bob.device, alice.user)
            .await
            .is_err()
    );
    assert_eq!(world.notifier.take(), []);
}

async fn a_failed_kick_fails_the_relay_so_it_is_retried<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    let tablet = world.add_device(&alice).await;
    world.notifier.take();
    world.notifier.fail_kicks(true);

    let revoked = world.svc.device_revoked(tablet, alice.user).await;
    assert!(matches!(revoked, Err(DomainError::KickFailed(_))));
    assert!(revoked.unwrap_err().is_retryable());
    let reset_at = world.clock.now();
    let reset = world.svc.password_reset(alice.user, Some(reset_at)).await;
    assert!(matches!(reset, Err(DomainError::KickFailed(_))));
    // The changes were stored even though their kicks were lost.
    assert_eq!(
        world.identify(tablet, alice.user).await,
        Err(DomainError::UnknownDevice)
    );
    assert_eq!(
        world.svc.identify(alice.device, alice.user, reset_at).await,
        Err(DomainError::PasswordChanged)
    );

    // The retries kick.
    world.notifier.fail_kicks(false);
    world.svc.device_revoked(tablet, alice.user).await.unwrap();
    world
        .svc
        .password_reset(alice.user, Some(reset_at))
        .await
        .unwrap();
    assert_eq!(
        world.notifier.take(),
        [
            Event::Kick {
                devices: vec![tablet],
                reason: KickReason::DeviceRevoked,
            },
            Event::Kick {
                devices: vec![alice.device],
                reason: KickReason::PasswordReset { reset_at },
            },
        ]
    );
}

async fn a_password_reset_kicks_every_active_device_of_the_user<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    let laptop = world.add_device(&alice).await;
    let revoked = world.add_device(&alice).await;
    world.svc.device_revoked(revoked, alice.user).await.unwrap();
    world.notifier.take();

    let reset_at = world.clock.now();
    world
        .svc
        .password_reset(alice.user, Some(reset_at))
        .await
        .unwrap();
    let mut devices = vec![alice.device, laptop];
    devices.sort();
    assert_eq!(
        world.notifier.take(),
        [Event::Kick {
            devices,
            reason: KickReason::PasswordReset { reset_at },
        }]
    );
    // The devices stay registered: a new sign-in on them works.
    world.clock.advance(TimeDelta::seconds(1));
    assert_eq!(world.identify(laptop, alice.user).await, Ok(()));
    assert_eq!(world.identify(bob.device, bob.user).await, Ok(()));

    // A user message-svc has not heard of has nothing to kick.
    world
        .svc
        .password_reset(UserId::new(), Some(reset_at))
        .await
        .unwrap();
    assert_eq!(world.notifier.take(), []);
}

/// A token that outlives the reset (up to 30 minutes), on a device whose kick was lost,
/// cannot sign in again. The reset's time is auth-svc's: a token issued after it passes
/// even when the worker relays the reset later.
async fn identify_refuses_tokens_issued_until_the_password_reset<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    // auth-svc reset the password half a minute before the worker relayed it.
    let reset_at = world.clock.now() - TimeDelta::seconds(30);
    world
        .svc
        .password_reset(alice.user, Some(reset_at))
        .await
        .unwrap();
    let before = reset_at - TimeDelta::minutes(10);
    let refused = world.svc.identify(alice.device, alice.user, before).await;
    assert_eq!(refused, Err(DomainError::PasswordChanged));
    assert!(!DomainError::PasswordChanged.is_retryable());
    // Within the second of the reset the order is unknown: refused too.
    assert_eq!(
        world.svc.identify(alice.device, alice.user, reset_at).await,
        Err(DomainError::PasswordChanged)
    );
    // Signed in after the reset, before the relay.
    let after = reset_at + TimeDelta::seconds(1);
    assert_eq!(
        world.svc.identify(alice.device, alice.user, after).await,
        Ok(())
    );
    // Other users are not affected.
    assert_eq!(
        world.svc.identify(bob.device, bob.user, before).await,
        Ok(())
    );
}

/// The reset can be relayed before the user's device. A redelivered or an older reset
/// moves nothing, so a session that signed in after the latest reset stays; a newer one
/// ends it. An event without a time takes message-svc's clock.
async fn a_redelivered_or_older_reset_signs_out_no_newer_session<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (user, device) = (UserId::new(), DeviceId::new());
    let reset_at = world.clock.now();
    world
        .svc
        .password_reset(user, Some(reset_at))
        .await
        .unwrap();
    world.svc.device_registered(device, user).await.unwrap();
    assert_eq!(
        world.svc.identify(device, user, reset_at).await,
        Err(DomainError::PasswordChanged)
    );
    world.clock.advance(TimeDelta::minutes(1));
    let signed_in = world.clock.now();
    assert_eq!(world.identify(device, user).await, Ok(()));

    // At-least-once: the same event again, a minute later; then an older reset.
    world.clock.advance(TimeDelta::minutes(1));
    world.notifier.take();
    world
        .svc
        .password_reset(user, Some(reset_at))
        .await
        .unwrap();
    let older = reset_at - TimeDelta::hours(1);
    world.svc.password_reset(user, Some(older)).await.unwrap();
    assert_eq!(world.svc.identify(device, user, signed_in).await, Ok(()));
    // The kicks carry the latest reset, which the newer session postdates.
    let kick = Event::Kick {
        devices: vec![device],
        reason: KickReason::PasswordReset { reset_at },
    };
    assert_eq!(world.notifier.take(), [kick.clone(), kick]);

    // A newer reset ends it.
    let newer = signed_in + TimeDelta::seconds(30);
    world.svc.password_reset(user, Some(newer)).await.unwrap();
    assert_eq!(
        world.svc.identify(device, user, signed_in).await,
        Err(DomainError::PasswordChanged)
    );

    // Without a time, now.
    world.clock.advance(TimeDelta::minutes(1));
    world.svc.password_reset(user, None).await.unwrap();
    assert_eq!(
        world.identify(device, user).await,
        Err(DomainError::PasswordChanged)
    );
    world.clock.advance(TimeDelta::seconds(1));
    assert_eq!(world.identify(device, user).await, Ok(()));
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
        registering_a_device_for_an_unknown_user_fails,
        identify_accepts_the_users_own_active_device,
        identify_rejects_a_device_of_another_user,
        identify_rejects_a_revoked_device,
        identify_asks_to_retry_a_device_not_relayed_yet,
        a_device_id_of_another_user_cannot_be_registered,
        a_revoked_device_cannot_be_registered_again,
        registering_the_same_user_and_device_twice_is_harmless,
        a_relayed_registration_records_the_user_too,
        a_relayed_registration_starts_up_to_date,
        a_revocation_relayed_before_the_registration_wins,
        a_relayed_revocation_closes_an_active_device,
        relayed_events_for_another_users_device_are_refused,
        a_relayed_revocation_kicks_the_device,
        a_failed_kick_fails_the_relay_so_it_is_retried,
        a_password_reset_kicks_every_active_device_of_the_user,
        identify_refuses_tokens_issued_until_the_password_reset,
        a_redelivered_or_older_reset_signs_out_no_newer_session,
    ]
}
