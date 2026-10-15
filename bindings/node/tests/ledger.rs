//! The secret store bridge's bookkeeping, without Node: the addon itself cannot be linked
//! into a test binary, so this includes the one module that does not need Node.

#![allow(dead_code)]

#[path = "../src/ledger.rs"]
mod ledger;

use std::sync::Arc;
use std::time::Duration;

use ledger::{Cache, ask, complete};

const SHORT: Duration = Duration::from_millis(50);

/// A call the store answers after `delay` (`error`: with a failure), from its own thread.
fn answering_after(
    delay: Duration,
    error: Option<&'static str>,
) -> impl FnOnce(u32) -> Result<(), String> {
    move |id| {
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            complete(id, error.map(str::to_owned), None);
        });
        Ok(())
    }
}

fn wait_for(cache: &Cache, key: &str, expected: Option<&[u8]>) {
    for _ in 0..200 {
        if cache.get(key).as_deref() == expected {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(cache.get(key).as_deref(), expected);
}

#[test]
fn a_write_confirmed_in_time_changes_the_cache() {
    let cache = Arc::new(Cache::default());
    let post = answering_after(Duration::ZERO, None);
    ask(
        &cache,
        Duration::from_secs(5),
        "save",
        "k",
        Some(b"v"),
        post,
    )
    .unwrap();
    assert_eq!(cache.get("k").as_deref(), Some(&b"v"[..]));
}

#[test]
fn a_late_confirmation_of_a_save_reaches_the_cache() {
    let cache = Arc::new(Cache::default());
    cache.preload("token", b"old".to_vec());
    let post = answering_after(Duration::from_millis(300), None);
    let error = ask(&cache, SHORT, "save", "token", Some(b"rotated"), post).unwrap_err();
    assert!(error.0.contains("did not answer"));
    assert_eq!(cache.get("token").as_deref(), Some(&b"old"[..]));
    wait_for(&cache, "token", Some(b"rotated"));
}

#[test]
fn a_late_confirmation_of_a_delete_reaches_the_cache() {
    let cache = Arc::new(Cache::default());
    cache.preload("token", b"old".to_vec());
    let post = answering_after(Duration::from_millis(300), None);
    ask(&cache, SHORT, "delete", "token", None, post).unwrap_err();
    wait_for(&cache, "token", None);
}

#[test]
fn a_late_failure_changes_nothing() {
    let cache = Arc::new(Cache::default());
    cache.preload("token", b"old".to_vec());
    let post = answering_after(Duration::from_millis(200), Some("the keychain is locked"));
    ask(&cache, SHORT, "save", "token", Some(b"rotated"), post).unwrap_err();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(cache.get("token").as_deref(), Some(&b"old"[..]));
}

#[test]
fn a_newer_write_wins_over_an_older_one_answered_late() {
    let cache = Arc::new(Cache::default());
    let older = answering_after(Duration::from_millis(300), None);
    ask(&cache, SHORT, "save", "token", Some(b"older"), older).unwrap_err();
    let newer = answering_after(Duration::ZERO, None);
    ask(
        &cache,
        Duration::from_secs(5),
        "save",
        "token",
        Some(b"newer"),
        newer,
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(cache.get("token").as_deref(), Some(&b"newer"[..]));
}

#[test]
fn a_load_never_changes_the_cache() {
    let cache = Arc::new(Cache::default());
    let post = answering_after(Duration::from_millis(200), None);
    ask(&cache, SHORT, "load", "token", None, post).unwrap_err();
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(cache.get("token"), None);
}
