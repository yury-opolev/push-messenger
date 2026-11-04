//! When calls are advertised (design A20): only while a voice adapter is ready and can
//! listen (`stt`). push-sdk fixes the calls capability per connection, so a change
//! reconnects ([`push_sdk::Client::set_calls_advertised`]); it therefore never happens
//! while a call rings, is answered, asked about, placed or runs: it waits for the call's
//! end. Flapping is damped: the adapter must stay ready [`READY_FOR`] before calls are
//! advertised; they are withdrawn at once (outside a call) when it is not ready.
//!
//! [`advertise`] runs that over a `watch` of the adapter's state and one of "a call is in
//! progress" (the core's), calling a setter, so it is tested without the SDK.

use std::time::Duration;

use tokio::sync::watch;
use tokio::time::{Instant, sleep_until};

use super::supervisor::AdapterState;

/// How long an adapter must stay ready before calls are advertised.
pub const READY_FOR: Duration = Duration::from_secs(2);

/// Whether `state` can take calls: ready with `stt`.
pub fn listens(state: &AdapterState) -> bool {
    state.ready().is_some_and(|info| info.capabilities.stt)
}

/// Advertises calls through `set` (starting from not advertised) while `adapter` listens
/// (see the module's documentation); `in_call` is `true` while a call is in progress.
/// Returns once the adapter's state channel closed and calls are withdrawn.
pub async fn advertise(
    mut adapter: watch::Receiver<AdapterState>,
    mut in_call: watch::Receiver<bool>,
    set: impl Fn(bool) + Send,
) {
    let mut advertised = false;
    let mut ready_since = listens(&adapter.borrow_and_update()).then(Instant::now);
    let (mut adapter_open, mut call_open) = (true, true);
    loop {
        let busy = call_open && *in_call.borrow_and_update();
        let mut wake = None;
        if !busy {
            match ready_since {
                Some(since) if !advertised => {
                    let due = since + READY_FOR;
                    if Instant::now() >= due {
                        advertised = true;
                        tracing::info!(event = "calls_advertised", on = true);
                        set(true);
                    } else {
                        wake = Some(due);
                    }
                }
                None if advertised => {
                    advertised = false;
                    tracing::info!(event = "calls_advertised", on = false);
                    set(false);
                }
                _ => {}
            }
        }
        if !adapter_open && !advertised {
            return;
        }
        tokio::select! {
            changed = adapter.changed(), if adapter_open => match changed {
                Ok(()) => {
                    let ready = listens(&adapter.borrow_and_update());
                    match (ready, ready_since) {
                        (true, None) => ready_since = Some(Instant::now()),
                        (false, _) => ready_since = None,
                        (true, Some(_)) => {}
                    }
                }
                Err(_) => {
                    adapter_open = false;
                    ready_since = None;
                }
            },
            changed = in_call.changed(), if call_open => {
                if changed.is_err() {
                    call_open = false;
                }
            }
            () = sleep_until(wake.unwrap_or_else(Instant::now)), if wake.is_some() => {}
            else => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use push_voice_adapter_proto::Capabilities;
    use tokio::sync::mpsc;
    use tokio::time::sleep;

    use super::*;
    use crate::voice::adapter::link::AdapterInfo;

    fn ready(stt: bool) -> AdapterState {
        AdapterState::Ready(AdapterInfo {
            protocol: "push-voice-adapter/1".into(),
            name: "testkit".into(),
            version: "1.2.3".into(),
            capabilities: Capabilities {
                stt,
                tts: true,
                ..Capabilities::default()
            },
            voices: Vec::new(),
        })
    }

    fn backoff() -> AdapterState {
        AdapterState::Backoff {
            until: Instant::now() + Duration::from_secs(1),
            reason: "crashed".into(),
        }
    }

    struct Advertiser {
        adapter: watch::Sender<AdapterState>,
        in_call: watch::Sender<bool>,
        changes: mpsc::UnboundedReceiver<(Instant, bool)>,
        started: Instant,
        on: Arc<Mutex<bool>>,
    }

    impl Advertiser {
        fn start(state: AdapterState) -> Self {
            let (adapter, adapter_rx) = watch::channel(state);
            let (in_call, in_call_rx) = watch::channel(false);
            let (sender, changes) = mpsc::unbounded_channel();
            let on = Arc::new(Mutex::new(false));
            let shown = on.clone();
            tokio::spawn(advertise(adapter_rx, in_call_rx, move |value| {
                *shown.lock().unwrap() = value;
                let _ = sender.send((Instant::now(), value));
            }));
            Self {
                adapter,
                in_call,
                changes,
                started: Instant::now(),
                on,
            }
        }

        /// The changes within `time`, with when (since the start).
        async fn during(&mut self, time: Duration) -> Vec<(Duration, bool)> {
            sleep(time).await;
            let mut seen = Vec::new();
            while let Ok((at, on)) = self.changes.try_recv() {
                seen.push((at - self.started, on));
            }
            seen
        }

        fn on(&self) -> bool {
            *self.on.lock().unwrap()
        }
    }

    const S: fn(u64) -> Duration = Duration::from_secs;
    const MS: fn(u64) -> Duration = Duration::from_millis;

    #[tokio::test(start_paused = true)]
    async fn a_listening_adapter_ready_for_2_s_is_advertised() {
        let mut a = Advertiser::start(AdapterState::Starting);
        sleep(MS(500)).await;
        a.adapter.send_replace(ready(true));
        assert_eq!(a.during(S(5)).await, [(MS(2500), true)]);
    }

    #[tokio::test(start_paused = true)]
    async fn an_adapter_ready_at_the_start_is_advertised_after_2_s() {
        let mut a = Advertiser::start(ready(true));
        assert_eq!(a.during(S(3)).await, [(S(2), true)]);
    }

    #[tokio::test(start_paused = true)]
    async fn an_adapter_that_cannot_listen_is_not_advertised() {
        let mut a = Advertiser::start(ready(false));
        assert_eq!(a.during(S(10)).await, []);
    }

    #[tokio::test(start_paused = true)]
    async fn losing_the_adapter_while_idle_withdraws_at_once() {
        let mut a = Advertiser::start(ready(true));
        assert_eq!(a.during(S(3)).await, [(S(2), true)]);
        a.adapter.send_replace(backoff());
        assert_eq!(a.during(MS(1)).await, [(S(3), false)]);
        assert!(!a.on());
    }

    #[tokio::test(start_paused = true)]
    async fn losing_the_adapter_during_a_call_withdraws_when_the_call_ends() {
        let mut a = Advertiser::start(ready(true));
        assert_eq!(a.during(S(3)).await, [(S(2), true)]);
        a.in_call.send_replace(true);
        sleep(S(1)).await;
        a.adapter.send_replace(backoff());
        sleep(S(1)).await;
        a.adapter.send_replace(AdapterState::Starting);
        assert_eq!(a.during(S(10)).await, [], "never during the call");
        assert!(a.on());
        a.in_call.send_replace(false);
        assert_eq!(a.during(MS(1)).await, [(S(15), false)]);
    }

    #[tokio::test(start_paused = true)]
    async fn an_adapter_ready_during_a_call_is_advertised_after_the_call() {
        let mut a = Advertiser::start(AdapterState::Starting);
        a.in_call.send_replace(true);
        a.adapter.send_replace(ready(true));
        assert_eq!(a.during(S(5)).await, []);
        a.in_call.send_replace(false);
        assert_eq!(a.during(MS(1)).await, [(S(5), true)]);
    }

    #[tokio::test(start_paused = true)]
    async fn flapping_under_2_s_is_never_advertised() {
        let mut a = Advertiser::start(AdapterState::Starting);
        for _ in 0..5 {
            a.adapter.send_replace(ready(true));
            sleep(MS(1500)).await;
            a.adapter.send_replace(backoff());
            sleep(MS(500)).await;
        }
        assert_eq!(a.during(S(1)).await, []);
        a.adapter.send_replace(ready(true));
        assert_eq!(a.during(S(3)).await, [(S(13), true)]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_hello_while_ready_keeps_the_advertisement() {
        let mut a = Advertiser::start(ready(true));
        assert_eq!(a.during(S(3)).await, [(S(2), true)]);
        a.adapter.send_replace(ready(true));
        a.adapter.send_replace(ready(false));
        assert_eq!(a.during(MS(1)).await, [(S(3), false)], "no stt: withdrawn");
    }

    #[tokio::test(start_paused = true)]
    async fn a_closed_supervisor_withdraws_and_the_task_ends() {
        let (adapter, adapter_rx) = watch::channel(ready(true));
        let (in_call, in_call_rx) = watch::channel(false);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        let task = tokio::spawn(advertise(adapter_rx, in_call_rx, move |on| {
            record.lock().unwrap().push(on);
        }));
        sleep(S(3)).await;
        in_call.send_replace(true);
        drop(adapter);
        sleep(S(1)).await;
        assert_eq!(*seen.lock().unwrap(), [true], "not during the call");
        drop(in_call);
        tokio::time::timeout(S(1), task).await.unwrap().unwrap();
        assert_eq!(*seen.lock().unwrap(), [true, false]);
    }
}
